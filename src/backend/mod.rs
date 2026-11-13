//! Pluggable storage backend abstraction.
//!
//! The filesystem never touches the network or the disk directly — it goes
//! through this trait. Today we ship [`local::LocalBackend`] (pread/pwrite on
//! the local disk). Later we can drop in a `GrpcBackend` that streams to the
//! central cluster **without changing any of the FUSE or metadata code**. This
//! is the seam that lets us fully decouple from NFS.
//!
//! Object identity model:
//! - Each logical file maps to an opaque `key` (a uuid stored in the metadata
//!   `node.data_ref`). The backend stores bytes per key.
//! - Keys are content-addressed later for dedup if desired; for now they are
//!   stable object ids that support random read/write (needed for POSIX write).

#[cfg(feature = "native")]
pub mod local;
#[cfg(feature = "native")]
pub mod placement;
#[cfg(feature = "native")]
pub mod sharded;

use std::io::{self, Read};
use std::path::Path;

/// How [`StorageBackend::import_at`] materialized an external file as an object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Imported {
    /// The object shares the source file's inode (same filesystem): zero
    /// extra bytes, and edits through either path are visible in the other.
    Hardlinked,
    /// The bytes were streamed into a fresh, independent object.
    Copied,
}

/// One stored object as an enumeration sees it.
///
/// `mtime_secs` is part of the contract, not a convenience: `gc`'s grace window
/// refuses to delete an object younger than `--older-than`, and that verdict has
/// to come from the machine HOLDING the bytes (a clock or a mount elsewhere can
/// be wrong by exactly the amount that matters). 0 means "this backend cannot
/// say" and the auditor treats the object as young — the safe direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectRow {
    pub key: String,
    pub size: u64,
    pub mtime_secs: i64,
}

/// One page of [`StorageBackend::list_objects`].
#[derive(Debug, Clone, Default)]
pub struct ObjectPage {
    pub rows: Vec<ObjectRow>,
    /// Opaque continuation: hand it back verbatim. `None` means the enumeration
    /// is COMPLETE — never "start over", which would turn a finished walk into
    /// an infinite one for a caller that simply loops while a cursor exists.
    pub next: Option<String>,
}

/// Largest page a backend will assemble for one enumeration round trip. The
/// wire response carries every row, so this is bounded by [`crate::remote::proto`]'s
/// frame cap rather than by memory: 8192 rows of a ~70-byte key is well under
/// 1 MiB, and a caller asking for more is clamped instead of refused (a bigger
/// page is a performance wish, not a different operation).
pub const LIST_LIMIT_MAX: usize = 8192;

/// Shared page walker for a backend that is a LIST of members — a node's disks
/// ([`placement`](crate::backend::placement)) or a striped layout's data servers
/// ([`sharded`](crate::backend::sharded)).
///
/// Cursor = `"<member index>:<member cursor>"`. Members are walked in order, so
/// one page can span several members without any cluster-wide index; a member
/// with nothing left walks into the next one, which is what keeps an empty
/// member from ending the walk early. The cursor stays opaque to the caller
/// and this function is the only place that gives it meaning, so both
/// decorators share exactly one traversal rule instead of two that could drift.
pub(crate) fn list_across_members<F>(
    count: usize,
    member: F,
    cursor: Option<&str>,
    limit: usize,
) -> io::Result<ObjectPage>
where
    F: Fn(usize) -> std::sync::Arc<dyn StorageBackend>,
{
    let bad = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "list_objects: malformed cursor",
        )
    };
    let (mut idx, mut inner) = match cursor {
        None => (0usize, None),
        Some(c) => {
            let (i, rest) = c.split_once(':').ok_or_else(bad)?;
            let i: usize = i.parse().map_err(|_| bad())?;
            if i >= count {
                return Err(bad());
            }
            (i, if rest.is_empty() { None } else { Some(rest.to_string()) })
        }
    };
    let limit = limit.clamp(1, LIST_LIMIT_MAX);
    let mut page = ObjectPage::default();
    let mut budget = limit;
    while idx < count {
        let part = member(idx).list_objects(inner.as_deref(), budget)?;
        page.rows.extend(part.rows);
        match part.next {
            Some(n) => {
                page.next = Some(format!("{idx}:{n}"));
                return Ok(page);
            }
            None => {
                idx += 1;
                inner = None;
                let left = limit.saturating_sub(page.rows.len());
                if left == 0 && idx < count {
                    // A member ended exactly on the page boundary: continue from
                    // the NEXT member in a new page rather than overrunning
                    // `limit` to finish the walk in one go.
                    page.next = Some(format!("{idx}:"));
                    return Ok(page);
                }
                budget = left.max(1);
            }
        }
    }
    Ok(page)
}

/// Filesystem-level capacity stats surfaced through FUSE `statfs` (i.e. `df`).
#[derive(Debug, Clone, Copy)]
pub struct FsStat {
    pub tblocks: u64, // total blocks
    pub bfree: u64,   // free blocks
    pub bavail: u64,  // blocks available to unprivileged users
    pub files: u64,   // total inodes
    pub ffree: u64,   // free inodes
    pub bsize: u64,   // optimal transfer block size
    pub namemax: u32, // max filename length
}

/// Minimal, POSIX-oriented blob/object interface the rest of the system relies on.
/// Implementations must be `Send + Sync` because fuser serves requests on a
/// thread pool.
pub trait StorageBackend: Send + Sync {
    /// Decide the durable identifier to store for a freshly generated uuid
    /// base. Backends that keep one object per file return `uuid` verbatim
    /// (the default), so an unstriped view's on-disk keys are the uuids. A
    /// striping backend instead encodes its layout (server count + stripe size)
    /// into the returned key so later reads are self-describing and an
    /// unprefixed key still resolves through the single-object path. The result
    /// is what lands in `node.data_ref` and is passed back to every other
    /// method, so a backend must accept any key it can produce here.
    fn new_key(&self, uuid: &str) -> String {
        uuid.to_string()
    }

    /// Ensure an object exists (empty). Idempotent.
    fn create(&self, key: &str) -> io::Result<()>;

    /// Positional read: read up to `len` bytes at `offset`.
    /// Returns fewer bytes at EOF. This maps directly to FUSE `read`.
    fn read(&self, key: &str, offset: u64, len: usize) -> io::Result<Vec<u8>>;

    /// Positional write: write `data` at `offset`. Returns bytes written.
    /// Maps directly to FUSE `write`. Extends the object when writing past EOF.
    fn write(&self, key: &str, offset: u64, data: &[u8]) -> io::Result<usize>;

    /// Current logical size of the object (used to answer getattr size).
    fn len(&self, key: &str) -> io::Result<u64>;

    /// Flush durable state for the object (maps to FUSE `fsync`).
    fn fsync(&self, key: &str) -> io::Result<()>;

    /// Truncate the object to `new_len` (maps to FUSE `truncate` / O_TRUNC).
    fn truncate(&self, key: &str, new_len: u64) -> io::Result<()>;

    /// Delete the object. Called by GC when the last reference goes away.
    fn remove(&self, key: &str) -> io::Result<()>;

    /// Enumerate stored objects in pages — the data plane's half of the offline
    /// `gc` audit.
    ///
    /// The cursor is opaque to the caller and only ever compared back with the
    /// same backend, so each implementation encodes its own progress (a bucket
    /// index on a local root, a member index on a multi-disk or sharded node)
    /// without the auditor knowing the layout. The ORDER is likewise the
    /// backend's own and only has to be stable for a fixed set of objects,
    /// because the one guarantee the caller needs is "each object appears
    /// exactly once in a full walk".
    ///
    /// The default REFUSES instead of returning an empty page: an enumeration
    /// that silently finds nothing would make `gc` report a clean bill of
    /// health about bytes it never looked at, which is the one outcome an
    /// offline deleter must never fake.
    fn list_objects(&self, _cursor: Option<&str>, _limit: usize) -> io::Result<ObjectPage> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this backend cannot enumerate stored objects",
        ))
    }

    /// Register an EXISTING local file as the object under `key` — the data
    /// plane's half of `grovefs import`. Implementations should avoid copying
    /// bytes when storage can be shared with the source (a hard link on the
    /// same filesystem); the default streams the file into a fresh object.
    /// The metadata side (`node.data_ref = key`) is the caller's job.
    fn import_at(&self, key: &str, src: &Path) -> io::Result<Imported> {
        let mut file = std::fs::File::open(src)?;
        self.create(key)?;
        let mut buf = vec![0u8; 1 << 20];
        let mut off = 0u64;
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            self.write(key, off, &buf[..n])?;
            off += n as u64;
        }
        Ok(Imported::Copied)
    }

    /// Capacity of the underlying store, surfaced by `df`/`statvfs`.
    fn statvfs(&self) -> io::Result<FsStat>;
}

/// Factory used at startup to pick a backend from CLI options. Keeps the
/// `Filesystem` code backend-agnostic (it only ever sees `Box<dyn StorageBackend>`).
#[cfg(feature = "native")]
pub fn open_local(root: &std::path::Path) -> io::Result<Box<dyn StorageBackend>> {
    Ok(Box::new(local::LocalBackend::new(root)?))
}

/// Multi-disk variant: one `data-server` storage root per disk, each whole
/// object placed on one root by a hash of its key (the root list must stay
/// fixed for a given store). A single-path list degrades to exactly [`open_local`].
#[cfg(feature = "native")]
pub fn open_local_roots(roots: &[std::path::PathBuf]) -> io::Result<Box<dyn StorageBackend>> {
    if roots.len() == 1 {
        return open_local(&roots[0]);
    }
    Ok(Box::new(placement::PlacementBackend::new(roots)?))
}

#[cfg(feature = "native")]
pub use sharded::ShardedBackend;

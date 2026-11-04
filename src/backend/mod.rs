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

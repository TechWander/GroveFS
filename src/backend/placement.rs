//! Multi-disk object placement for a single `data-server`.
//!
//! One storage node often owns several disks. This backend spreads objects
//! over an ordered list of storage roots so capacity and IOPS scale by adding
//! disks:
//! - Placement is a PURE function of the object key (`FNV-1a` of the
//!   placement unit mod the root count), so any process holding the same root
//!   list resolves the same root — no server-side index, no client change, no
//!   protocol change (`data_ref` never learns about roots).
//! - Sub-stripe whole files distribute too: a whole object places on ONE root
//!   (chunk families pin to their object's root — inside a single node a file
//!   split across disks buys no bandwidth and halves nothing), which is what
//!   un-skews the "all small files land on the first disk" pattern.
//! - The root list is fixed for a given store: adding or removing a disk
//!   changes the modulo and therefore re-maps every key, so a node's disk set
//!   is provisioned up front rather than grown in place.
//!
//! Byte-level layout WITHIN each root (the 256-bucket fan-out) is the
//! [`LocalBackend`](super::local::LocalBackend) living on that root; this
//! decorator only picks the root.

use super::{FsStat, Imported, ObjectPage, StorageBackend};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

struct Member {
    #[allow(dead_code)] // kept for diagnostics (root_count/path reporting)
    root: PathBuf,
    backend: Arc<dyn StorageBackend>,
}

/// FNV-1a — the same hash family the bucket fan-out uses — over the bytes of
/// the placement unit (see [`placement_unit`]).
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// The part of a key that decides placement: everything before the first
/// `#` chunk marker, so all chunks of one striped object share a root. Bare
/// uuid keys are their own unit.
fn placement_unit(key: &str) -> &[u8] {
    let bytes = key.as_bytes();
    match bytes.iter().position(|&b| b == b'#') {
        Some(i) => &bytes[..i],
        None => bytes,
    }
}

pub struct PlacementBackend {
    members: Vec<Member>,
}

impl PlacementBackend {
    /// Open one [`LocalBackend`](super::local::LocalBackend) per root (its bucket
    /// table included). Errors if a root cannot be
    /// opened — a storage node must not silently serve a subset of its disks.
    pub fn new(roots: &[PathBuf]) -> io::Result<Self> {
        if roots.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "placement backend needs at least one storage root",
            ));
        }
        let mut members = Vec::with_capacity(roots.len());
        // One process-wide fd budget, split across the roots. Each LocalBackend
        // caches object handles, so letting every disk assume it owns
        // RLIMIT_NOFILE would make an n-disk node hit EMFILE at n times the
        // budget it actually has.
        let per_root = (super::local::fd_limit_default() / roots.len()).max(64);
        for r in roots {
            let backend = Arc::new(super::local::LocalBackend::with_fd_limit(r, per_root)?)
                as Arc<dyn StorageBackend>;
            members.push(Member {
                root: r.clone(),
                backend,
            });
        }
        Ok(Self { members })
    }

    pub fn root_count(&self) -> usize {
        self.members.len()
    }

    /// The root `key` belongs to: a pure function of its placement unit, so
    /// every process with the same root list agrees with no shared state.
    fn primary(&self, key: &str) -> usize {
        (fnv1a(placement_unit(key)) % self.members.len() as u64) as usize
    }

    /// The member backend that owns `key`.
    fn backend(&self, key: &str) -> Arc<dyn StorageBackend> {
        self.members[self.primary(key)].backend.clone()
    }

    /// `df`-style aggregation over the node's disks: sum block/inode counts,
    /// min namemax, and only error when NO root reports (a dead disk then
    /// understates capacity, the safe direction, mirroring the cross-server
    /// stripe's policy).
    fn aggregate_statvfs(&self) -> io::Result<FsStat> {
        let mut acc: Option<FsStat> = None;
        let mut last_err: Option<io::Error> = None;
        for m in &self.members {
            match m.backend.statvfs() {
                Ok(st) => {
                    acc = Some(match acc {
                        None => st,
                        Some(a) => FsStat {
                            tblocks: a.tblocks.saturating_add(st.tblocks),
                            bfree: a.bfree.saturating_add(st.bfree),
                            bavail: a.bavail.saturating_add(st.bavail),
                            files: a.files.saturating_add(st.files),
                            ffree: a.ffree.saturating_add(st.ffree),
                            bsize: a.bsize,
                            namemax: a.namemax.min(st.namemax),
                        },
                    });
                }
                Err(e) => last_err = Some(e),
            }
        }
        acc.ok_or_else(|| last_err.unwrap_or_else(|| io::Error::from_raw_os_error(libc::EIO)))
    }
}

impl StorageBackend for PlacementBackend {
    fn create(&self, key: &str) -> io::Result<()> {
        self.members[self.primary(key)].backend.create(key)
    }

    fn read(&self, key: &str, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        self.backend(key).read(key, offset, len)
    }

    fn write(&self, key: &str, offset: u64, data: &[u8]) -> io::Result<usize> {
        self.backend(key).write(key, offset, data)
    }

    fn len(&self, key: &str) -> io::Result<u64> {
        self.backend(key).len(key)
    }

    fn fsync(&self, key: &str) -> io::Result<()> {
        self.backend(key).fsync(key)
    }

    fn truncate(&self, key: &str, new_len: u64) -> io::Result<()> {
        self.backend(key).truncate(key, new_len)
    }

    fn remove(&self, key: &str) -> io::Result<()> {
        // GC is idempotent: an object absent at its home root is success.
        match self.backend(key).remove(key) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            r => r,
        }
    }

    fn import_at(&self, key: &str, src: &Path) -> io::Result<Imported> {
        self.members[self.primary(key)].backend.import_at(key, src)
    }

    fn list_objects(&self, cursor: Option<&str>, limit: usize) -> io::Result<ObjectPage> {
        // One page may span disks: the shared walker walks members in order and
        // keeps the cursor opaque, so `gc` never learns about the root list.
        super::list_across_members(
            self.members.len(),
            |i| self.members[i].backend.clone(),
            cursor,
            limit,
        )
    }

    fn statvfs(&self) -> io::Result<FsStat> {
        self.aggregate_statvfs()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_roots(tag: &str, n: usize) -> Vec<PathBuf> {
        (0..n)
            .map(|k| {
                let mut p = std::env::temp_dir();
                p.push(format!("grovefs_place_{tag}_{}_{}", std::process::id(), k));
                let _ = std::fs::remove_dir_all(&p);
                p
            })
            .collect()
    }

    fn cleanup(roots: &[PathBuf]) {
        for r in roots {
            let _ = std::fs::remove_dir_all(r);
        }
    }

    /// A bucket file of `key` under root `r` — the layout its LocalBackend
    /// owns; the placement layer must not need to know more than that.
    fn bucket_file(root: &Path, key: &str) -> PathBuf {
        // The member LocalBackend buckets by FNV-1a of the FULL key (chunk
        // keys included), so the in-root path is `root/blocks/<hex>/<key>`.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in key.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100_0000_01b3);
        }
        let v = (h % 256) as u8;
        let hex = format!("{:02x}", v);
        root.join("blocks").join(hex).join(key)
    }

    #[test]
    fn new_objects_spread_over_all_roots_and_roundtrip() {
        let roots = tmp_roots("spread", 4);
        let be = PlacementBackend::new(&roots).unwrap();
        let mut per_root = [0u32; 4];
        for i in 0..400u32 {
            let key = format!("{i:08x}-0000-4000-8000-000000000000");
            be.create(&key).unwrap();
            be.write(&key, 0, b"payload").unwrap();
            // The object physically exists on exactly one root.
            let owned: Vec<usize> = (0..4)
                .filter(|r| bucket_file(&roots[*r], &key).exists())
                .collect();
            assert_eq!(
                owned.len(),
                1,
                "key {key} on roots {owned:?}, want exactly one"
            );
            per_root[owned[0]] += 1;
            assert_eq!(be.read(&key, 0, 32).unwrap(), b"payload".to_vec());
            assert_eq!(be.len(&key).unwrap(), 7);
        }
        for c in per_root {
            assert!(
                c > 40,
                "root got only {c} of 400 objects — placement skewed"
            );
        }
        cleanup(&roots);
    }

    #[test]
    fn chunk_family_pins_to_its_objects_root() {
        let roots = tmp_roots("chunk", 3);
        let be = PlacementBackend::new(&roots).unwrap();
        let base = "cc0ffee0-0000-4000-8000-000000000000";
        be.create(base).unwrap(); // the object itself anchors the root
        be.write(base, 0, b"x").unwrap();
        for i in 0..40u32 {
            let ck = format!("{base}#{i}");
            be.create(&ck).unwrap();
            be.write(&ck, 0, b"c").unwrap();
        }
        let owner = |k: &str| (0..3).find(|r| bucket_file(&roots[*r], k).exists());
        let anchor = owner(base).expect("base object placed");
        assert_eq!(
            owner("nonexistent-peek-0"),
            None,
            "probe-free absent key must exist nowhere"
        );
        for i in 0..40u32 {
            assert_eq!(
                owner(&format!("{base}#{i}")),
                Some(anchor),
                "chunk #{i} drifted off the object's root"
            );
        }
        cleanup(&roots);
    }

    #[test]
    fn statvfs_sums_the_node_disks() {
        let roots = tmp_roots("stat", 2);
        let be = PlacementBackend::new(&roots).unwrap();
        let agg = be.statvfs().unwrap();
        assert!(agg.tblocks > 0 && agg.files > 0);
        cleanup(&roots);
    }

    #[test]
    fn empty_root_list_is_refused() {
        assert!(PlacementBackend::new(&[]).is_err());
    }

    /// One walk covers every disk: objects live on exactly one root, so the
    /// member cursor is what lets `gc` see the whole node instead of only the
    /// disk it happened to be pointed at.
    #[test]
    fn list_objects_walks_every_disk_exactly_once() {
        let roots = tmp_roots("list", 3);
        let be = PlacementBackend::new(&roots).unwrap();
        let mut want = std::collections::BTreeSet::new();
        for i in 0..150u32 {
            let key = format!("{i:08x}-0000-4000-8000-000000000000");
            be.create(&key).unwrap();
            be.write(&key, 0, b"x").unwrap();
            want.insert(key);
        }
        for page_size in [5usize, 37, 149, 150, 151] {
            let mut seen = std::collections::BTreeSet::new();
            let mut cursor: Option<String> = None;
            loop {
                let page = be.list_objects(cursor.as_deref(), page_size).unwrap();
                assert!(
                    page.rows.len() <= page_size.clamp(1, super::super::LIST_LIMIT_MAX),
                    "page exceeded the requested {page_size}"
                );
                for r in page.rows {
                    assert!(seen.insert(r.key.clone()), "{} listed twice", r.key);
                }
                match page.next {
                    Some(n) => cursor = Some(n),
                    None => break,
                }
            }
            assert_eq!(seen, want, "page size {page_size} missed a disk's objects");
        }
        cleanup(&roots);
    }
}

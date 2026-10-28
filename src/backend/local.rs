//! Local-disk implementation of [`StorageBackend`].
//!
//! Design notes for high efficiency:
//! - One object == one file under `<root>/blocks/<bucket>/<key>`. Objects fan
//!   out over a FIXED 256-bucket prefix table (FNV-1a of the key), so a single
//!   storage root scales to tens of millions of objects while every directory
//!   stays small: path resolution remains O(1) (two joins, no scanning) and no
//!   htree/B+tree directory ever degrades to backup-hostile entry counts.
//!   Buckets are precreated at startup, so the hot path pays no mkdir.
//! - We cache the raw file descriptor per key so repeated read/write on the
//!   same hot file never re-open. Locks are striping-free here because fuser
//!   already parallelizes across objects; the RwLock only guards the map.
//! - IO uses `pread`/`pwrite` (positional, no `lseek`), so concurrent offsets
//!   on the same fd are safe and race-free.
//! - `fsync` uses `fdatasync` semantics via `fsync` on the fd.

use super::{FsStat, Imported, StorageBackend};
use crate::stats;
use libc::{close, fstat, fsync, ftruncate, pread, pwrite, statvfs};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::ffi::c_void;
use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// Bucket fan-out factor. Fixed forever: the value is baked into every
/// object's on-disk location, so changing it would orphan existing placements.
const BUCKETS: u64 = 256;

/// FNV-1a over the key bytes, reduced to a two-hex-digit bucket name.
/// Deterministic and process-independent, and it mixes ALL key bytes — keys
/// share long prefixes (`s2:16777216:<uuid>` for stripe layout, `<uuid>#<chunk>`
/// for chunk objects), so slicing the first two chars would correlate the
/// bucket choice with that shared prefix and put one object's chunks (or one
/// layout's objects) in the same bucket. Bucket names are two lowercase hex
/// digits, the same alphabet a uuid prefix uses.
fn bucket_hex(key: &str) -> [u8; 2] {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in key.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    let v = (h % BUCKETS) as u8;
    [hex_digit(v >> 4), hex_digit(v & 0x0f)]
}

fn hex_digit(n: u8) -> u8 {
    debug_assert!(n < 16);
    if n < 10 {
        b'0' + n
    } else {
        b'a' + (n - 10)
    }
}

/// Two-hex-digit bucket name for index `b` (0..BUCKETS), matching the
/// alphabet [`bucket_hex`] produces for keys that hash to `b`.
fn hex_pair(b: u64) -> [u8; 2] {
    let v = b as u8;
    [hex_digit(v >> 4), hex_digit(v & 0x0f)]
}

pub struct LocalBackend {
    root: PathBuf,
    /// key -> cached raw fd. A negative entry (None) means "not open"; we only
    /// cache fds that have been touched at least once.
    fds: RwLock<HashMap<String, i32>>,
    /// Guards object creation so two threads don't race to open a brand-new file.
    creating: RwLock<()>,
    dirty_marker: AtomicBool,
}

impl LocalBackend {
    pub fn new(root: &Path) -> io::Result<Self> {
        let blocks = root.join("blocks");
        fs::create_dir_all(&blocks)?;
        // Precreate the whole fixed bucket table so the hot path never mkdirs.
        // 256 mkdirs once at startup is milliseconds; failures (existing dirs)
        // are irrelevant.
        for b in 0..BUCKETS {
            let mut name = [0u8; 2];
            name.copy_from_slice(&hex_pair(b as u64));
            let _ = fs::create_dir(blocks.join(std::str::from_utf8(&name).unwrap()));
        }
        Ok(Self {
            root: root.to_path_buf(),
            fds: RwLock::new(HashMap::new()),
            creating: RwLock::new(()),
            dirty_marker: AtomicBool::new(false),
        })
    }

    /// Where a key is (or would be) stored under the fan-out layout.
    fn bucket_path(&self, key: &str) -> PathBuf {
        let hex = bucket_hex(key);
        let name = std::str::from_utf8(&hex).unwrap();
        self.root.join("blocks").join(name).join(key)
    }

    /// An object's on-disk location: its fan-out bucket. Public so tests and
    /// on-disk diagnostics resolve the same path the IO path uses.
    pub fn object_path(&self, key: &str) -> PathBuf {
        self.bucket_path(key)
    }

    /// Return a cached fd for `key`, opening the file if needed.
    fn fd(&self, key: &str, create: bool) -> io::Result<i32> {
        // Fast path: read lock only.
        if let Some(&fd) = self.fds.read().get(key) {
            return Ok(fd);
        }
        // Slow path: serialize creation, then open.
        let _g = self.creating.write();
        // Re-check under the creation lock (double-checked locking).
        if let Some(&fd) = self.fds.read().get(key) {
            return Ok(fd);
        }
        let bucket = self.bucket_path(key);
        let mut opts = OpenOptions::new();
        opts.read(true).write(true);
        let file = match opts.open(&bucket) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound && create => {
                // Brand-new object: materialize it under its bucket (the parent
                // is precreated at startup, so the mkdir is a no-op in practice).
                if let Some(d) = bucket.parent() {
                    fs::create_dir_all(d)?;
                }
                opts.create(true).open(&bucket)?
            }
            // Missing and not creating: return the real OS error, which carries
            // `raw_os_error() == ENOENT` (a synthesized NotFound would degrade to
            // EIO over the remote wire and break the striped size probe).
            Err(e) => return Err(e),
        };
        let fd = file.as_raw_fd();
        // Leak the File so the fd stays valid for the cache lifetime; we close
        // it only on remove(). This is intentional for a long-running daemon.
        std::mem::forget(file);
        self.fds.write().insert(key.to_string(), fd);
        Ok(fd)
    }

    fn stat_len(fd: i32) -> io::Result<u64> {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let rc = unsafe { fstat(fd, &mut st as *mut _ as *mut libc::stat) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(st.st_size as u64)
    }
}

impl StorageBackend for LocalBackend {
    fn create(&self, key: &str) -> io::Result<()> {
        self.fd(key, true)?;
        Ok(())
    }

    fn read(&self, key: &str, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let _t = stats::start(stats::Stage::IoLocalRead);
        let fd = self.fd(key, false)?;
        // pread naturally returns fewer bytes at EOF, so no fstat needed.
        let mut buf = vec![0u8; len];
        let n = unsafe {
            pread(
                fd,
                buf.as_mut_ptr() as *mut c_void,
                len,
                offset as libc::off_t,
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        buf.truncate(n as usize);
        Ok(buf)
    }

    fn write(&self, key: &str, offset: u64, data: &[u8]) -> io::Result<usize> {
        let _t = stats::start(stats::Stage::IoLocalWrite);
        let fd = self.fd(key, true)?;
        let mut done = 0usize;
        while done < data.len() {
            let n = unsafe {
                pwrite(
                    fd,
                    data[done..].as_ptr() as *const c_void,
                    data.len() - done,
                    (offset + done as u64) as libc::off_t,
                )
            };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            done += n as usize;
        }
        self.dirty_marker.store(true, Ordering::Relaxed);
        Ok(done)
    }

    fn len(&self, key: &str) -> io::Result<u64> {
        let _t = stats::start(stats::Stage::IoLocalLen);
        let fd = self.fd(key, false)?;
        Self::stat_len(fd)
    }

    fn fsync(&self, key: &str) -> io::Result<()> {
        let fd = self.fd(key, false)?;
        if unsafe { fsync(fd) } != 0 {
            return Err(io::Error::last_os_error());
        }
        self.dirty_marker.store(false, Ordering::Relaxed);
        Ok(())
    }

    fn truncate(&self, key: &str, new_len: u64) -> io::Result<()> {
        let fd = self.fd(key, true)?;
        if unsafe { ftruncate(fd, new_len as libc::off_t) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn remove(&self, key: &str) -> io::Result<()> {
        // Drop the cached fd first, then unlink the bucket file.
        let fd = self.fds.write().remove(key);
        if let Some(fd) = fd {
            unsafe { close(fd) };
        }
        let path = self.bucket_path(key);
        if path.exists() {
            fs::remove_file(path)?;
        }
        Ok(())
    }

    fn import_at(&self, key: &str, src: &Path) -> io::Result<Imported> {
        let dst = self.bucket_path(key);
        if let Some(d) = dst.parent() {
            fs::create_dir_all(d)?;
        }
        // Zero-copy fast path: a hard link shares the source's inode, so the
        // object costs no extra bytes and reads/writes through EITHER path see
        // the same live data. `remove()` (GC) then only unlinks the blocks/
        // path — the source path survives until its own last link goes, which
        // is exactly the "importing must not destroy the dataset" guarantee.
        if fs::hard_link(src, &dst).is_ok() {
            return Ok(Imported::Hardlinked);
        }
        // Cross-device, filesystems forbidding links, or source already being
        // a link we may not duplicate: fall back to streaming a copy.
        let mut file = fs::File::open(src)?;
        let mut out = fs::File::create(&dst)?;
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = std::io::Read::read(&mut file, &mut buf)?;
            if n == 0 {
                break;
            }
            std::io::Write::write_all(&mut out, &buf[..n])?;
        }
        // Durably land the bytes before the object is registered as imported:
        // an interrupted copy otherwise leaves a short object that the metadata
        // side would still point `data_ref` at (the hard-link path shares the
        // source inode and needs no flush).
        out.sync_all()?;
        Ok(Imported::Copied)
    }

    fn statvfs(&self) -> io::Result<FsStat> {
        let croot = std::ffi::CString::new(self.root.as_os_str().as_encoded_bytes())
            .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
        let mut sv: statvfs = unsafe { std::mem::zeroed() };
        if unsafe { statvfs(croot.as_ptr(), &mut sv as *mut _) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(FsStat {
            tblocks: sv.f_blocks,
            bfree: sv.f_bfree,
            bavail: sv.f_bavail,
            files: sv.f_files,
            ffree: sv.f_ffree,
            bsize: sv.f_bsize,
            namemax: sv.f_namemax as u32,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("grovefs_local_{}_{}", std::process::id(), tag));
        let _ = fs::remove_dir_all(&p);
        p
    }

    /// The bucket table is precreated at startup with exactly the names
    /// `bucket_hex` produces — a key's hash and its on-disk dir can never
    /// disagree.
    #[test]
    fn buckets_precreated_and_consistent_with_key_hash() {
        let root = tmp_root("buckets");
        // The binding itself is the point: startup precreates the table.
        let _be = LocalBackend::new(&root).unwrap();
        for b in 0..BUCKETS {
            let name = String::from_utf8_lossy(&hex_pair(b)).to_string();
            assert!(
                root.join("blocks").join(&name).is_dir(),
                "missing bucket {name}"
            );
        }
        // bucket_hex lands inside the precreated set for arbitrary keys.
        for key in [
            "3f2a1b4c-0000-4000-8000-000000000001",
            "3f2a1b4c-0000-4000-8000-000000000001#7",
            "s2:16777216:3f2a1b4c-0000-4000-8000-000000000001",
            "k1",
        ] {
            let hex = bucket_hex(key);
            let name = std::str::from_utf8(&hex).unwrap();
            assert!(
                root.join("blocks").join(name).is_dir(),
                "key {key} hashes to bucket {name} which is not precreated"
            );
        }
        let _ = fs::remove_dir_all(&root);
    }

    /// New objects land in their bucket, and the placement distributes:
    /// both bare uuids and `<uuid>#<chunk>` families (which share a long
    /// prefix) fan across the table instead of stacking in one dir.
    #[test]
    fn objects_fan_out_and_spread_evenly() {
        let root = tmp_root("fanout");
        let be = LocalBackend::new(&root).unwrap();
        let mut buckets = std::collections::HashSet::new();
        for i in 0..4096u32 {
            let key = format!("{i:08x}-{:04x}-4000-8000-000000000000", i * 7);
            be.write(&key, 0, b"x").unwrap();
            let p = be.object_path(&key);
            assert_eq!(
                p.parent().unwrap().file_name().unwrap(),
                String::from_utf8_lossy(&bucket_hex(&key)).as_ref()
            );
            assert!(
                !root.join("blocks").join(&key).exists(),
                "new object landed at the flat root"
            );
            buckets.insert(p.parent().unwrap().file_name().unwrap().to_owned());
        }
        // Uniform mod-256 hashing of 4096 distinct keys keeps every bucket at
        // ~16 entries; a prefix-correlated scheme would collapse to a handful.
        assert!(
            buckets.len() >= 200,
            "only {} distinct buckets used",
            buckets.len()
        );
        // One object's own chunk family must not stack either.
        let mut chunk_buckets = std::collections::HashSet::new();
        for i in 0..1000u32 {
            chunk_buckets.insert(
                be.bucket_path(&format!("cc0ffee0-dead-4000-8000-000000000000#{i}"))
                    .parent()
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_owned(),
            );
        }
        assert!(
            chunk_buckets.len() >= 50,
            "chunk family stacked in {} buckets",
            chunk_buckets.len()
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// Zero-copy import must keep its promise under the fan-out: the object
    /// path is a second link on the source's inode.
    #[test]
    fn import_hardlink_lands_in_bucket_sharing_inode() {
        use std::os::unix::fs::MetadataExt;
        let root = tmp_root("import");
        fs::create_dir_all(&root).unwrap();
        let src = root.join("src.bin");
        fs::write(&src, b"legacy").unwrap();
        let ino = fs::metadata(&src).unwrap().ino();
        let be = LocalBackend::new(&root).unwrap();
        let got = be.import_at("imp-key", &src).unwrap();
        assert_eq!(got, Imported::Hardlinked);
        let obj = fs::metadata(be.object_path("imp-key")).unwrap();
        assert_eq!(obj.ino(), ino, "imported object shares the source inode");
        assert_eq!(obj.nlink(), 2);
        be.remove("imp-key").unwrap();
        assert_eq!(
            fs::read_to_string(&src).unwrap(),
            "legacy",
            "GC never hurts the source"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A missing object must fail with a REAL errno (ENOENT), never a
    /// synthesized `Error::new(NotFound, ..)` that carries no raw code. The
    /// remote data-server maps an error over the wire with
    /// `raw_os_error().unwrap_or(EIO)`, and the striped `len()` needs a genuine
    /// NotFound to tell "chunk definitively absent" (stop the size scan) apart
    /// from "transient EIO" (retry, then fail honestly). A code-less NotFound
    /// degraded to EIO, so a striped file truncated below its high-water mark
    /// reported size 0 — the regression this guards.
    #[test]
    fn missing_object_reports_real_enoent() {
        let root = tmp_root("enoent");
        let be = LocalBackend::new(&root).unwrap();
        let e = be.len("no-such-key").unwrap_err();
        assert_eq!(e.raw_os_error(), Some(libc::ENOENT), "len: {e:?}");
        let e = be.read("no-such-key", 0, 8).unwrap_err();
        assert_eq!(e.raw_os_error(), Some(libc::ENOENT), "read: {e:?}");
        let _ = fs::remove_dir_all(&root);
    }
}

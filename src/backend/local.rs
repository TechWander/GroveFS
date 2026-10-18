//! Local-disk implementation of [`StorageBackend`].
//!
//! Design notes for high efficiency:
//! - One object == one file under `<root>/blocks/<key>`. Flat layout avoids
//!   deep directory trees and keeps path resolution O(1).
//! - We cache the raw file descriptor per key so repeated read/write on the
//!   same hot file never re-open. Locks are striping-free here because fuser
//!   already parallelizes across objects; the RwLock only guards the map.
//! - IO uses `pread`/`pwrite` (positional, no `lseek`), so concurrent offsets
//!   on the same fd are safe and race-free.
//! - `fsync` uses `fdatasync` semantics via `fsync` on the fd.

use super::{FsStat, Imported, StorageBackend};
use libc::{close, fstat, fsync, ftruncate, pread, pwrite, statvfs};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::ffi::c_void;
use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

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
        fs::create_dir_all(root.join("blocks"))?;
        Ok(Self {
            root: root.to_path_buf(),
            fds: RwLock::new(HashMap::new()),
            creating: RwLock::new(()),
            dirty_marker: AtomicBool::new(false),
        })
    }

    fn block_path(&self, key: &str) -> PathBuf {
        self.root.join("blocks").join(key)
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
        let path = self.block_path(key);
        let mut opts = OpenOptions::new();
        opts.read(true).write(true);
        if create {
            opts.create(true);
        }
        let file = opts.open(&path)?;
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
        let fd = self.fd(key, false)?;
        let cur_len = Self::stat_len(fd)?;
        if offset >= cur_len {
            return Ok(Vec::new());
        }
        let want = len.min((cur_len - offset) as usize);
        let mut buf = vec![0u8; want];
        let n = unsafe {
            pread(
                fd,
                buf.as_mut_ptr() as *mut c_void,
                want,
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
        // Drop the cached fd first, then unlink.
        let fd = self.fds.write().remove(key);
        if let Some(fd) = fd {
            unsafe { close(fd) };
        }
        let path = self.block_path(key);
        if path.exists() {
            fs::remove_file(path)?;
        }
        Ok(())
    }

    fn import_at(&self, key: &str, src: &Path) -> io::Result<Imported> {
        let dst = self.block_path(key);
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

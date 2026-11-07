//! The FUSE `Filesystem` implementation: the glue that turns kernel POSIX calls
//! into (a) view-tree lookups in the metadata engine and (b) object IO in the
//! pluggable storage backend. No NFS anywhere in this path.
//!
//! Performance choices baked in here:
//! - `read`/`write` go straight to the backend's positional IO (no temp copies).
//! - Attribute lookups consult the view tree; sizes come from the backend's
//!   cached fd (`fstat`), which is cheap.
//! - We set modest kernel entry/attr timeouts so the VFS dentry cache absorbs
//!   repeated `lookup`/`getattr` without invalidating isolation.
//! - The session dispatches callbacks concurrently across worker threads
//!   (fuser 0.18 `Config::n_threads`), which the `&self` trait makes sound:
//!   all per-inode state lives behind the locks below.

use crate::backend::StorageBackend;
use crate::meta::{LockType, MetaError, MetaProvider, NodeInfo, NodeKind};
use fuser::{
    AccessFlags, BsdFileFlags, Errno, FileAttr, FileHandle, FileType, FopenFlags, Generation,
    INodeNo, InitFlags, IoctlFlags, KernelConfig, LockOwner, OpenFlags, RenameFlags, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyIoctl,
    ReplyLock, ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr, Request, TimeOrNow, WriteFlags,
};
use parking_lot::RwLock;
use std::ffi::OsStr;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::stats;

/// Name of the synthetic, read-only control node exposing the live telemetry
/// snapshot. It materializes ONLY while `--stats` is active, so a default mount
/// never sees it and the real namespace / governance invariants are untouched.
pub const STATS_NAME: &str = ".grove-stats";
/// Sentinel inode for the control node, far above any DB-generated id (real ids
/// come from a small 1-based sequence), so it can never collide with a node.
pub const STATS_INO: u64 = u64::MAX - 1;
/// Opaque readdir cookie handed to the control-node row. `off_t` is signed, so
/// it stays inside `i64::MAX`; real per-entry offsets are tiny directory ranks
/// and never reach it. A resume at this exact offset means "listing done" -> EOF.
const STATS_COOKIE: u64 = i64::MAX as u64;

/// Handle table: maps an inode number to its object key so read/write don't
/// re-query metadata each call.
///
/// We key by INODE, not by the open handle `fh`, because with `FUSE_WRITEBACK_CACHE`
/// the kernel flushes dirty pages asynchronously — often *after* the writing
/// process has closed the file (i.e. after our `release`). A per-handle table
/// would already be torn down by then and reject the writeback with ENOENT.
/// The inode->object binding is immutable for the inode's lifetime, so this map
/// is populated on lookup/create/open and pruned in `forget` (the kernel's
/// "inode is dead" signal).
pub struct GroveFS {
    meta: Arc<dyn MetaProvider>,
    backend: Arc<dyn StorageBackend>,
    ino_to_key: RwLock<std::collections::HashMap<u64, Arc<str>>>,
    /// Last write-path mtime touch per inode, for the `write()` debounce below.
    last_write_touch: RwLock<std::collections::HashMap<u64, SystemTime>>,
    /// When set, every mutating operation is refused with EROFS. This is a
    /// filesystem-side safety net that mirrors the kernel `MS_RDONLY` mount flag
    /// (both are applied for `--ro`), so the view stays read-only even if the
    /// kernel-level flag were somehow not honored for a given request.
    readonly: bool,
    /// When set, CONTENT of existing files is read-only while the tree stays
    /// writable: `write` and a `setattr` size change (truncate/extend, O_TRUNC)
    /// are refused with EROFS, but mkdir/rename/unlink/symlink/link and
    /// attribute updates pass through. This is how a FORK mounts: it owns its
    /// metadata tree (organize it freely) while the bytes are shared with the
    /// base view, so uploading content belongs to the base view only. Set from
    /// the server's M_OPEN ack (remote) or the `fork <base>` definition
    /// (local). Honest boundary: it is a CLIENT-side gate — a raw protocol
    /// client that skips this FUSE layer can still write to the data plane.
    content_ro: bool,
    /// Strict coherence: disable the kernel's attribute/dentry cache (TTL 0) and
    /// read file data straight through (`FopenFlags::FOPEN_DIRECT_IO`, bypassing
    /// the page cache). Chosen for multi-client mounts that share a `meta-server`, where
    /// one client's write must be visible to another immediately rather than
    /// after the default cache TTL. Costs the caching benefit; opt-in via `--strict`.
    strict: bool,
    /// Negotiate FUSE_READDIRPLUS (kernel >= 4.6): READDIR replies then carry
    /// per-entry dentry+attr, so `ls -l` on a big dir costs one metadata
    /// round-trip instead of N LOOKUP+GETATTR. On by default; `--no-readdirplus`
    /// turns it off for A/B benchmarks and compatibility debugging. `init` only
    /// ever offers it if the provider also pages in its own store — see
    /// `MetaProvider::supports_paged_readdir`.
    readdirplus: bool,
    /// Base kernel attr/dentry cache TTL used when *not* `--strict`. Defaults to
    /// 2 s (unchanged behavior). `--tune-cache <secs>` raises it for read-heavy /
    /// immutable trees (e.g. the astronomy "data never lands" read-once pattern):
    /// fewer LOOKUP/GETATTR round-trips. Cross-client freshness is still
    /// preserved by the default notify-invalidation push, and `--strict` pins
    /// this to 0, so this knob only ever widens caching for callers who opt in.
    cache_ttl: Duration,
    /// GCRA shapers for CONTENT bytes of THIS mount (`--bw-read` /
    /// `--bw-write`; rate 0 = off). One schedule per direction, shared by
    /// every worker thread of the mount. Honest boundary: a client-side,
    /// per-mount shaper — self-discipline, not isolation (see `crate::bwlimit`).
    bw_read: crate::bwlimit::RateLimiter,
    bw_write: crate::bwlimit::RateLimiter,
    /// Per-mount session id stamped on every lock this daemon grants, so a
    /// background heartbeat can renew exactly its own leases (`renew_locks`)
    /// and the authority can reap them on disconnect without touching peers.
    lock_sid: u64,
    /// Set once the lock-renewal heartbeat thread has been spawned (first lock
    /// grant), so we never spawn more than one per process.
    lock_renew_started: std::sync::atomic::AtomicBool,
    /// Server-arbitrated POSIX record locks (`fcntl F_SETLK/F_SETLKW`) get NO
    /// close/exit notification from the kernel: `fuser` surfaces a `release`
    /// `lock_owner` only for flock(2) (`FUSE_RELEASE_FLOCK_UNLOCK`), so the
    /// `if let Some(owner)` teardown in [`GroveFS::release`] never fires for
    /// them. Without bookkeeping a POSIX lock left by `close`/process exit
    /// would be renewed forever by the sid heartbeat and hang blocked waiters
    /// up to the 10-minute cap (§5-3). So this mount records, per open file
    /// description (`fh`), the `(ino, owner)` it took POSIX locks for, and
    /// drops the authority-side rows from `release` once the LAST locking
    /// description of that `(ino, owner)` closes.
    posix_lock_fh_owner: RwLock<std::collections::HashMap<u64, (i64, u64)>>,
    /// Live locking file descriptions per `(ino, owner)`, so a description
    /// closing while another of the same `(ino, owner)` stays open does NOT
    /// drop the lock (it is released only when the last one closes).
    posix_lock_owner_fds:
        RwLock<std::collections::HashMap<(i64, u64), std::collections::HashSet<u64>>>,
}

/// Generate a process-unique, non-zero mount session id from the clock, pid and
/// a monotonic counter, avalanche-mixed so distinct mounts started in the same
/// tick still differ.
fn gen_sid() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let pid = std::process::id() as u64;
    let mut h = t ^ (pid << 17) ^ n.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    if h == 0 {
        1
    } else {
        h
    }
}

impl GroveFS {
    /// One flat constructor for the mount's option surface (the CLI resolves
    /// every knob once and passes it through verbatim), so the call site stays
    /// readable rather than hiding them behind a builder struct.
    #[allow(clippy::too_many_arguments)] // mount options mirror the CLI flags 1:1
    pub fn new(
        meta: Arc<dyn MetaProvider>,
        backend: Arc<dyn StorageBackend>,
        readonly: bool,
        content_ro: bool,
        strict: bool,
        readdirplus: bool,
        cache_ttl_secs: u64,
        bw_read_bps: u64,
        bw_write_bps: u64,
    ) -> Self {
        Self {
            meta,
            backend,
            ino_to_key: RwLock::new(std::collections::HashMap::new()),
            last_write_touch: RwLock::new(std::collections::HashMap::new()),
            readonly,
            content_ro,
            strict,
            readdirplus,
            cache_ttl: Duration::from_secs(cache_ttl_secs),
            bw_read: crate::bwlimit::RateLimiter::new(bw_read_bps),
            bw_write: crate::bwlimit::RateLimiter::new(bw_write_bps),
            lock_sid: gen_sid(),
            lock_renew_started: std::sync::atomic::AtomicBool::new(false),
            posix_lock_fh_owner: RwLock::new(std::collections::HashMap::new()),
            posix_lock_owner_fds: RwLock::new(std::collections::HashMap::new()),
        }
    }

    /// Ensure the lock-renewal heartbeat thread is running (spawned at most once
    /// per process, on the first successful lock grant). Every `lease/3` it
    /// extends the lease on this mount's `lock_sid`, so a live holder never hits
    /// its deadline while the daemon is up.
    ///
    /// Why blanket sid-renewal is safe: a holder's lock row is DELETED by FUSE
    /// `release` the instant its file description's last reference closes — on
    /// normal exit *and* on `SIGKILL` (the kernel still closes the fd, and the
    /// delivered `release` carries the matching `lock_owner`). So the only rows
    /// still present at a renew tick belong to genuinely-open descriptions; a
    /// closed (or crashed-holder) row is already gone and can not be resurrected.
    /// A crashed *daemon* runs no tick at all, so its locks age out inside one
    /// lease — the bounded-crash-window guarantee.
    fn ensure_lock_renewer(&self) {
        use std::sync::atomic::Ordering;
        if self.lock_renew_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let meta = self.meta.clone();
        let sid = self.lock_sid;
        let cadence = (crate::meta::LockLease::secs() / 3).max(1) as u64;
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(cadence));
            meta.renew_locks(sid);
        });
    }

    /// Record that file description `fh` holds POSIX locks on `(ino, owner)`.
    /// Called on every Read/Write grant; inserting an already-present `fh` is
    /// a no-op, so repeat locks (same-owner convert) do not inflate the set.
    fn note_posix_lock(&self, fh: u64, ino: i64, owner: u64) {
        self.posix_lock_fh_owner
            .write()
            .insert(fh, (ino, owner));
        self.posix_lock_owner_fds
            .write()
            .entry((ino, owner))
            .or_default()
            .insert(fh);
    }

    /// POSIX counterpart of the `release` `lock_owner` teardown, which the
    /// kernel never delivers for `fcntl` locks. When the LAST locking
    /// description of an `(ino, owner)` closes, delete its rows from the
    /// authority with a full-range Unlock (the delete arm ignores `pid`), so
    /// a lock left by `close`/process exit stops being renewed and a blocked
    /// `F_SETLKW` waiter is released within one poll instead of hanging to the
    /// 10-minute cap (§5-3). Best-effort: if the meta call fails the row still
    /// ages out within one lease, which is no worse than the pre-fix state.
    fn drop_posix_lock_on_release(&self, fh: u64) {
        let pair = match self.posix_lock_fh_owner.write().remove(&fh) {
            Some(p) => p,
            None => return,
        };
        let mut live = self.posix_lock_owner_fds.write();
        let last = match live.get_mut(&pair) {
            Some(set) => {
                set.remove(&fh);
                set.is_empty()
            }
            None => false,
        };
        if last {
            live.remove(&pair);
            drop(live);
            let _ = self.meta.lock_op(
                pair.0,
                pair.1,
                0,
                u64::MAX,
                LockType::Unlock,
                0,
                self.lock_sid,
            );
        }
    }

    /// Entry/attribute cache lifetime for lookups that could go stale from a
    /// peer's write: full caching normally, zero under `--strict`.
    fn ttl(&self) -> Duration {
        if self.strict {
            Duration::ZERO
        } else {
            self.cache_ttl
        }
    }

    /// Shorter TTL used right after a mutating op returns a fresh attr.
    fn short_ttl(&self) -> Duration {
        if self.strict {
            Duration::ZERO
        } else {
            // Half the base TTL (floored at 1 s when the base is >=2 s), matching
            // the historical 2 s / 1 s pairing under the default cache_ttl.
            (self.cache_ttl / 2).max(Duration::from_secs(1))
        }
    }

    /// `open` flags: bypass the page cache under `--strict` so reads see other
    /// clients' writes; otherwise let the kernel cache normally.
    fn open_flags(&self) -> FopenFlags {
        if self.strict {
            FopenFlags::FOPEN_DIRECT_IO
        } else {
            FopenFlags::empty()
        }
    }

    /// Page-size policy for the directory callbacks, as (first page, cap).
    /// An engine that pages inside its own store is asked for geometrically
    /// growing SQL pages — a buffer that fills after ~20 entries costs 32 rows,
    /// not the whole directory. This covers the SQLite and PostgreSQL engines,
    /// and a remote client whose `meta-server` advertised the `M_READDIRP` cap.
    /// Providers that cannot page in their store (a derived/layout lens, or a
    /// remote server that did not advertise the cap) would re-materialise the
    /// entire listing for every page, so they are asked for one full listing per
    /// kernel buffer: one round trip per buffer, no paging assumption.
    fn fetch_policy(&self) -> (usize, usize) {
        if self.meta.supports_paged_readdir() {
            (READDIR_FETCH_MIN, READDIR_FETCH_MAX)
        } else {
            (usize::MAX, usize::MAX)
        }
    }

    /// Remember the object key backing an inode.
    fn cache_key(&self, ino: u64, key: &str) {
        self.ino_to_key.write().insert(ino, Arc::from(key));
    }

    /// Resolve the object key for an inode: fast in-memory map, falling back to
    /// the metadata engine (then caching it) so a request that races ahead of
    /// `open` — e.g. a writeback flush — still resolves.
    /// Returns `Arc<str>` so the hot path costs a refcount bump, not a heap alloc.
    fn key_for_ino(&self, ino: u64) -> Option<Arc<str>> {
        if let Some(k) = self.ino_to_key.read().get(&ino) {
            return Some(Arc::clone(k));
        }
        let k = self.meta.getattr(ino as i64)?.data_ref?;
        self.cache_key(ino, &k);
        Some(Arc::from(k.as_str()))
    }

    /// Debounce gate for the write-path mtime touch: takes the timestamp lock
    /// only to decide/record, and NEVER calls into `meta` while holding it —
    /// with --meta that would serialize every concurrent writer behind one
    /// synchronous RPC inside our own lock.
    fn should_touch_write(&self, ino: u64) -> bool {
        should_touch_write(&mut self.last_write_touch.write(), ino, SystemTime::now())
    }

    fn build_attr(&self, node: &NodeInfo) -> FileAttr {
        let (size, kind) = match node.kind {
            NodeKind::Dir => (node.size, FileType::Directory),
            NodeKind::Symlink => (node.size, FileType::Symlink),
            NodeKind::Regular => {
                let sz = match &node.data_ref {
                    Some(k) => self.backend.len(k).unwrap_or(node.size),
                    None => 0,
                };
                (sz, FileType::RegularFile)
            }
        };
        FileAttr {
            ino: INodeNo(node.id as u64),
            size,
            blocks: size.div_ceil(512),
            atime: timeval_from(node.atime),
            mtime: timeval_from(node.mtime),
            ctime: timeval_from(node.ctime),
            crtime: UNIX_EPOCH,
            kind,
            perm: (node.mode & 0o7777) as u16,
            nlink: node.nlink as u32,
            uid: node.uid,
            gid: node.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    /// Attribute for the synthetic control node. It is a small, read-only,
    /// regular file owned by root with 0444; its size is the current snapshot
    /// length (informational — `read` is authoritative and returns a short/empty
    /// slice at EOF, which is the portable way to size a dynamic virtual file).
    fn stats_attr() -> FileAttr {
        let size = stats::snapshot_text().len() as u64;
        FileAttr {
            ino: INodeNo(STATS_INO),
            size,
            blocks: size.div_ceil(512),
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind: FileType::RegularFile,
            perm: 0o444,
            nlink: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    /// Slice the live snapshot for a `read` at `offset..offset+size`. Returns
    /// empty past EOF so the kernel sees end-of-file after the last chunk.
    fn stats_slice(offset: u64, size: usize) -> Vec<u8> {
        let body = stats::snapshot_text();
        let b = body.as_bytes();
        let off = offset as usize;
        if off >= b.len() {
            return Vec::new();
        }
        let end = off.saturating_add(size).min(b.len());
        b[off..end].to_vec()
    }

    fn err_to_errno(e: MetaError) -> Errno {
        match e {
            MetaError::Exists => Errno::EEXIST,
            MetaError::NotFound => Errno::ENOENT,
            MetaError::NotEmpty => Errno::ENOTEMPTY,
            MetaError::TypeMismatch => Errno::ENOTDIR,
            MetaError::OpNotPermitted => Errno::EPERM,
            MetaError::BadParentCycle => Errno::EINVAL,
            MetaError::Db(_) => Errno::EIO,
        }
    }
}

fn timeval_from(secs: i64) -> std::time::SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs.max(0) as u64)
}

impl fuser::Filesystem for GroveFS {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> io::Result<()> {
        // Negotiate kernel-side optimizations. With the session now dispatching
        // concurrently across worker threads these stay the biggest throughput
        // levers, because they cut the number of synchronous request/response
        // round-trips rather than adding parallelism (that comes from n_threads):
        // - ASYNC_READ     : kernel may issue overlapping reads without serializing.
        // - BIG_WRITES     : accept writes larger than 4KiB (up to max_write), so
        //   dd/cp stream in big chunks instead of 4KiB at a time.
        // - WRITEBACK_CACHE: kernel buffers/coalesces writes and flushes them in
        //   large batches. This is the single biggest sequential-write win (it
        //   lifted measured throughput ~4x) but means the kernel can flush a
        //   dirty page *after* the writer closed the file; our IO resolves the
        //   object by INODE (see `ino_to_key`), not by the open handle, precisely
        //   so those late writebacks still land correctly — and resolve under
        //   the RwLock, so a worker flushing one inode never races a lookup of
        //   another.
        //   NOT requested on a CONTENT-read-only (fork) mount: with the page
        //   cache in play the kernel answers write(2) from cache and the EROFS
        //   our `write` gate returns only surfaces at fsync/close — a shell `>>`
        //   exits 0 having written nothing. Without writeback every write is a
        //   synchronous FUSE_WRITE, so the refusal lands on the first write(2)
        //   and the failure point matches the truncate-at-open one. Forks have
        //   no write throughput to lose: every write is refused regardless.
        // - PARALLEL_DIROPS: allow concurrent directory-entry (lookup/readdir) ops.
        // - POSIX_LOCKS + FLOCK_LOCKS: negotiate remote-aware advisory locking.
        //   Without these the kernel short-circuits `fcntl F_SETLK` and `flock(2)`
        //   to its own per-mount `posix_lock` table, so two clients mounting the
        //   same view (or two hosts sharing a `meta-server`) never send FUSE_LK
        //   and can't observe each other's locks. With them, the kernel forwards
        //   every LK request here and we arbitrate via the `locks` table in
        //   `MetaEngine` — the single authoritative state that WAL-replicates to
        //   the HA standby and survives failover.
        let caps = InitFlags::FUSE_ASYNC_READ
            | InitFlags::FUSE_BIG_WRITES
            | InitFlags::FUSE_PARALLEL_DIROPS
            | InitFlags::FUSE_POSIX_LOCKS
            | InitFlags::FUSE_FLOCK_LOCKS
            | if self.content_ro {
                InitFlags::empty()
            } else {
                InitFlags::FUSE_WRITEBACK_CACHE
            };
        // Ignore which exact bits the kernel rejected; use whatever it supports.
        let _ = config.add_capabilities(caps);
        // AUTO_INVAL_DATA (kernel >= 3.15): when a cached attr is refreshed and
        // mtime/size changed, the kernel also drops the file's cached page data.
        // This is what makes a raised `--tune-cache` TTL safe: a remote write that
        // bumps size/mtime still invalidates a peer's stale pages on next access,
        // complementing our notify-invalidation push. Negotiated as its OWN
        // all-or-nothing group so an older kernel that lacks it drops only this
        // bit (the core caps above stay applied) — behavior degrades to the
        // pre-existing notify-only path, never regressing the rest.
        let _ = config.add_capabilities(InitFlags::FUSE_AUTO_INVAL_DATA);
        // READDIRPLUS (kernel >= 4.6): reply a directory listing with per-entry
        // dentry+attr already filled in, so `ls -l` on a big dir costs one metadata
        // round-trip instead of N LOOKUP+GETATTR. Negotiated as its own
        // all-or-nothing group (add_capabilities is per-group): on a kernel without
        // it we simply keep plain READDIR — the other caps above stay applied.
        // Only offered when the provider can page in its own store: PLUS records are
        // ~4x fatter, so they split the same listing into ~4x more kernel buffers,
        // and a provider that re-fetches the whole listing per buffer would pay that
        // multiplier in full round trips instead of saving the per-entry LOOKUPs.
        if self.readdirplus && self.meta.supports_paged_readdir() {
            let _ = config.add_capabilities(
                InitFlags::FUSE_DO_READDIRPLUS | InitFlags::FUSE_READDIRPLUS_AUTO,
            );
        }
        // Advertise a 4 MiB max write/readahead; fuser clamps each to its own
        // MAX_WRITE_SIZE ceiling and the kernel further clamps to FUSE_MAX_MAX_PAGES,
        // so this is the auto-degrade point (5.4 -> 1 MiB/page-capped, 6.x+ -> more).
        let _ = config.set_max_write(4 << 20);
        let _ = config.set_max_readahead(4 << 20);
        // Report the bits the kernel actually granted (config.capabilities() is the
        // kernel-supported mask) so `--stats` operators can see which new-kernel
        // accelerations are live vs silently degraded on this host.
        if crate::stats::enabled() {
            let g = config.capabilities();
            eprintln!(
                "grovefs negotiated caps: PARALLEL_DIROPS={} AUTO_INVAL_DATA={} WRITEBACK={} MAX_PAGES={} (max_write req 4 MiB)",
                g.contains(InitFlags::FUSE_PARALLEL_DIROPS),
                g.contains(InitFlags::FUSE_AUTO_INVAL_DATA),
                g.contains(InitFlags::FUSE_WRITEBACK_CACHE),
                g.contains(InitFlags::FUSE_MAX_PAGES),
            );
        }
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let name_str = name.to_string_lossy().to_string();
        // Synthetic telemetry control node: resolves only under the root and
        // only while `--stats` is active; zero-cost otherwise (one relaxed load).
        if stats::enabled() && parent == INodeNo::ROOT && name_str == STATS_NAME {
            reply.entry(&Duration::from_secs(0), &Self::stats_attr(), Generation(0));
            return;
        }
        match self.meta.lookup(parent.0 as i64, &name_str) {
            Some(node) => {
                if let Some(k) = &node.data_ref {
                    self.cache_key(node.id as u64, k);
                }
                let attr = self.build_attr(&node);
                reply.entry(&self.ttl(), &attr, Generation(0));
            }
            None => reply.error(Errno::ENOENT),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        if stats::enabled() && ino.0 == STATS_INO {
            reply.attr(&Duration::from_secs(0), &Self::stats_attr());
            return;
        }
        match self.meta.getattr(ino.0 as i64) {
            Some(node) => {
                let attr = self.build_attr(&node);
                reply.attr(&self.ttl(), &attr);
            }
            None => reply.error(Errno::ENOENT),
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        if self.readonly {
            reply.error(Errno::EROFS);
            return;
        }
        let _t = stats::start(stats::Stage::MetaSetattr);
        if let Some(m) = mode {
            self.meta.set_mode(ino.0 as i64, m);
        }
        if uid.is_some() || gid.is_some() {
            let node = self.meta.getattr(ino.0 as i64);
            let cur_uid = node
                .as_ref()
                .map(|n| n.uid)
                .unwrap_or(unsafe { libc::getuid() });
            let cur_gid = node
                .as_ref()
                .map(|n| n.gid)
                .unwrap_or(unsafe { libc::getgid() });
            self.meta
                .set_uid_gid(ino.0 as i64, uid.unwrap_or(cur_uid), gid.unwrap_or(cur_gid));
        }
        if let Some(new_size) = size {
            // A fork mounts structure-writable / content-read-only: truncating
            // (or extending) an existing object changes its BYTES, which is
            // exactly what a derived view may not do. Attribute-only setattr
            // (mode/owner/times below) stays allowed.
            if self.content_ro {
                reply.error(Errno::EROFS);
                return;
            }
            // Truncate / extend the backing object (O_TRUNC also lands here).
            let node = self.meta.getattr(ino.0 as i64);
            match node {
                Some(n) if n.is_dir() => {
                    reply.error(Errno::EISDIR);
                    return;
                }
                Some(n) => {
                    if let Some(k) = &n.data_ref {
                        if let Err(e) = self.backend.truncate(k, new_size) {
                            reply.error(errno_of(&e));
                            return;
                        }
                        self.meta.touch_mtime(ino.0 as i64);
                    }
                }
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        }
        // Honor explicit atime/mtime updates (utimes / touch / SET_ATIME etc.).
        let a = time_to_epoch(atime);
        let m = time_to_epoch(mtime);
        if a.is_some() || m.is_some() {
            self.meta.set_times(ino.0 as i64, a, m);
        }
        match self.meta.getattr(ino.0 as i64) {
            Some(node) => reply.attr(&self.short_ttl(), &self.build_attr(&node)),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn symlink(
        &self,
        req: &Request,
        parent: INodeNo,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        if self.readonly {
            reply.error(Errno::EROFS);
            return;
        }
        let name = link_name.to_string_lossy().to_string();
        let tgt = target.to_string_lossy().to_string();
        if name.is_empty() || tgt.is_empty() {
            reply.error(Errno::EINVAL);
            return;
        }
        match self
            .meta
            .create_symlink(parent.0 as i64, &name, &tgt, req.uid(), req.gid())
        {
            Ok(id) => {
                match self.meta.getattr(id) {
                    Some(node) => {
                        reply.entry(&self.short_ttl(), &self.build_attr(&node), Generation(0))
                    }
                    // The just-created inode was concurrently unlinked between
                    // `create` (which released the meta lock) and this lookup.
                    // Report it gone rather than unwrapping `None` into a
                    // FUSE-worker panic; matches getattr/lookup/setattr.
                    None => reply.error(Errno::ENOENT),
                }
            }
            Err(e) => reply.error(Self::err_to_errno(e)),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        match self.meta.getattr(ino.0 as i64) {
            Some(n) if n.kind == NodeKind::Symlink => {
                reply.data(n.link_target.unwrap_or_default().as_bytes());
            }
            Some(_) => reply.error(Errno::EINVAL),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn link(
        &self,
        _req: &Request,
        ino: INodeNo,
        newparent: INodeNo,
        newname: &OsStr,
        reply: ReplyEntry,
    ) {
        if self.readonly {
            reply.error(Errno::EROFS);
            return;
        }
        let name = newname.to_string_lossy().to_string();
        match self.meta.link_node(ino.0 as i64, newparent.0 as i64, &name) {
            Ok(id) => {
                match self.meta.getattr(id) {
                    Some(node) => {
                        reply.entry(&self.short_ttl(), &self.build_attr(&node), Generation(0))
                    }
                    // The just-created inode was concurrently unlinked between
                    // `create` (which released the meta lock) and this lookup.
                    // Report it gone rather than unwrapping `None` into a
                    // FUSE-worker panic; matches getattr/lookup/setattr.
                    None => reply.error(Errno::ENOENT),
                }
            }
            Err(e) => reply.error(Self::err_to_errno(e)),
        }
    }

    fn mknod(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        rdev: u32,
        reply: ReplyEntry,
    ) {
        // We only materialize regular files via mknod (S_IFREG). Special files
        // (fifo/char/block/socket) are not backed by a data object here.
        if self.readonly {
            reply.error(Errno::EROFS);
            return;
        }
        let filetype = mode & libc::S_IFMT;
        if filetype != libc::S_IFREG {
            // fifo/char/block/socket have no data object backing in this fs.
            reply.error(Errno::EPERM);
            return;
        }
        let _ = rdev;
        let name_str = name.to_string_lossy().to_string();
        // The durable data_ref is backend-decided: single-object backends return
        // the bare uuid (unchanged behavior), a sharded backend encodes its
        // stripe layout so reads are self-describing.
        let base = uuid::Uuid::new_v4().to_string();
        let key = self.backend.new_key(&base);
        if let Err(e) = self.backend.create(&key) {
            reply.error(errno_of(&e));
            return;
        }
        let perm = mode & !umask & 0o7777;
        match self.meta.create_node(
            parent.0 as i64,
            &name_str,
            false,
            Some(&key),
            perm,
            req.uid(),
            req.gid(),
        ) {
            Ok(id) => {
                match self.meta.getattr(id) {
                    Some(node) => {
                        self.cache_key(id as u64, &key);
                        reply.entry(&self.short_ttl(), &self.build_attr(&node), Generation(0));
                    }
                    // Raced with a concurrent unlink of the just-created name:
                    // reply ENOENT instead of panicking a FUSE worker on `None`
                    // (the unlink already GC'd the backing object via gced_key).
                    None => reply.error(Errno::ENOENT),
                }
            }
            Err(e) => {
                let _ = self.backend.remove(&key);
                reply.error(Self::err_to_errno(e));
            }
        }
    }

    fn mkdir(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        if self.readonly {
            reply.error(Errno::EROFS);
            return;
        }
        let name_str = name.to_string_lossy().to_string();
        let perm = mode & !umask;
        match self.meta.create_node(
            parent.0 as i64,
            &name_str,
            true,
            None,
            perm,
            req.uid(),
            req.gid(),
        ) {
            Ok(id) => {
                match self.meta.getattr(id) {
                    Some(node) => {
                        reply.entry(&self.short_ttl(), &self.build_attr(&node), Generation(0))
                    }
                    // The just-created inode was concurrently unlinked between
                    // `create` (which released the meta lock) and this lookup.
                    // Report it gone rather than unwrapping `None` into a
                    // FUSE-worker panic; matches getattr/lookup/setattr.
                    None => reply.error(Errno::ENOENT),
                }
            }
            Err(e) => reply.error(Self::err_to_errno(e)),
        }
    }

    fn create(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        if self.readonly {
            reply.error(Errno::EROFS);
            return;
        }
        let name_str = name.to_string_lossy().to_string();
        let base = uuid::Uuid::new_v4().to_string();
        let key = self.backend.new_key(&base);
        // Materialize the object first so a failed create doesn't leave a dangling node.
        if let Err(e) = self.backend.create(&key) {
            reply.error(errno_of(&e));
            return;
        }
        let perm = mode & !umask;
        match self.meta.create_node(
            parent.0 as i64,
            &name_str,
            false,
            Some(&key),
            perm,
            req.uid(),
            req.gid(),
        ) {
            Ok(id) => {
                match self.meta.getattr(id) {
                    Some(node) => {
                        self.cache_key(id as u64, &key);
                        let attr = self.build_attr(&node);
                        // Echo the inode as the handle: IO resolves by inode, so fh is
                        // opaque here. Returning it keeps the reply shape valid.
                        reply.created(
                            &self.short_ttl(),
                            &attr,
                            Generation(0),
                            FileHandle(id as u64),
                            FopenFlags::empty(),
                        );
                    }
                    // Raced with a concurrent unlink of the just-created name:
                    // reply ENOENT instead of panicking a FUSE worker on `None`
                    // (the unlink already GC'd the backing object via gced_key).
                    None => reply.error(Errno::ENOENT),
                }
            }
            Err(e) => {
                let _ = self.backend.remove(&key);
                reply.error(Self::err_to_errno(e));
            }
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        if stats::enabled() && ino.0 == STATS_INO {
            // Read-only virtual handle; no backend object is bound to it.
            reply.opened(FileHandle(STATS_INO), FopenFlags::empty());
            return;
        }
        match self.meta.getattr(ino.0 as i64) {
            Some(node) if !node.is_dir() => {
                if let Some(k) = &node.data_ref {
                    self.cache_key(ino.0, k);
                }
                // Handle is the inode; read/write resolve the object from it.
                reply.opened(FileHandle(ino.0), self.open_flags());
            }
            Some(_) => reply.error(Errno::EISDIR),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        if stats::enabled() && ino.0 == STATS_INO {
            reply.data(&Self::stats_slice(offset, size as usize));
            return;
        }
        // Outermost per-op wall clock: the real end-to-end "read speed" the
        // kernel sees, so `read.op_total` minus (key_resolve + backend_total)
        // exposes the FUSE reply-serialization / dispatch overhead the sub-stages
        // deliberately leave out. Placed after the control-node branch so reading
        // `.grove-stats` itself does not pollute the data-read distribution.
        let _op = stats::start(stats::Stage::OpRead);
        let key = {
            let _t = stats::start(stats::Stage::ReadKeyResolve);
            match self.key_for_ino(ino.0) {
                Some(k) => k,
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };
        // Time the backend fetch on its own so the `reply.data` serialization
        // back into the FUSE buffer (a different cost) is not folded in.
        let data = {
            let _t = stats::start(stats::Stage::ReadBackendTotal);
            match self.backend.read(&key, offset, size as usize) {
                Ok(d) => {
                    stats::add_bytes(stats::Stage::ReadBackendTotal, d.len() as u64);
                    stats::count(stats::Counter::BytesRead, d.len() as u64);
                    d
                }
                Err(e) => {
                    reply.error(errno_of(&e));
                    return;
                }
            }
        };
        // Book this reply in the mount's shared read schedule BEFORE handing
        // bytes back, so bytes leave at no more than the configured rate.
        // Charged on the bytes actually returned — an EOF read costs nothing.
        // A throttled worker sleeps; other workers keep dispatching.
        self.bw_read.acquire(data.len() as u64);
        reply.data(&data);
    }

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        if self.readonly {
            reply.error(Errno::EROFS);
            return;
        }
        if self.content_ro {
            // Fork view: the tree is the client's to organize, its bytes are not.
            reply.error(Errno::EROFS);
            return;
        }
        // Outermost per-op write latency (kernel-visible "write speed"); the gap
        // to (key_resolve + backend_total + touch_mtime) is unmeasured glue.
        let _op = stats::start(stats::Stage::OpWrite);
        let key = {
            let _t = stats::start(stats::Stage::WriteKeyResolve);
            match self.key_for_ino(ino.0) {
                Some(k) => k,
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };
        let n = {
            let _t = stats::start(stats::Stage::WriteBackendTotal);
            match self.backend.write(&key, offset, data) {
                Ok(n) => {
                    stats::add_bytes(stats::Stage::WriteBackendTotal, n as u64);
                    stats::count(stats::Counter::BytesWritten, n as u64);
                    n
                }
                Err(e) => {
                    reply.error(errno_of(&e));
                    return;
                }
            }
        };
        // Charge this mount's write budget for the bytes actually persisted:
        // the FUSE reply (and therefore the app's write/flush) is delayed by
        // any deficit, which backpressures the writer instead of dropping data.
        self.bw_write.acquire(n as u64);
        // Debounced mtime touch: a streaming writer fires one write
        // callback per 1MiB chunk, and each unhedged touch is a full
        // metadata write (a local tx, or — with --meta — a synchronous
        // RPC round-trip and one row in the HA replication stream).
        // One touch per WRITE_TOUCH_DEBOUNCE window per inode bounds
        // that amplification. Honest cost: mtime granularity degrades
        // to ~the window (200ms); the setattr path (truncate, explicit
        // utimes/touch) is NOT debounced because those semantics require
        // immediacy.
        {
            let _t = stats::start(stats::Stage::WriteTouchMtime);
            if self.should_touch_write(ino.0) {
                self.meta.touch_mtime(ino.0 as i64);
            }
        }
        reply.written(n as u32);
    }

    fn fsync(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        match self.key_for_ino(ino.0) {
            Some(k) => match self.backend.fsync(&k) {
                Ok(()) => reply.ok(),
                Err(e) => reply.error(errno_of(&e)),
            },
            None => reply.error(Errno::ENOENT),
        }
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        // Intentionally keep the ino->key entry: with writeback cache the kernel
        // may still flush this inode's dirty pages after close. `forget` prunes it
        // once the inode is truly dead.
        //
        // FUSE delivers `release` exactly once, when the file description's LAST
        // reference closes — on normal exit *and* on `SIGKILL` (the kernel still
        // tears the fd down). flock(2) passthrough makes the request carry a
        // `lock_owner` (only under `FUSE_RELEASE_FLOCK_UNLOCK`), so that is the
        // authoritative "holder is gone" signal for flock locks: drop its rows
        // immediately rather than after a lease timeout.
        if let Some(o) = lock_owner {
            self.meta.release_locks_of_owner(o.0);
        }
        // POSIX `fcntl` locks arrive with NO `lock_owner` (the kernel sets
        // `FUSE_RELEASE_FLOCK_UNLOCK` only for flock), so the branch above
        // cannot see them. Reclaim them from the `(fh -> ino, owner)` bookkeeping
        // instead, when the last locking description of that pair closes (§5-3).
        self.drop_posix_lock_on_release(fh.0);
        reply.ok();
    }

    /// close(2) lands here. Every write was already persisted synchronously in
    /// `write()` — this layer holds no user-side buffering — so an empty flush
    /// SUCCEEDS, which is the correct POSIX answer. The trait default replies
    /// `ENOSYS`, which surfaces to applications as a spurious `close()` failure
    /// (and logs a WARN). Lock teardown deliberately does NOT happen here: a
    /// `flush` fires on EVERY close and its `lock_owner` need not match the
    /// `setlk` holder, so reclamation stays with last-close `release` above
    /// (the authoritative signal).
    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    /// The four callbacks below keep the trait defaults' exact `ENOSYS` answers
    /// — behavior is unchanged to the bit — and only suppress the defaults'
    /// `warn!("[Not Implemented] …")`, so a healthy mount leaves no log noise:
    ///   * `access`: permission checks run kernel-side off the cached getattr
    ///     attributes unless `default_permissions`; ENOSYS is the normal answer
    ///     for a FUSE fs without an explicit access hook.
    ///   * xattr ops: the view stores no extended attributes, so every verb
    ///     answers ENOSYS and libfuse's `security.capability` probe falls back
    ///     cleanly (and silently) to the plain attribute path.
    ///   * `ioctl`: no device ioctls live behind a networked object store.
    fn access(&self, _req: &Request, _ino: INodeNo, _mask: AccessFlags, reply: ReplyEmpty) {
        reply.error(Errno::ENOSYS);
    }

    fn getxattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _name: &OsStr,
        _size: u32,
        reply: ReplyXattr,
    ) {
        reply.error(Errno::ENOSYS);
    }

    fn setxattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _name: &OsStr,
        _value: &[u8],
        _flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        reply.error(Errno::ENOSYS);
    }

    fn removexattr(&self, _req: &Request, _ino: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(Errno::ENOSYS);
    }

    fn listxattr(&self, _req: &Request, _ino: INodeNo, _size: u32, reply: ReplyXattr) {
        reply.error(Errno::ENOSYS);
    }

    #[allow(clippy::too_many_arguments)]
    fn ioctl(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _flags: IoctlFlags,
        _cmd: u32,
        _in_data: &[u8],
        _out_size: u32,
        reply: ReplyIoctl,
    ) {
        reply.error(Errno::ENOSYS);
    }

    /// F_GETLK: report the first lock currently blocking the requested interval
    /// without mutating state. Reads are allowed even on `--ro` mounts (advisory
    /// locks do not modify file bytes).
    #[allow(clippy::too_many_arguments)]
    fn getlk(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        lock_owner: LockOwner,
        start: u64,
        end: u64,
        typ: i32,
        _pid: u32,
        reply: ReplyLock,
    ) {
        let ty = match LockType::from_fuse_i32(typ) {
            Some(t) if t != LockType::Unlock => t,
            _ => {
                // F_GETLK with F_UNLCK is meaningless; report no conflict.
                reply.locked(0, 0, 2 /* F_UNLCK */, 0);
                return;
            }
        };
        match self
            .meta
            .get_lock(ino.0 as i64, lock_owner.0, start, end, ty)
        {
            Some(c) => reply.locked(c.start, c.end, c.ty.to_fuse_i32(), c.pid),
            None => reply.locked(0, 0, 2 /* F_UNLCK */, 0),
        }
    }

    /// F_SETLK / F_SETLKW / flock(2): acquire, modify, or release a POSIX
    /// advisory record lock. Kernel multiplexes both flavors through this
    /// single callback with `typ` already translated to F_RDLCK/F_WRLCK/
    /// F_UNLCK and the whole-file range for flock.
    #[allow(clippy::too_many_arguments)]
    fn setlk(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        lock_owner: LockOwner,
        start: u64,
        end: u64,
        typ: i32,
        pid: u32,
        sleep: bool,
        reply: ReplyEmpty,
    ) {
        // Locks are metadata writes (INSERT/DELETE on the `locks` table).
        // Refuse them on a `--ro` mount rather than half-succeeding — an
        // advisory lock that isn't actually stored would silently let peers
        // believe the file is unlocked.
        if self.readonly && typ != LockType::Unlock.to_fuse_i32() {
            reply.error(Errno::EROFS);
            return;
        }
        let ty = match LockType::from_fuse_i32(typ) {
            Some(t) => t,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };
        // Blocking acquire (F_SETLKW / flock without LOCK_NB): poll the
        // authority until granted or until our bounded wait expires. A truly
        // POSIX-perfect implementation would be event-driven (server pushes a
        // wake-up when the holder releases); that's listed as evolution in
        // the plan. This path is off the data hot loop so a 100ms cadence is
        // fine and keeps the meta protocol simple.
        const BLOCKING_POLL_MS: u64 = 100;
        const BLOCKING_WAIT_MAX_MS: u64 = 600_000; // 10 min safety cap
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(BLOCKING_WAIT_MAX_MS);
        loop {
            match self.meta.lock_op(
                ino.0 as i64,
                lock_owner.0,
                start,
                end,
                ty,
                pid,
                self.lock_sid,
            ) {
                Ok(None) => {
                    // A granted Read/Write lock must be kept alive: start the
                    // per-mount renewal heartbeat (idempotent) so peers see it
                    // held for exactly as long as the holder's file description
                    // stays open. Record the `(fh -> ino, owner)` so `release`
                    // can drop it: the kernel never hands `release` a
                    // `lock_owner` for POSIX locks (only flock(2)), so without
                    // this a `close`/exit-released lock would be renewed forever
                    // and hang blocked waiters (§5-3).
                    if ty != LockType::Unlock {
                        self.ensure_lock_renewer();
                        self.note_posix_lock(fh.0, ino.0 as i64, lock_owner.0);
                    }
                    reply.ok();
                    return;
                }
                Ok(Some(_conflict)) => {
                    if !sleep {
                        // F_SETLK non-blocking / flock LOCK_NB: report would-block.
                        reply.error(Errno::EACCES);
                        return;
                    }
                    if std::time::Instant::now() >= deadline {
                        reply.error(Errno::ETIMEDOUT);
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(BLOCKING_POLL_MS));
                    continue;
                }
                Err(MetaError::Db(s)) => {
                    // Read-only open on HA standby surfaces here; report EROFS.
                    if s.to_lowercase().contains("readonly")
                        || s.to_lowercase().contains("read-only")
                    {
                        reply.error(Errno::EROFS);
                    } else {
                        reply.error(Errno::EIO);
                    }
                    return;
                }
                Err(_) => {
                    reply.error(Errno::EIO);
                    return;
                }
            }
        }
    }

    fn forget(&self, _req: &Request, ino: INodeNo, _nlookup: u64) {
        self.ino_to_key.write().remove(&ino.0);
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        if self.readonly {
            reply.error(Errno::EROFS);
            return;
        }
        let name_str = name.to_string_lossy().to_string();
        match self.meta.unlink(parent.0 as i64, &name_str) {
            Ok(gced_key) => {
                if let Some(k) = gced_key {
                    stats::count(stats::Counter::GcFreed, 1);
                    let _ = self.backend.remove(&k);
                }
                reply.ok();
            }
            Err(e) => reply.error(Self::err_to_errno(e)),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.unlink(_req, parent, name, reply);
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        if self.readonly {
            reply.error(Errno::EROFS);
            return;
        }
        // RENAME_EXCHANGE (swap two names) and RENAME_WHITEOUT (leave a placeholder)
        // need semantics we do not implement. Silently running them as a plain
        // overwrite would destroy data the caller expected to keep/swap, so fail
        // loudly instead. NOREPLACE *is* supported (honored in the engine).
        if flags.intersects(RenameFlags::RENAME_EXCHANGE | RenameFlags::RENAME_WHITEOUT) {
            reply.error(Errno::ENOSYS);
            return;
        }
        let noreplace = flags.contains(RenameFlags::RENAME_NOREPLACE);
        let from = name.to_string_lossy().to_string();
        let to = newname.to_string_lossy().to_string();
        match self
            .meta
            .rename(parent.0 as i64, &from, newparent.0 as i64, &to, noreplace)
        {
            Ok(gc) => {
                // Renaming over an existing file drops that file's last name; free
                // its storage blob so the space does not leak (mirrors unlink).
                if let Some(k) = gc {
                    stats::count(stats::Counter::GcFreed, 1);
                    let _ = self.backend.remove(&k);
                }
                reply.ok();
            }
            Err(e) => reply.error(Self::err_to_errno(e)),
        }
    }

    /// READDIR_PLUS: same deterministic listing as `readdir`, but each entry
    /// carries its full attributes (taken straight from the `NodeInfo` the
    /// engine already returned — zero extra metadata queries), so the kernel
    /// inserts dentry+attr in one shot and `ls -l` skips the per-name
    /// LOOKUP/GETATTR storm.
    fn readdirplus(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectoryPlus,
    ) {
        // Resume at the control node's cookie -> the listing already ended -> EOF.
        if stats::enabled() && ino == INodeNo::ROOT && offset == STATS_COOKIE {
            reply.ok();
            return;
        }
        let node = match self.meta.getattr(ino.0 as i64) {
            Some(n) if n.is_dir() => n,
            Some(_) => {
                reply.error(Errno::ENOTDIR);
                return;
            }
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        // "." / ".." first, then children in ORDER BY name (same resume semantics
        // as `readdir`; the two share the kernel-side offset indexing).
        let ttl = self.ttl();
        let self_attr = self.build_attr(&node);
        let skip = offset as usize;
        if skip == 0 && reply.add(ino, 1, ".", &ttl, &self_attr, Generation(0)) {
            reply.ok();
            return;
        }
        let parent_ino = if ino == INodeNo::ROOT {
            ino.0
        } else {
            node.parent_id as u64
        };
        let parent_attr = match self.meta.getattr(parent_ino as i64) {
            Some(p) => self.build_attr(&p),
            None => self_attr,
        };
        if skip <= 1
            && reply.add(
                INodeNo(parent_ino),
                2,
                "..",
                &ttl,
                &parent_attr,
                Generation(0),
            )
        {
            reply.ok();
            return;
        }
        // Children are streamed a SQL page at a time until the reply buffer
        // fills or the listing ends, so one kernel READDIRPLUS request costs
        // O(what it can hold) instead of O(whole directory). `child_i` indexes
        // the ORDER BY name sequence, so the reported offset (`child_i + 2`, the
        // two slots taken by "." and "..") is exactly the offset the previous
        // full-scan implementation reported for the same entry.
        let mut child_i = skip.saturating_sub(2);
        let (mut want, want_max) = self.fetch_policy();
        loop {
            let page = self.meta.readdir_page(ino.0 as i64, child_i, want);
            let got = page.len();
            for c in page {
                if let Some(k) = &c.data_ref {
                    self.cache_key(c.id as u64, k);
                }
                let attr = self.build_attr(&c);
                let idx = (child_i + 2) as u64;
                if reply.add(
                    INodeNo(c.id as u64),
                    idx + 1,
                    OsStr::new(&c.name),
                    &ttl,
                    &attr,
                    Generation(0),
                ) {
                    reply.ok();
                    return; // buffer full; kernel resumes at offset = idx + 1
                }
                child_i += 1;
            }
            if got < want {
                break; // short page: the directory is exhausted
            }
            want = want.saturating_mul(2).min(want_max);
        }
        // Append the synthetic telemetry row once at the end of the root
        // listing (see `readdir`), leaving the real-children path unchanged.
        if stats::enabled() && ino == INodeNo::ROOT {
            let sattr = Self::stats_attr();
            let sttl = Duration::from_secs(0);
            if reply.add(
                INodeNo(STATS_INO),
                STATS_COOKIE,
                OsStr::new(STATS_NAME),
                &sttl,
                &sattr,
                Generation(0),
            ) {
                reply.ok();
                return;
            }
        }
        reply.ok();
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        // A resume at the control node's cookie means the listing (including the
        // synthetic row) already ended -> EOF. Guards against re-emitting it.
        if stats::enabled() && ino == INodeNo::ROOT && offset == STATS_COOKIE {
            reply.ok();
            return;
        }
        let node = match self.meta.getattr(ino.0 as i64) {
            Some(n) if n.is_dir() => n,
            Some(_) => {
                reply.error(Errno::ENOTDIR);
                return;
            }
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        // Deterministic listing: ".", ".." then children (SQL already ORDER BY
        // name), so index-based resume stays stable across calls. Children are
        // streamed through `readdir_page` in chunks (see `readdirplus`), and the
        // offsets match the READDIRPLUS ones, which the kernel may interleave
        // with these on the same directory handle.
        let skip = offset as usize;
        let parent_ino = if ino == INodeNo::ROOT {
            ino.0
        } else {
            node.parent_id as u64
        };
        if skip <= 1 {
            if skip == 0 && reply.add(ino, 1, FileType::Directory, OsStr::new(".")) {
                reply.ok();
                return;
            }
            if reply.add(
                INodeNo(parent_ino),
                2,
                FileType::Directory,
                OsStr::new(".."),
            ) {
                reply.ok();
                return;
            }
        }
        let mut child_i = skip.saturating_sub(2);
        let (mut want, want_max) = self.fetch_policy();
        loop {
            let page = self.meta.readdir_page(ino.0 as i64, child_i, want);
            let got = page.len();
            for c in page {
                let kind = match c.kind {
                    NodeKind::Dir => FileType::Directory,
                    NodeKind::Symlink => FileType::Symlink,
                    NodeKind::Regular => FileType::RegularFile,
                };
                let idx = (child_i + 2) as u64;
                if reply.add(INodeNo(c.id as u64), idx + 1, kind, OsStr::new(&c.name)) {
                    reply.ok();
                    return; // buffer full; kernel re-requests at offset = idx + 1
                }
                child_i += 1;
            }
            if got < want {
                break;
            }
            want = want.saturating_mul(2).min(want_max);
        }
        // Root gets the synthetic telemetry row appended once, after the real
        // children are exhausted, occupying one trailing slot with its own
        // opaque cookie. This leaves the real-children enumeration byte-for-byte
        // unchanged and never runs when `--stats` is off.
        if stats::enabled()
            && ino == INodeNo::ROOT
            && reply.add(
                INodeNo(STATS_INO),
                STATS_COOKIE,
                FileType::RegularFile,
                OsStr::new(STATS_NAME),
            )
        {
            reply.ok();
            return;
        }
        reply.ok();
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        if self
            .meta
            .getattr(ino.0 as i64)
            .map(|n| n.is_dir())
            .unwrap_or(false)
        {
            reply.opened(FileHandle(0), FopenFlags::empty());
        } else {
            reply.error(Errno::ENOTDIR);
        }
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        let _t = stats::start(stats::Stage::MetaStatfs);
        match self.backend.statvfs() {
            Ok(s) => {
                let bsize = s.bsize.max(512) as u32;
                reply.statfs(
                    s.tblocks,
                    s.bfree,
                    s.bavail,
                    s.files,
                    s.ffree,
                    bsize,
                    s.namemax.max(255),
                    bsize, // frsize: block size in units for f_frsize
                );
            }
            Err(e) => reply.error(errno_of(&e)),
        }
    }
}

/// Debounce window for the write-path mtime touch (see `GroveFS::write`).
const WRITE_TOUCH_DEBOUNCE: Duration = Duration::from_millis(200);

/// First (smallest) page of children fetched per metadata query while
/// streaming a READDIR/READDIRPLUS reply, and the cap it grows to. fuser does
/// not expose how much room is left in the reply buffer, so the fetch size
/// doubles on each refill: the kernel asks for 4 KiB READDIRPLUS buffers (~23
/// fat records), where a fixed large chunk would waste most of every query,
/// while a geometric run costs at most about one extra chunk per buffer — and
/// still adapts when the kernel does ask for a big one.
const READDIR_FETCH_MIN: usize = 32;
const READDIR_FETCH_MAX: usize = 1024;

/// True if `ino`'s write-path mtime should be touched now: either never
/// touched since the last prune, or the debounce window has elapsed. Updates
/// the timestamp map when firing. Split out of `write()` so it is unit-testable
/// with an injected clock.
fn should_touch_write(
    map: &mut std::collections::HashMap<u64, SystemTime>,
    ino: u64,
    now: SystemTime,
) -> bool {
    let fire = match map.get(&ino) {
        Some(&last) => now.duration_since(last).unwrap_or(Duration::ZERO) >= WRITE_TOUCH_DEBOUNCE,
        None => true,
    };
    if fire {
        map.insert(ino, now);
    }
    fire
}

fn errno_of(e: &io::Error) -> Errno {
    // The backend reports raw OS errnos; `from_i32` falls back to EIO for
    // non-errno errors (e.g. synthesized io::Error::other).
    Errno::from_i32(e.raw_os_error().unwrap_or(0))
}

/// Convert a fuser TimeOrNow into an epoch-seconds value for the metadata store.
fn time_to_epoch(t: Option<TimeOrNow>) -> Option<i64> {
    match t? {
        TimeOrNow::Now => Some(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
        ),
        TimeOrNow::SpecificTime(st) => st
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|d| d.as_secs() as i64),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The write-path touch debounce: first write to an inode fires, repeats
    /// inside the window are suppressed, the next one after the window fires
    /// again — and different inodes keep independent windows.
    #[test]
    fn write_touch_debounces_per_inode() {
        let mut map = std::collections::HashMap::new();
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        assert!(
            should_touch_write(&mut map, 7, t0),
            "first write always touches"
        );
        assert!(
            !should_touch_write(&mut map, 7, t0 + Duration::from_millis(50)),
            "50ms later is inside the window"
        );
        assert!(
            !should_touch_write(&mut map, 7, t0 + Duration::from_millis(199)),
            "199ms is still inside"
        );
        // A suppressed read of the window must not move the deadline:
        assert!(
            should_touch_write(&mut map, 7, t0 + Duration::from_millis(200)),
            "exactly the window elapsed fires (and re-stamps)"
        );
        // Separate inode has its own window (new file while streaming another).
        assert!(should_touch_write(
            &mut map,
            8,
            t0 + Duration::from_millis(201)
        ));
    }

    /// Regression guard for the amplification math the P2 claim rests on:
    /// 1000 write callbacks spread over 10s must collapse to ~the window count,
    /// not 1000 metadata writes.
    #[test]
    fn write_touch_collapses_streaming_writes() {
        let mut map = std::collections::HashMap::new();
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let fires = (0..1000)
            .filter(|i| should_touch_write(&mut map, 1, t0 + Duration::from_millis(10 * i)))
            .count();
        // 1000 callbacks at 10ms spacing span 0..9990ms; each fire re-stamps at
        // its own instant, so touches land at 0,200,...,9800 -> exactly 50.
        assert_eq!(fires, 50, "1000 x 10ms writes fired {fires} touches");
    }
}

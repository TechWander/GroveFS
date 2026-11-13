//! Metadata engine: the per-view **view tree** that makes "different users see
//! different file lists" possible, plus custom folder hierarchy.
//!
//! The physical bytes live in the [`crate::backend::StorageBackend`]; this
//! module only stores *structure*: which nodes exist, how they nest, and what a
//! file node points to. Node kinds:
//! - directory          : is_dir=1
//! - regular file       : is_dir=0, data_ref=Some(key), link_target=None
//! - symbolic link      : is_dir=0, data_ref=None,   link_target=Some(path)
//!
//! Hard links share one `data_ref`; inside a view the object is GC'd only when
//! the last node referencing it disappears (see [`MetaEngine::unlink`]), and
//! ACROSS views a shared key (two views holding names for one `data_ref`, e.g.
//! the same source imported twice) is guarded by the cross-view holder ledger
//! in `<base>/meta/refs.db` (see the ledger section below) — a blob is only
//! physically freeable when its last name in a view goes AND no other view
//! holds it. Because a client only ever queries its own view DB, files it
//! cannot see are never materialized on the network — isolation is enforced
//! here, not by permission bits.
//!
//! Inode model: a FUSE inode number == `node.ino`. Root is always ino 1.
//! Multiple directory entries (rows) may share one `ino` — that *is* a hard
//! link: they name the same inode and therefore the same data object, the same
//! size and the same timestamps. Fresh files/dirs get `ino == row id`.

// The client-facing half of this module (types + [`MetaProvider`] + [`MetaError`])
// needs only `std::sync::Arc` for the fence-handle default, referenced inline as
// `std::sync::Arc`. The heavy `rusqlite`/`serde_json`/`std::fs` imports live with
// the native-only `engine` submodule below.

pub const ROOT_INO: i64 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Dir,
    Regular,
    Symlink,
}

/// POSIX record lock type (matches FUSE `l_type` after kernel translation for
/// both `fcntl F_SETLK` and BSD `flock(2)` — the kernel collapses them into
/// F_RDLCK/F_WRLCK/F_UNLCK before sending FUSE_LK).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockType {
    /// Shared (F_RDLCK / LOCK_SH).
    Read,
    /// Exclusive (F_WRLCK / LOCK_EX).
    Write,
    /// Release the interval for this owner (F_UNLCK / LOCK_UN).
    Unlock,
}

impl LockType {
    /// Wire code used by the meta protocol and the SQLite `ltype` column.
    /// Kept stable so primary/standby snapshots interop across versions.
    pub fn to_u8(self) -> u8 {
        match self {
            LockType::Read => 0,
            LockType::Write => 1,
            LockType::Unlock => 2,
        }
    }
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(LockType::Read),
            1 => Some(LockType::Write),
            2 => Some(LockType::Unlock),
            _ => None,
        }
    }
    /// Wire value for `struct flock.l_type` (kernel FUSE passes it through).
    pub fn from_fuse_i32(v: i32) -> Option<Self> {
        // F_RDLCK=0, F_WRLCK=1, F_UNLCK=2 on Linux.
        match v {
            0 => Some(LockType::Read),
            1 => Some(LockType::Write),
            2 => Some(LockType::Unlock),
            _ => None,
        }
    }
    pub fn to_fuse_i32(self) -> i32 {
        match self {
            LockType::Read => 0,
            LockType::Write => 1,
            LockType::Unlock => 2,
        }
    }
}

/// The lock currently blocking our requested interval, reported by F_GETLK
/// so the caller can render `struct flock` for the application.
#[derive(Debug, Clone, Copy)]
pub struct LockConflict {
    pub start: u64,
    pub end: u64,
    pub ty: LockType,
    pub pid: u32,
}

/// Default POSIX advisory-lock lease. A lock row's `expires_at` is refreshed by
/// the owning mount daemon's renewal heartbeat (`renew_locks`) every
/// `secs()/3`, so a *live* holder never hits this deadline. The lease is the
/// backstop for a holder that died WITHOUT renewing (daemon crash, network
/// partition, SIGKILL before `release`): with renewal in place the lease can be
/// short — 15s bounds the post-crash dangling window (down from the old 60s)
/// while a healthy holder is invisible to it. Operators/tests may override via
/// `GROVEFS_LOCK_LEASE_SECS`.
pub const LOCK_LEASE_SECS: i64 = 15;

/// Runtime lease knob. The default is `LOCK_LEASE_SECS`. Test suites and
/// operators may lower/raise it by exporting `GROVEFS_LOCK_LEASE_SECS=<secs>`
/// before spawning `mount` / `meta-server` — the CLI surface stays unchanged
/// (an internal knob, not a supported tuning), so e2e tests can exercise the
/// SIGKILL-then-expire path without sleeping 60s. Cached on first read.
pub struct LockLease;

impl LockLease {
    fn slot() -> &'static std::sync::atomic::AtomicI64 {
        static V: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(-1);
        &V
    }
    pub fn secs() -> i64 {
        let cur = Self::slot().load(std::sync::atomic::Ordering::Relaxed);
        if cur > 0 {
            return cur;
        }
        let v = std::env::var("GROVEFS_LOCK_LEASE_SECS")
            .ok()
            .and_then(|s| s.parse::<i64>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(LOCK_LEASE_SECS);
        Self::slot().store(v, std::sync::atomic::Ordering::Relaxed);
        v
    }
}

#[derive(Debug, Clone)]
pub struct NodeInfo {
    pub id: i64,
    pub parent_id: i64,
    pub name: String,
    pub kind: NodeKind,
    pub data_ref: Option<String>,
    pub link_target: Option<String>,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub mtime: i64,
    pub atime: i64,
    pub ctime: i64,
    pub nlink: u64,
    pub size: u64,
}

impl NodeInfo {
    pub fn is_dir(&self) -> bool {
        self.kind == NodeKind::Dir
    }
}

// ===================== cross-view blob reference ledger ====================
//
// A `data_ref` names ONE backend object, but several physical views can hold
// names for the same key: `grovefs import` of the same source file into two
// views resolves to the same content key, and an operator can seed two trees
// over shared objects. The per-view `node.data_ref` refcount can only see names
// inside its own db — left alone, a delete in view A would hand the caller a
// freed key and GC the bytes view B still lists. So bookkeeping lives beside
// the view trees, in `<base>/meta/refs.db`: one row per data_ref that SOME view
// holds with its last name, counting how many VIEWS hold it. A blob is only
// physically freeable when its last name in a view is removed AND no other view
// holds a name (see [`ledger_remove`]).
//
// Mechanics: every view engine ATTACHes the sibling `refs.db` as schema `refs`
// at open, so the ledger row moves inside the SAME IMMEDIATE transaction as the
// node INSERT/DELETE it books. Both files lock at BEGIN and commit together, but
// note SQLite's own caveat: with the main db in WAL mode a transaction spanning
// attached dbs is atomic per database, NOT as a set (no master journal). A crash
// inside that window can therefore land the node row without the holder row;
// `grovefs refs rebuild` is the repair path (see docs §13.3).
// Deadlock control: `BEGIN IMMEDIATE` takes the write lock of EVERY attached
// db at start, so each ledger-touching txn locks `refs.db` first and its view
// db second in one fixed global order — cycles are structurally impossible
// even with WAL's non-detecting busy handler; `busy_timeout` just queues the
// losers. Within one process several engines (several mutexes) share refs.db,
// so ledger writers additionally take [`LEDGER_SERIAL`] BEFORE their engine
// lock (a process-wide order; cross-process order is the db-lock order).
//
// Staleness boundary: a view db deleted out-of-band (`rm <view>.db`, a fresh
// `--home`) never decrements its holders — the ledger over-counts, blobs are
// kept too long, and `grovefs refs rebuild` rewrites the ledger from the trees
// that actually exist. A key with no ledger row is simply private to one view
// (no other view known -> free on last name).

/// The shared ledger file beside the view trees (`<meta dir>/refs.db`).
pub const REFS_DB: &str = "refs.db";
/// The schema name every engine connection ATTACHes the ledger under.
pub const REFS_SCHEMA: &str = "refs";

/// One `CREATE` request in a batch (see [`MetaProvider::create_nodes_batch`]).
///
/// Borrowed, because the caller keeps the owned names in its queue and only
/// lends them to the store for the duration of one commit — no copies on the
/// hot path.
#[derive(Clone, Copy)]
pub struct NewNode<'a> {
    pub parent: i64,
    pub name: &'a str,
    pub is_dir: bool,
    pub data_ref: Option<&'a str>,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
}

/// One view's tree as the FUSE layer sees it: name/attribute/listing operations
/// over a per-view directory tree.
///
/// Implemented locally by [`MetaEngine`] (a direct per-view SQLite file) and
/// remotely by [`crate::remote::meta::RemoteMeta`] (TCP to a central
/// `MetaServer`). The same FUSE code therefore drives either a view that lives
/// only on one machine or a *single authoritative* view shared by many clients
/// over the LAN, without knowing which — this is what makes the same view's
/// folder tree consistent across machines.
pub trait MetaProvider: Send + Sync {
    fn lookup(&self, parent: i64, name: &str) -> Option<NodeInfo>;
    fn getattr(&self, ino: i64) -> Option<NodeInfo>;
    fn readdir(&self, parent: i64) -> Vec<NodeInfo>;
    /// Extra KERNEL-cache coordinates a change at the SOURCE coordinate
    /// `(src_parent, name)` implies for this provider's display namespace.
    ///
    /// A push invalidation from a `meta-server` always names the real source
    /// `(parent inode, name)` of a mutated entry, and the kernel only ever
    /// remembers a dentry by `(display parent inode, display name)`. For a
    /// pass-through provider those are the same pair, so the default (empty)
    /// is correct. A presentation lens can render an entry SOMEWHERE ELSE —
    /// and the one coordinate a source event provably cannot reach is a leaf
    /// hanging under a *synthesized* scaffolding dir, whose inode exists only
    /// in the lens. Each returned `(parent, name)` is such an additional
    /// display coordinate for the given source entry, i.e. one the caller
    /// should ALSO invalidate (plus, for a synthesized parent, flush its
    /// cached listing: a virtual dir's mtime never moves, so its directory
    /// pages cannot self-invalidate and would otherwise hold the stale name
    /// until the attribute TTL).
    fn display_targets(&self, _src_parent: i64, _name: &str) -> Vec<(i64, String)> {
        Vec::new()
    }
    /// Drop whatever this provider has materialized for the listing of
    /// `parent` from its OWN cache, so the next read reflects the mutation that
    /// prompted the call.
    ///
    /// The companion of [`Self::display_targets`]: invalidating the KERNEL's
    /// view of a display coordinate is not enough when the provider also caches
    /// listings itself. A push event about the base tree never passes through a
    /// lens's mutating operations, so nothing else would flush it, and the
    /// display coordinate would keep serving the pre-event rows until the
    /// attribute TTL. Providers that read their store afresh (every engine, a
    /// plain remote client) keep the default no-op.
    fn flush_listing(&self, _parent: i64) {}
    /// Page `offset..offset+limit` of the same deterministic listing `readdir`
    /// returns. `GroveFS` streams directory replies through this so a big
    /// directory costs one small query per kernel buffer. Engines that page
    /// inside their store (SQLite, PostgreSQL, and a `RemoteMeta` client whose
    /// server advertised `M_READDIRP`) override this, and so does a
    /// derived/layout lens (which pages over one materialized listing instead of
    /// SQL). Providers that cannot — a remote client against a server too old to
    /// page — keep working through the default below, just without the O(page)
    /// win.
    fn readdir_page(&self, parent: i64, offset: usize, limit: usize) -> Vec<NodeInfo> {
        self.readdir(parent)
            .into_iter()
            .skip(offset)
            .take(limit)
            .collect()
    }
    /// Whether [`Self::readdir_page`] is a real sub-query (or, for a lens, a
    /// slice of a listing materialized once) rather than a full listing rebuilt
    /// per call. `GroveFS` uses this to decide its fetch chunking: a provider
    /// that cannot page is asked for one whole listing per kernel buffer
    /// (exactly what it did before paging existed), and it is not offered
    /// READDIRPLUS, whose fatter records would multiply that full fetch.
    fn supports_paged_readdir(&self) -> bool {
        false
    }
    #[allow(clippy::too_many_arguments)]
    fn create_node(
        &self,
        parent: i64,
        name: &str,
        is_dir: bool,
        data_ref: Option<&str>,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<i64, MetaError>;
    fn create_symlink(
        &self,
        parent: i64,
        name: &str,
        target: &str,
        uid: u32,
        gid: u32,
    ) -> Result<i64, MetaError>;
    /// `create_node` plus the attributes a `CREATE`/`MKDIR` reply needs.
    ///
    /// A local engine answers this with one extra store read, which costs no
    /// round trip. A remote client overrides it so the server ships the row it
    /// just wrote inside the same reply — otherwise every creation pays a
    /// second full `GETATTR` RTT purely to fill the FUSE reply (measured: 3
    /// metadata RTTs per create, see USAGE §4.1.1). Keeping the pair as a
    /// separate method leaves every other provider's signature untouched.
    #[allow(clippy::too_many_arguments)]
    fn create_node_with_attr(
        &self,
        parent: i64,
        name: &str,
        is_dir: bool,
        data_ref: Option<&str>,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<NodeInfo, MetaError> {
        let id = self.create_node(parent, name, is_dir, data_ref, mode, uid, gid)?;
        // A concurrent unlink between the create and this read is a real
        // outcome (the caller reports ENOENT), not a provider failure.
        self.getattr(id).ok_or(MetaError::NotFound)
    }
    /// Whether [`Self::create_nodes_batch`] really amortizes the commit.
    ///
    /// `GroveFS` groups concurrent `CREATE`s only when the store says yes: for a
    /// store that would merely loop `create_node_with_attr`, grouping turns N
    /// parallel commits into N *sequential* ones — strictly worse than letting
    /// every FUSE worker pay its own.
    fn supports_create_batch(&self) -> bool {
        false
    }
    /// Create several nodes, returning one result per item in input order.
    ///
    /// The default keeps per-item semantics (one commit each). A store with a
    /// local transaction overrides it so a burst of creates costs ONE commit
    /// instead of one per file: with a single-writer + global-ledger-lock engine
    /// that is the difference between a flat ~2k creates/s and a curve that
    /// scales with the number of writers. A failed item must not fail its
    /// siblings — the error stays in its own slot.
    fn create_nodes_batch(&self, items: &[NewNode<'_>]) -> Vec<Result<NodeInfo, MetaError>> {
        items
            .iter()
            .map(|it| {
                self.create_node_with_attr(
                    it.parent, it.name, it.is_dir, it.data_ref, it.mode, it.uid, it.gid,
                )
            })
            .collect()
    }
    /// Size any warm read parallelism to `n` concurrent readers.
    ///
    /// Only the embedded SQLite engine has such a pool; remote/external stores
    /// already fan out over their own connections, so the default is a no-op. A
    /// mount calls this with its FUSE worker count, which is exactly the most
    /// requests that can be in flight at once.
    fn set_read_pool(&self, _n: usize) {}
    /// Same idea for symlinks: the reply needs the entry attributes and the
    /// remote provider can fetch them without a second trip.
    fn create_symlink_with_attr(
        &self,
        parent: i64,
        name: &str,
        target: &str,
        uid: u32,
        gid: u32,
    ) -> Result<NodeInfo, MetaError> {
        let id = self.create_symlink(parent, name, target, uid, gid)?;
        self.getattr(id).ok_or(MetaError::NotFound)
    }
    fn link_node(&self, src_ino: i64, new_parent: i64, new_name: &str) -> Result<i64, MetaError>;
    fn unlink(&self, parent: i64, name: &str) -> Result<Option<String>, MetaError>;
    fn rename(
        &self,
        parent: i64,
        name: &str,
        new_parent: i64,
        new_name: &str,
        noreplace: bool,
    ) -> Result<Option<String>, MetaError>;
    fn set_mode(&self, ino: i64, mode: u32);
    fn set_uid_gid(&self, ino: i64, uid: u32, gid: u32);
    fn set_times(&self, ino: i64, atime: Option<i64>, mtime: Option<i64>);
    fn touch_mtime(&self, ino: i64);

    /// POSIX advisory record lock operation. Implementations MUST be atomic
    /// with respect to concurrent callers on the same `ino`. `sid` is the
    /// caller's mount-session id, stamped on granted rows so the daemon can
    /// renew them (`renew_locks`) and the authority can reap them on disconnect.
    #[allow(clippy::too_many_arguments)]
    fn lock_op(
        &self,
        ino: i64,
        owner: u64,
        start: u64,
        end: u64,
        ty: LockType,
        pid: u32,
        sid: u64,
    ) -> Result<Option<LockConflict>, MetaError>;
    /// F_GETLK: report the first blocking lock without modifying state.
    fn get_lock(
        &self,
        ino: i64,
        owner: u64,
        start: u64,
        end: u64,
        ty: LockType,
    ) -> Option<LockConflict>;
    /// FUSE `release`/`flush` cleanup: drop every lock held by `owner`.
    fn release_locks_of_owner(&self, owner: u64);
    /// Renewal heartbeat: extend the lease on every lock row owned by session
    /// `sid`. Called periodically by the mount daemon while it holds locks.
    fn renew_locks(&self, sid: u64);

    /// Handle to this authority's live fencing epoch, shared with the data-plane
    /// client so it can stamp writes with the current token. The default is a
    /// detached zero (no remote fence) — a local `MetaEngine` has no HA epoch and
    /// its (local) data path is never gated. `RemoteMeta` overrides this with the
    /// handle it refreshes on every `M_OPEN` handshake.
    fn fence_handle(&self) -> std::sync::Arc<std::sync::atomic::AtomicU64> {
        std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0))
    }

    /// Re-query the authoritative epoch now (e.g. after the data-server fenced a
    /// write stamped with a stale token) and return it. The default reports the
    /// already-known handle value; `RemoteMeta` reconnects to pick up a promoted
    /// node's higher epoch.
    fn fetch_fence_epoch(&self) -> u64 {
        self.fence_handle()
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Flat dump of the full tree for server-side aggregation (`du`). Each
    /// entry is `(id, parent_id, name, is_dir, data_ref)`. Providers that
    /// cannot dump (e.g. derived lenses, remote proxy) return empty.
    fn dump_tree(&self) -> Vec<(i64, i64, String, bool, Option<String>)> {
        Vec::new()
    }
}

#[derive(Debug)]
pub enum MetaError {
    Exists,
    NotFound,
    NotEmpty,
    TypeMismatch,
    OpNotPermitted,
    BadParentCycle,
    Db(String),
}

impl std::fmt::Display for MetaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MetaError::Exists => write!(f, "already exists"),
            MetaError::NotFound => write!(f, "not found"),
            MetaError::NotEmpty => write!(f, "directory not empty"),
            MetaError::TypeMismatch => write!(f, "type mismatch"),
            MetaError::OpNotPermitted => write!(f, "operation not permitted"),
            MetaError::BadParentCycle => write!(f, "invalid move into own subtree"),
            MetaError::Db(s) => write!(f, "metadata db error: {s}"),
        }
    }
}

impl std::error::Error for MetaError {}

// ---- server-side / embedded half (native only) ------------------------------
// Everything that opens an on-disk view database (SQLite via rusqlite). The
// `remote-only` build (Windows client) drops this whole subtree, and with it
// the bundled-SQLite C compilation.
#[cfg(feature = "native")]
mod engine;
#[cfg(feature = "native")]
pub use engine::*;

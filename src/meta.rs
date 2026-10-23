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
//! Hard links share one `data_ref`; the object is GC'd only when the last node
//! referencing it disappears (see [`MetaEngine::unlink`]). Because a client only
//! ever queries its own view DB, files it cannot see are never materialized on
//! the network — isolation is enforced here, not by permission bits.
//!
//! Inode model: a FUSE inode number == `node.ino`. Root is always ino 1.
//! Multiple directory entries (rows) may share one `ino` — that *is* a hard
//! link: they name the same inode and therefore the same data object, the same
//! size and the same timestamps. Fresh files/dirs get `ino == row id`.

use rusqlite::{params, Connection, OpenFlags, TransactionBehavior};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

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

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub struct MetaEngine {
    // A single connection, serialized behind a fast parking_lot mutex. The fuser
    // high-level session is already single-threaded, so this lock mainly guards
    // direct/concurrent callers and keeps each logical metadata op atomic.
    // Cross-process sharing (the *same* view mounted by two processes) is handled
    // by SQLite WAL plus the `busy_timeout` set in `open`, so a contending writer
    // waits for the lock instead of failing with SQLITE_BUSY (which would surface
    // to FUSE as EIO).
    conn: parking_lot::Mutex<Connection>,
    /// Pooled read-only connections (opened read-write to sidestep WAL `-shm`
    /// restrictions on strictly read-only handles, but only ever used for
    /// SELECT) so concurrent metadata reads fan out instead of serializing
    /// behind the single writer's `conn` mutex.
    read_pool: parking_lot::Mutex<Vec<Connection>>,
    /// DB path, kept so the pool can open extra read connections when drained.
    path: std::path::PathBuf,
}

/// Number of pooled read connections opened per view engine for concurrent
/// metadata reads.
pub const READ_POOL_SIZE: usize = 4;

/// Borrows one read-pool connection; returns it on drop (bounded). Derefs to
/// `Connection` so callers use it exactly like the writer guard.
struct ReadGuard<'a> {
    conn: Option<Connection>,
    eng: &'a MetaEngine,
}
impl std::ops::Deref for ReadGuard<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.conn.as_ref().expect("read conn alive until drop")
    }
}
impl Drop for ReadGuard<'_> {
    fn drop(&mut self) {
        if let Some(c) = self.conn.take() {
            let mut pool = self.eng.read_pool.lock();
            if pool.len() < READ_POOL_SIZE {
                pool.push(c);
            }
        }
    }
}

/// The columns returned by every node SELECT, in order.
const COLS: &str =
    "id,parent_id,name,is_dir,data_ref,link_target,mode,uid,gid,mtime,atime,ctime,ino";

impl MetaEngine {
    /// Open (and if necessary initialize) the view database for a view.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        Self::open_ex(path, false)
    }

    /// Open the view database, optionally as strictly read-only (`readonly`),
    /// which is how a HA *standby* meta-server serves its replicated copies:
    /// SQLite refuses any write at the API level, so a mistaken write-intent
    /// on a standby is impossible rather than merely discouraged. A read-only
    /// open never runs the schema-creation/migration statements (they would
    /// fail); the replicated copy already carries the schema.
    pub fn open_ex(path: &Path, readonly: bool) -> rusqlite::Result<Self> {
        let conn = if readonly {
            Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        } else {
            Connection::open(path)
        }?;
        if readonly {
            let _ = conn.execute_batch("PRAGMA busy_timeout=5000;");
            return Ok(Self {
                conn: parking_lot::Mutex::new(conn),
                read_pool: parking_lot::Mutex::new(Vec::new()),
                path: path.to_path_buf(),
            });
        }
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA foreign_keys=ON;
             PRAGMA busy_timeout=5000;
             CREATE TABLE IF NOT EXISTS node (
                 id           INTEGER PRIMARY KEY,
                 parent_id    INTEGER,
                 name         TEXT NOT NULL,
                 is_dir       INTEGER NOT NULL,
                 data_ref     TEXT,
                 link_target  TEXT,
                 mode         INTEGER NOT NULL,
                 uid          INTEGER NOT NULL,
                 gid          INTEGER NOT NULL,
                 mtime        INTEGER NOT NULL,
                 atime        INTEGER NOT NULL,
                 ctime        INTEGER NOT NULL,
                 UNIQUE(parent_id, name)
             );
             CREATE INDEX IF NOT EXISTS idx_node_parent ON node(parent_id);
             CREATE INDEX IF NOT EXISTS idx_node_dataref ON node(data_ref);
             CREATE TABLE IF NOT EXISTS ctrl (k TEXT PRIMARY KEY, v INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS locks (
                 id          INTEGER PRIMARY KEY AUTOINCREMENT,
                 ino         INTEGER NOT NULL,
                 owner       INTEGER NOT NULL,
                 sid         INTEGER NOT NULL DEFAULT 0,
                 l_start     INTEGER NOT NULL,
                 l_end       INTEGER NOT NULL,
                 pid         INTEGER NOT NULL,
                 ltype       INTEGER NOT NULL,
                 expires_at  INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_locks_ino    ON locks(ino, l_start, l_end);
             CREATE INDEX IF NOT EXISTS idx_locks_owner  ON locks(owner);
             CREATE INDEX IF NOT EXISTS idx_locks_sid    ON locks(sid);
             CREATE INDEX IF NOT EXISTS idx_locks_expire ON locks(expires_at);",
        )?;
        // Migration: older DBs may predate link_target / ino / locks.sid. Ignore
        // "duplicate column"; existing lock rows get sid=0 (never renewed, so
        // they simply age out on the lease — harmless at upgrade time).
        let _ = conn.execute("ALTER TABLE locks ADD COLUMN sid INTEGER NOT NULL DEFAULT 0", []);
        let _ = conn.execute("ALTER TABLE node ADD COLUMN link_target TEXT", []);
        let _ = conn.execute("ALTER TABLE node ADD COLUMN ino INTEGER", []);
        // Backfill ino for rows inserted before the column existed: one inode per name.
        let _ = conn.execute("UPDATE node SET ino=id WHERE ino IS NULL", []);
        // Index the inode column AFTER the migration above (so the column is
        // guaranteed to exist even on a pre-upgrade DB). FUSE `getattr` and every
        // setattr-path UPDATE key on `ino`, not the rowid `id`; without this index
        // each is a full table scan. This is the single hottest metadata read.
        conn.execute("CREATE INDEX IF NOT EXISTS idx_node_ino ON node(ino)", [])?;

        let has_root: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM node WHERE id = ?1",
                params![ROOT_INO],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if has_root == 0 {
            let t = now();
            conn.execute(
                "INSERT INTO node (id,parent_id,name,is_dir,data_ref,link_target,mode,uid,gid,mtime,atime,ctime,ino)
                 VALUES (?1,NULL,'/',1,NULL,NULL,?2,?3,?4,?5,?5,?5,?1)",
                params![ROOT_INO, 0o0755u32, unsafe { libc::getuid() }, unsafe { libc::getgid() }, t],
            )?;
        }
        let mut pool = Vec::new();
        for _ in 0..READ_POOL_SIZE {
            if let Ok(rc) = Connection::open(path) {
                let _ = rc.execute_batch("PRAGMA busy_timeout=5000;");
                pool.push(rc);
            }
        }
        Ok(Self {
            conn: parking_lot::Mutex::new(conn),
            read_pool: parking_lot::Mutex::new(pool),
            path: path.to_path_buf(),
        })
    }

    /// Borrow a pooled read connection for one SELECT-only op; falls back to a
    /// lazily-opened connection when the pool is drained (returned on drop).
    fn read_conn(&self) -> ReadGuard<'_> {
        let c = self.read_pool.lock().pop().unwrap_or_else(|| {
            let rc = Connection::open(&self.path).expect("open read connection");
            let _ = rc.execute_batch("PRAGMA busy_timeout=5000;");
            rc
        });
        ReadGuard { conn: Some(c), eng: self }
    }

    /// Resolve (parent, name) -> node. This is the FUSE `lookup` hot path.
    /// NOTE: DROP the lock before `finalize` (which re-locks) — parking_lot's
    /// Mutex is not reentrant, so holding it across the call would deadlock.
    pub fn lookup(&self, parent: i64, name: &str) -> Option<NodeInfo> {
        let sql = format!("SELECT {COLS} FROM node WHERE parent_id=?1 AND name=?2");
        let raw = {
            let conn = self.read_conn();
            let mut stmt = conn.prepare(&sql).ok()?;
            stmt.query_row(params![parent, name], row_to_raw).ok()
        };
        raw.map(|r| self.finalize(r))
    }

    /// Fetch a node by inode number (FUSE `getattr`). Any name sharing the
    /// inode resolves to the same attributes.
    pub fn getattr(&self, ino: i64) -> Option<NodeInfo> {
        let sql = format!("SELECT {COLS} FROM node WHERE ino=?1 LIMIT 1");
        let raw = {
            let conn = self.read_conn();
            let mut stmt = conn.prepare(&sql).ok()?;
            stmt.query_row(params![ino], row_to_raw).ok()
        };
        raw.map(|r| self.finalize(r))
    }

    /// Raw parent_id lookup without nlink/size computation (cheap, for fs_impl).
    pub fn readdir(&self, parent: i64) -> Vec<NodeInfo> {
        let sql = format!("SELECT {COLS} FROM node WHERE parent_id=?1 ORDER BY name");
        let raws = {
            let conn = self.read_conn();
            let mut stmt = match conn.prepare(&sql) {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
            let rows = match stmt.query_map(params![parent], row_to_raw) {
                Ok(r) => r,
                Err(_) => return Vec::new(),
            };
            rows.filter_map(|r| r.ok()).collect::<Vec<_>>()
        };
        raws.into_iter().map(|r| self.finalize(r)).collect()
    }

    /// One slice of the deterministic listing: children in the same
    /// `ORDER BY name` sequence `readdir` uses, starting at `offset` and at most
    /// `limit` rows. Paging is pushed into SQL so replying to one kernel
    /// READDIR/READDIRPLUS buffer costs O(buffer) instead of re-scanning *and*
    /// re-finalizing the whole directory (one `COUNT` query per entry) once per
    /// buffer — the difference between a large directory feeling instant and
    /// taking minutes.
    pub fn readdir_page(&self, parent: i64, offset: usize, limit: usize) -> Vec<NodeInfo> {
        let sql =
            format!("SELECT {COLS} FROM node WHERE parent_id=?1 ORDER BY name LIMIT ?2 OFFSET ?3");
        let raws = {
            let conn = self.read_conn();
            let mut stmt = match conn.prepare(&sql) {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
            let rows = match stmt.query_map(
                params![parent, limit as i64, offset as i64],
                row_to_raw,
            ) {
                Ok(r) => r,
                Err(_) => return Vec::new(),
            };
            rows.filter_map(|r| r.ok()).collect::<Vec<_>>()
        };
        raws.into_iter().map(|r| self.finalize(r)).collect()
    }

    /// Create a new file/dir node. `data_ref` is Some for files, None for dirs.
    #[allow(clippy::too_many_arguments)]
    pub fn create_node(
        &self,
        parent: i64,
        name: &str,
        is_dir: bool,
        data_ref: Option<&str>,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<i64, MetaError> {
        self.insert(parent, name, is_dir, data_ref, None, mode, uid, gid)
    }

    /// Create a symbolic link node pointing at `target`.
    pub fn create_symlink(
        &self,
        parent: i64,
        name: &str,
        target: &str,
        uid: u32,
        gid: u32,
    ) -> Result<i64, MetaError> {
        // Symlink perms are always 0777 (ignored by the kernel on Linux).
        self.insert(parent, name, false, None, Some(target), 0o0777, uid, gid)
    }

    #[allow(clippy::too_many_arguments)]
    fn insert(
        &self,
        parent: i64,
        name: &str,
        is_dir: bool,
        data_ref: Option<&str>,
        link_target: Option<&str>,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<i64, MetaError> {
        let t = now();
        let mut conn = self.conn.lock();
        // Atomic, exactly like rename/unlink/lock_op: one IMMEDIATE tx wraps the
        // INSERT *and* the `ino = rowid` back-fill. Two autocommit statements
        // would let a concurrent reader on a separate WAL connection observe the
        // row between them with `ino IS NULL` (invisible to `WHERE ino=?`), and a
        // crash in that window would leave a dangling NULL-ino row until the next
        // open's back-fill. A failed INSERT (constraint) rolls the tx back and
        // still surfaces as Exists; the UPDATE's error is propagated, not swallowed.
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO node (parent_id,name,is_dir,data_ref,link_target,mode,uid,gid,mtime,atime,ctime)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?9,?9)",
            params![parent, name, is_dir as i64, data_ref, link_target, mode, uid, gid, t],
        )
        .map_err(|e| match e {
            rusqlite::Error::SqliteFailure(f, _)
                if f.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                MetaError::Exists
            }
            other => MetaError::Db(other.to_string()),
        })?;
        let rowid = tx.last_insert_rowid();
        // A brand-new name owns a fresh inode: ino == its own row id.
        tx.execute("UPDATE node SET ino=?1 WHERE id=?1", params![rowid])?;
        tx.commit()?;
        Ok(rowid)
    }

    /// Create a hard link: a new directory entry that SHARES `src_ino`'s inode
    /// (same data object, same size, same timestamps). Only regular files may be
    /// hard-linked (POSIX forbids linking dirs). Returns the shared inode number.
    pub fn link_node(&self, src_ino: i64, new_parent: i64, new_name: &str) -> Result<i64, MetaError> {
        let conn = self.conn.lock();
        let row = conn
            .query_row(
                "SELECT is_dir,data_ref,mode,uid,gid FROM node WHERE ino=?1 LIMIT 1",
                params![src_ino],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, i64>(4)?,
                    ))
                },
            )
            .map_err(|_| MetaError::NotFound)?;
        let (is_dir, data_ref, mode, uid, gid) = row;
        if is_dir == 1 || data_ref.is_none() {
            return Err(MetaError::OpNotPermitted);
        }
        let t = now();
        conn.execute(
            "INSERT INTO node (parent_id,name,is_dir,data_ref,link_target,mode,uid,gid,mtime,atime,ctime,ino)
             VALUES (?1,?2,0,?3,NULL,?4,?5,?6,?7,?7,?7,?8)",
            params![new_parent, new_name, data_ref, mode, uid, gid, t, src_ino],
        )
        .map_err(|e| match e {
            rusqlite::Error::SqliteFailure(f, _)
                if f.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                MetaError::Exists
            }
            other => MetaError::Db(other.to_string()),
        })?;
        Ok(src_ino)
    }

    /// Rename / move a node within the view tree (custom hierarchy lives here).
    /// Wrapped in an IMMEDIATE transaction so the read-modify-write (find src ->
    /// cycle check -> replace target -> reparent) is atomic and takes the write
    /// lock up front, so two concurrent mounts can never observe a half-applied
    /// rename.
    pub fn rename(&self, parent: i64, name: &str, new_parent: i64, new_name: &str, noreplace: bool) -> Result<Option<String>, MetaError> {
        let t = now();
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (src_id, src_ino): (i64, Option<i64>) = tx
            .query_row(
                "SELECT id,ino FROM node WHERE parent_id=?1 AND name=?2",
                params![parent, name],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(|_| MetaError::NotFound)?;
        // Refuse to move a directory into its own subtree (would corrupt the tree).
        if self.is_descendant_locked(&tx, new_parent, src_id) {
            return Err(MetaError::BadParentCycle);
        }
        let tgt: Option<(i64, i64, Option<String>, Option<i64>)> = tx
            .query_row(
                "SELECT id,is_dir,data_ref,ino FROM node WHERE parent_id=?1 AND name=?2",
                params![new_parent, new_name],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, Option<i64>>(3)?)),
            )
            .ok();
        // rename(a, a): the target row IS the source row — a successful no-op.
        // Without this guard the DELETE below would remove the source and the
        // later UPDATE would match nothing, silently destroying the entry.
        // The same-inode case covers two *different* hard link names of one
        // file: POSIX (man 2 rename) also mandates a silent no-op there, but
        // the row-id guard alone would miss it and delete the target name,
        // shrinking nlink (a link vanishes). Compare ino, not just row id.
        if let Some((tid, _, _, tino)) = &tgt {
            if *tid == src_id || (*tino).is_some() && *tino == src_ino {
                tx.commit()?;
                return Ok(None);
            }
        }
        // RENAME_NOREPLACE: an existing target must fail with EEXIST instead of
        // being silently overwritten and destroyed.
        if tgt.is_some() && noreplace {
            return Err(MetaError::Exists);
        }
        // Blob freed by overwriting a target that held the last name to its data;
        // the caller GCs it in the storage backend (mirrors unlink's refcount rule).
        let mut gc: Option<String> = None;
        if let Some((tid, is_dir, tgt_ref, _)) = tgt {
            let src_is_dir: i64 = tx
                .query_row("SELECT is_dir FROM node WHERE id=?1", params![src_id], |r| r.get(0))
                .unwrap_or(0);
            if src_is_dir != is_dir {
                return Err(MetaError::TypeMismatch);
            }
            // Renaming a directory over an EXISTING directory requires the target
            // be empty: deleting a non-empty one orphans its children (their
            // parent_id points at the removed row). POSIX: ENOTEMPTY.
            if is_dir == 1 {
                let kids: i64 = tx
                    .query_row(
                        "SELECT COUNT(*) FROM node WHERE parent_id=?1",
                        params![tid],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                if kids > 0 {
                    return Err(MetaError::NotEmpty);
                }
            }
            tx.execute("DELETE FROM node WHERE id=?1", params![tid])?;
            // A dir/symlink target has no data_ref (gc stays None); a regular-file
            // target's blob is freed only once no other name still references it.
            if let Some(dr) = tgt_ref {
                let refs: i64 = tx
                    .query_row(
                        "SELECT COUNT(*) FROM node WHERE data_ref=?1",
                        params![dr],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                if refs == 0 {
                    gc = Some(dr);
                }
            }
        }
        tx.execute(
            "UPDATE node SET parent_id=?1, name=?2, ctime=?3 WHERE id=?4",
            params![new_parent, new_name, t, src_id],
        )?;
        tx.commit()?;
        Ok(gc)
    }

    fn is_descendant_locked(
        &self,
        conn: &Connection,
        new_parent: i64,
        src_id: i64,
    ) -> bool {
        // Walk ancestors of new_parent; if we reach src_id it's a cycle.
        let mut cur = new_parent;
        for _ in 0..4096 {
            if cur == src_id {
                return true;
            }
            let parent: Option<i64> = conn
                .query_row("SELECT parent_id FROM node WHERE id=?1", params![cur], |r| r.get(0))
                .unwrap_or(None);
            match parent {
                Some(p) => cur = p,
                None => return false,
            }
        }
        false
    }

    /// Remove a node. Returns the freed data_ref if this was the last reference
    /// (so the caller can GC the backend blob).
    ///
    /// The find -> DELETE -> refcount-read runs as one IMMEDIATE transaction so
    /// two processes hardlinking the same inode can never interleave and either
    /// GC a still-referenced object or miss GC of the last name; `busy_timeout`
    /// makes the loser of a lock race wait rather than error.
    pub fn unlink(&self, parent: i64, name: &str) -> Result<Option<String>, MetaError> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (id, is_dir, data_ref) = tx
            .query_row(
                "SELECT id,is_dir,data_ref FROM node WHERE parent_id=?1 AND name=?2",
                params![parent, name],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<String>>(2)?)),
            )
            .map_err(|_| MetaError::NotFound)?;
        if is_dir == 1 {
            let kids: i64 = tx
                .query_row(
                    "SELECT COUNT(*) FROM node WHERE parent_id=?1",
                    params![id],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            if kids > 0 {
                return Err(MetaError::NotEmpty);
            }
        }
        tx.execute("DELETE FROM node WHERE id=?1", params![id])?;
        let gc = match &data_ref {
            Some(dr) => {
                let refs: i64 = tx
                    .query_row(
                        "SELECT COUNT(*) FROM node WHERE data_ref=?1",
                        params![dr],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                if refs == 0 { Some(dr.clone()) } else { None }
            }
            None => None,
        };
        tx.commit()?;
        Ok(gc)
    }

    /// Update a file's mtime/ctime (after a write or truncate). Applies to every
    /// name sharing the inode, so hard links stay in sync.
    pub fn touch_mtime(&self, ino: i64) {
        let t = now();
        let conn = self.conn.lock();
        let _ = conn.execute(
            "UPDATE node SET mtime=?1, ctime=?1 WHERE ino=?2",
            params![t, ino],
        );
    }

    /// Explicitly set access/modification times (setattr with SET_ATIME/SET_MTIME).
    pub fn set_times(&self, ino: i64, atime: Option<i64>, mtime: Option<i64>) {
        let t = now();
        let conn = self.conn.lock();
        if let Some(a) = atime {
            let _ = conn.execute("UPDATE node SET atime=?1, ctime=?2 WHERE ino=?3", params![a, t, ino]);
        }
        if let Some(m) = mtime {
            let _ = conn.execute("UPDATE node SET mtime=?1, ctime=?2 WHERE ino=?3", params![m, t, ino]);
        }
    }

    pub fn set_mode(&self, ino: i64, mode: u32) {
        let t = now();
        let conn = self.conn.lock();
        let _ = conn.execute("UPDATE node SET mode=?1, ctime=?2 WHERE ino=?3", params![mode, t, ino]);
    }

    pub fn set_uid_gid(&self, ino: i64, uid: u32, gid: u32) {
        let t = now();
        let conn = self.conn.lock();
        let _ = conn.execute(
            "UPDATE node SET uid=?1, gid=?2, ctime=?3 WHERE ino=?4",
            params![uid, gid, t, ino],
        );
    }

    /// Current HA epoch stored in this DB's `ctrl` table (0 when absent, e.g.
    /// pre-HA files or a read-only copy whose `ctrl` row hasn't been seeded).
    pub fn get_epoch(&self) -> i64 {
        let conn = self.conn.lock();
        conn.query_row("SELECT v FROM ctrl WHERE k='epoch'", [], |r| r.get(0))
            .unwrap_or(0)
    }

    /// Set the HA epoch row (creating it when the ctrl table was just added).
    pub fn set_epoch(&self, e: i64) -> rusqlite::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO ctrl (k,v) VALUES ('epoch',?1)
             ON CONFLICT(k) DO UPDATE SET v=excluded.v",
            params![e],
        )?;
        Ok(())
    }

    /// Consistent whole-database snapshot as raw bytes for HA replication.
    /// Uses SQLite's `VACUUM INTO` (single-writer online backup: the output is
    /// a complete, checkpointed image — a side-copy of `<view>.db` would race
    /// WAL checkpoints and could ship a torn file). Holds the engine's own
    /// connection lock for the duration, so replication never reads a
    /// half-written page; the step blocks concurrent metadata writes for the
    /// vacuum time of one view DB (ms for view-sized trees).
    pub fn backup_to_bytes(&self) -> rusqlite::Result<Vec<u8>> {
        static SNAP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SNAP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = std::env::temp_dir().join(format!("grovefs_snap_{}_{}.db", std::process::id(), n));
        let _ = std::fs::remove_file(&tmp);
        let conn = self.conn.lock();
        let r = conn.execute("VACUUM INTO ?1", params![tmp.to_string_lossy().as_ref()]);
        let out = match r {
            Ok(_) => std::fs::read(&tmp).map_err(|e| {
                rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_IOERR),
                    Some(e.to_string()),
                )
            }),
            Err(e) => Err(e),
        };
        let _ = std::fs::remove_file(&tmp);
        out
    }

    // ---- POSIX advisory record locks ------------------------------------------------
    //
    // Kernel FUSE translates both `fcntl F_SETLK` and BSD `flock(2)` into the
    // same wire contract (`typ` = F_RDLCK/F_WRLCK/F_UNLCK + interval + owner),
    // with flock degenerating to `[0, ULLONG_MAX)` on the whole file. We store
    // the interval as signed i64 and clamp ULLONG_MAX to i64::MAX — 9.2 exabytes
    // is far past any real file, and it lets us index the table.
    //
    // Conflict rules (POSIX): Read/Read compatible; Read/Write and Write/Write
    // conflict; a lock owned by the same `owner` (file description) *converts*
    // overlapping ranges instead of conflicting (matches `fcntl` semantics).
    //
    // Lease: each row carries an `expires_at` (unix seconds, now + LOCK_LEASE_SECS
    // at insert/renew time). Expired rows are swept at the head of every write
    // transaction — this is our safety net for clients SIGKILL'd before their
    // FUSE `release` reached us.

    /// Perform a POSIX advisory lock operation. `ty == Unlock` removes the
    /// overlapping interval for `owner`; `Read`/`Write` either grants (returns
    /// `Ok(None)`) or reports the first blocking lock (returns `Ok(Some(..))`).
    #[allow(clippy::too_many_arguments)]
    pub fn lock_op(
        &self,
        ino: i64,
        owner: u64,
        start: u64,
        end: u64,
        ty: LockType,
        pid: u32,
        sid: u64,
    ) -> Result<Option<LockConflict>, MetaError> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now();
        tx.execute(
            "DELETE FROM locks WHERE expires_at < ?1",
            params![now],
        )?;
        let s = start.min(i64::MAX as u64) as i64;
        let e = end.min(i64::MAX as u64) as i64;
        match ty {
            LockType::Unlock => {
                tx.execute(
                    "DELETE FROM locks WHERE ino=?1 AND owner=?2 AND l_start<=?3 AND l_end>=?4",
                    params![ino, owner as i64, e, s],
                )?;
            }
            LockType::Read | LockType::Write => {
                // Detect cross-owner conflict first; same-owner overlaps convert.
                let conflict: Option<(i64, i64, i64, i64)> = tx
                    .query_row(
                        "SELECT l_start, l_end, ltype, pid FROM locks \
                         WHERE ino=?1 AND owner<>?2 AND l_start<=?3 AND l_end>=?4 \
                         AND (?5 = 1 OR ltype = 1) \
                         LIMIT 1",
                        params![ino, owner as i64, e, s, ty.to_u8() as i64],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                    )
                    .ok();
                if let Some((cs, ce, ct, cp)) = conflict {
                    return Ok(Some(LockConflict {
                        start: cs.max(0) as u64,
                        end: ce.max(0) as u64,
                        ty: LockType::from_u8(ct as u8).unwrap_or(LockType::Read),
                        pid: cp as u32,
                    }));
                }
                // Convert: drop any same-owner overlapping rows, then insert.
                tx.execute(
                    "DELETE FROM locks WHERE ino=?1 AND owner=?2 AND l_start<=?3 AND l_end>=?4",
                    params![ino, owner as i64, e, s],
                )?;
                tx.execute(
                    "INSERT INTO locks (ino, owner, sid, l_start, l_end, pid, ltype, expires_at) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                    params![ino, owner as i64, sid as i64, s, e, pid as i64, ty.to_u8() as i64, now + LockLease::secs()],
                )?;
            }
        }
        tx.commit()?;
        Ok(None)
    }

    /// F_GETLK: report the first blocking lock for the requested interval, or
    /// `None` if the operation would succeed. No row is written.
    pub fn get_lock(
        &self,
        ino: i64,
        owner: u64,
        start: u64,
        end: u64,
        ty: LockType,
    ) -> Option<LockConflict> {
        let conn = self.conn.lock();
        let now = now();
        let s = start.min(i64::MAX as u64) as i64;
        let e = end.min(i64::MAX as u64) as i64;
        conn.query_row(
            "SELECT l_start, l_end, ltype, pid FROM locks \
             WHERE ino=?1 AND owner<>?2 AND expires_at>=?3 AND l_start<=?4 AND l_end>=?5 \
             AND (?6 = 1 OR ltype = 1) \
             LIMIT 1",
            params![ino, owner as i64, now, e, s, ty.to_u8() as i64],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?)),
        )
        .ok()
        .map(|(cs, ce, ct, cp)| LockConflict {
            start: cs.max(0) as u64,
            end: ce.max(0) as u64,
            ty: LockType::from_u8(ct as u8).unwrap_or(LockType::Read),
            pid: cp as u32,
        })
    }

    /// Release every lock held by `owner` — called from FUSE `release` on the
    /// last close of a file description that had locks. Idempotent.
    pub fn release_locks_of_owner(&self, owner: u64) {
        let conn = self.conn.lock();
        let _ = conn.execute("DELETE FROM locks WHERE owner=?1", params![owner as i64]);
    }

    /// Renewal heartbeat: push `expires_at` out by one full lease for every lock
    /// row owned by session `sid`. Driven by the mount daemon on a
    /// `lease/3` cadence so a *live* holder never expires; a dead one stops
    /// renewing and ages out within one lease. No-op (zero rows) when idle.
    pub fn renew_locks(&self, sid: u64) {
        let conn = self.conn.lock();
        let now = now();
        let _ = conn.execute(
            "UPDATE locks SET expires_at = ?1 WHERE sid = ?2",
            params![now + LockLease::secs(), sid as i64],
        );
    }

    /// Reap every lock held by session `sid`. Called on the meta-server when a
    /// client's connection drops (daemon crash / unmount): reclaims its locks
    /// *immediately* instead of waiting out the lease, shrinking the dangling
    /// window to ~0 for the common remote-crash case. The lease stays only as
    /// the final backstop for network partitions where the socket lingers.
    pub fn release_locks_of_session(&self, sid: u64) {
        let conn = self.conn.lock();
        let _ = conn.execute("DELETE FROM locks WHERE sid=?1", params![sid as i64]);
    }

    /// Explicitly reap expired locks. Called opportunistically inside every
    /// write txn; exposed so tests (and future lease-renew RPCs) can drive it.
    pub fn sweep_expired_locks(&self, now_secs: i64) {
        let conn = self.conn.lock();
        let _ = conn.execute("DELETE FROM locks WHERE expires_at < ?1", params![now_secs]);
    }

    /// Compute nlink/size and resolve the node kind. Re-locks the connection,
    /// so callers MUST have already released their own guard.
    fn finalize(&self, raw: RawNode) -> NodeInfo {
        let kind = if raw.is_dir == 1 {
            NodeKind::Dir
        } else if raw.link_target.is_some() {
            NodeKind::Symlink
        } else {
            NodeKind::Regular
        };
        let conn = self.read_conn();
        let (nlink, size) = match kind {
            NodeKind::Dir => {
                let subdirs: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM node WHERE parent_id=?1 AND is_dir=1",
                        params![raw.id],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                (2 + subdirs as u64, 4096u64)
            }
            NodeKind::Symlink => {
                let n = raw.link_target.as_ref().map(|s| s.len() as u64).unwrap_or(0);
                (1, n)
            }
            NodeKind::Regular => {
                let links: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM node WHERE ino=?1",
                        params![raw.ino],
                        |r| r.get(0),
                    )
                    .unwrap_or(1);
                (links.max(1) as u64, 0u64)
            }
        };
        NodeInfo {
            id: raw.ino,
            parent_id: raw.parent_id.unwrap_or(0),
            name: raw.name,
            kind,
            data_ref: raw.data_ref,
            link_target: raw.link_target,
            mode: raw.mode as u32,
            uid: raw.uid as u32,
            gid: raw.gid as u32,
            mtime: raw.mtime,
            atime: raw.atime,
            ctime: raw.ctime,
            nlink,
            size,
        }
    }
}

/// The metadata interface [`crate::fs_impl::GroveFS`] actually depends on. It is
/// implemented locally by [`MetaEngine`] (a direct per-view SQLite file) and
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
    /// Page `offset..offset+limit` of the same deterministic listing `readdir`
    /// returns. `GroveFS` streams directory replies through this so a big
    /// directory costs one small query per kernel buffer. Engines that page
    /// inside their store (SQLite, PostgreSQL, and a `RemoteMeta` client whose
    /// server advertised `M_READDIRP`) override this; providers that cannot (a
    /// derived/layout lens, or a remote client against a server too old to page)
    /// keep working through the default below, just without the O(page) win.
    fn readdir_page(&self, parent: i64, offset: usize, limit: usize) -> Vec<NodeInfo> {
        self.readdir(parent).into_iter().skip(offset).take(limit).collect()
    }
    /// Whether [`Self::readdir_page`] is a real sub-query or just a slice of a
    /// full listing. Only engines that can page *inside their store* answer
    /// true. `GroveFS` uses this to keep the cheap behaviour for the others:
    /// a provider with no store-level paging is asked for one whole listing per
    /// kernel buffer (exactly what it did before paging existed), and it is not
    /// offered READDIRPLUS, whose fatter records would multiply that full fetch.
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
    fn create_symlink(&self, parent: i64, name: &str, target: &str, uid: u32, gid: u32) -> Result<i64, MetaError>;
    fn link_node(&self, src_ino: i64, new_parent: i64, new_name: &str) -> Result<i64, MetaError>;
    fn unlink(&self, parent: i64, name: &str) -> Result<Option<String>, MetaError>;
    fn rename(&self, parent: i64, name: &str, new_parent: i64, new_name: &str, noreplace: bool) -> Result<Option<String>, MetaError>;
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
    fn get_lock(&self, ino: i64, owner: u64, start: u64, end: u64, ty: LockType) -> Option<LockConflict>;
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
        self.fence_handle().load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl MetaProvider for MetaEngine {
    fn lookup(&self, parent: i64, name: &str) -> Option<NodeInfo> {
        MetaEngine::lookup(self, parent, name)
    }
    fn getattr(&self, ino: i64) -> Option<NodeInfo> {
        MetaEngine::getattr(self, ino)
    }
    fn readdir(&self, parent: i64) -> Vec<NodeInfo> {
        MetaEngine::readdir(self, parent)
    }
    fn readdir_page(&self, parent: i64, offset: usize, limit: usize) -> Vec<NodeInfo> {
        MetaEngine::readdir_page(self, parent, offset, limit)
    }
    fn supports_paged_readdir(&self) -> bool {
        true
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
    ) -> Result<i64, MetaError> {
        MetaEngine::create_node(self, parent, name, is_dir, data_ref, mode, uid, gid)
    }
    fn create_symlink(&self, parent: i64, name: &str, target: &str, uid: u32, gid: u32) -> Result<i64, MetaError> {
        MetaEngine::create_symlink(self, parent, name, target, uid, gid)
    }
    fn link_node(&self, src_ino: i64, new_parent: i64, new_name: &str) -> Result<i64, MetaError> {
        MetaEngine::link_node(self, src_ino, new_parent, new_name)
    }
    fn unlink(&self, parent: i64, name: &str) -> Result<Option<String>, MetaError> {
        MetaEngine::unlink(self, parent, name)
    }
    fn rename(&self, parent: i64, name: &str, new_parent: i64, new_name: &str, noreplace: bool) -> Result<Option<String>, MetaError> {
        MetaEngine::rename(self, parent, name, new_parent, new_name, noreplace)
    }
    fn set_mode(&self, ino: i64, mode: u32) {
        MetaEngine::set_mode(self, ino, mode)
    }
    fn set_uid_gid(&self, ino: i64, uid: u32, gid: u32) {
        MetaEngine::set_uid_gid(self, ino, uid, gid)
    }
    fn set_times(&self, ino: i64, atime: Option<i64>, mtime: Option<i64>) {
        MetaEngine::set_times(self, ino, atime, mtime)
    }
    fn touch_mtime(&self, ino: i64) {
        MetaEngine::touch_mtime(self, ino)
    }
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
    ) -> Result<Option<LockConflict>, MetaError> {
        MetaEngine::lock_op(self, ino, owner, start, end, ty, pid, sid)
    }
    fn get_lock(&self, ino: i64, owner: u64, start: u64, end: u64, ty: LockType) -> Option<LockConflict> {
        MetaEngine::get_lock(self, ino, owner, start, end, ty)
    }
    fn release_locks_of_owner(&self, owner: u64) {
        MetaEngine::release_locks_of_owner(self, owner)
    }
    fn renew_locks(&self, sid: u64) {
        MetaEngine::renew_locks(self, sid)
    }
}

// ===================== pluggable metadata store (SQLite | PostgreSQL) ======

/// A concrete metadata store the central `meta-server` (or a direct mount) can
/// drive: the embedded SQLite [`MetaEngine`] or the [`crate::pg_store::PgEngine`]
/// PostgreSQL backend. Both implement [`MetaProvider`], and this enum exposes
/// the *same* inherent surface (`get_epoch`/`set_epoch`/`backup_to_bytes`) that
/// the server's HA/replication code calls on the engine, so swapping backends is
/// a construction-time choice in `engine_for` — the dispatch and event code never
/// branch on the store.
///
/// GroveFS-native HA (`VACUUM INTO` snapshot shipping) is SQLite-only; under the
/// PG variant `backup_to_bytes` reports unsupported and a `--store postgres://…`
/// meta-server rejects the `--role/--peer/--witness` flags at CLI time.
pub enum MetaStore {
    Sqlite(MetaEngine),
    Pg(Box<crate::pg_store::PgEngine>),
}

impl MetaStore {
    /// Open a SQLite store for `<base>/meta/<view>.db` (`readonly` for a standby).
    pub fn open_sqlite(base: &std::path::Path, view: &str, readonly: bool) -> rusqlite::Result<Self> {
        let path = base.join("meta").join(format!("{view}.db"));
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        MetaEngine::open_ex(&path, readonly).map(MetaStore::Sqlite)
    }

    /// Open a PostgreSQL store for `view` in the database at `conn_uri`.
    pub fn open_pg(conn_uri: &str, view: &str, readonly: bool) -> std::io::Result<Self> {
        crate::pg_store::PgEngine::open(conn_uri, view, readonly).map(|p| MetaStore::Pg(Box::new(p)))
    }

    pub fn get_epoch(&self) -> i64 {
        match self {
            MetaStore::Sqlite(m) => m.get_epoch(),
            MetaStore::Pg(p) => p.get_epoch(),
        }
    }

    pub fn set_epoch(&self, e: i64) -> std::io::Result<()> {
        match self {
            MetaStore::Sqlite(m) => m.set_epoch(e).map_err(|err| std::io::Error::other(err.to_string())),
            MetaStore::Pg(p) => p.set_epoch(e),
        }
    }

    pub fn backup_to_bytes(&self) -> std::io::Result<Vec<u8>> {
        match self {
            MetaStore::Sqlite(m) => m.backup_to_bytes().map_err(|err| std::io::Error::other(err.to_string())),
            MetaStore::Pg(p) => p.backup_to_bytes(),
        }
    }
}

impl MetaProvider for MetaStore {
    fn lookup(&self, parent: i64, name: &str) -> Option<NodeInfo> {
        match self { MetaStore::Sqlite(m) => m.lookup(parent, name), MetaStore::Pg(p) => p.lookup(parent, name) }
    }
    fn getattr(&self, ino: i64) -> Option<NodeInfo> {
        match self { MetaStore::Sqlite(m) => m.getattr(ino), MetaStore::Pg(p) => p.getattr(ino) }
    }
    fn readdir(&self, parent: i64) -> Vec<NodeInfo> {
        match self { MetaStore::Sqlite(m) => m.readdir(parent), MetaStore::Pg(p) => p.readdir(parent) }
    }
    fn readdir_page(&self, parent: i64, offset: usize, limit: usize) -> Vec<NodeInfo> {
        match self { MetaStore::Sqlite(m) => m.readdir_page(parent, offset, limit), MetaStore::Pg(p) => p.readdir_page(parent, offset, limit) }
    }
    fn supports_paged_readdir(&self) -> bool {
        // Both concrete stores page inside their own store (SQLite via
        // LIMIT/OFFSET on the read pool, PG via LIMIT/OFFSET on its read pool),
        // so a meta-server backed by either can serve M_READDIRP and offer
        // READDIRPLUS to its clients. A derived view served through a
        // [`crate::layout::LayoutMeta`] lens does NOT reach here — the lens keeps
        // the trait default (false) because its hidden/mapped shape cannot be
        // expressed as a single SQL page.
        match self {
            MetaStore::Sqlite(m) => MetaProvider::supports_paged_readdir(m),
            MetaStore::Pg(p) => MetaProvider::supports_paged_readdir(&**p),
        }
    }
    #[allow(clippy::too_many_arguments)]
    fn create_node(&self, parent: i64, name: &str, is_dir: bool, data_ref: Option<&str>, mode: u32, uid: u32, gid: u32) -> Result<i64, MetaError> {
        match self { MetaStore::Sqlite(m) => m.create_node(parent, name, is_dir, data_ref, mode, uid, gid), MetaStore::Pg(p) => p.create_node(parent, name, is_dir, data_ref, mode, uid, gid) }
    }
    fn create_symlink(&self, parent: i64, name: &str, target: &str, uid: u32, gid: u32) -> Result<i64, MetaError> {
        match self { MetaStore::Sqlite(m) => m.create_symlink(parent, name, target, uid, gid), MetaStore::Pg(p) => p.create_symlink(parent, name, target, uid, gid) }
    }
    fn link_node(&self, src_ino: i64, new_parent: i64, new_name: &str) -> Result<i64, MetaError> {
        match self { MetaStore::Sqlite(m) => m.link_node(src_ino, new_parent, new_name), MetaStore::Pg(p) => p.link_node(src_ino, new_parent, new_name) }
    }
    fn unlink(&self, parent: i64, name: &str) -> Result<Option<String>, MetaError> {
        match self { MetaStore::Sqlite(m) => m.unlink(parent, name), MetaStore::Pg(p) => p.unlink(parent, name) }
    }
    fn rename(&self, parent: i64, name: &str, new_parent: i64, new_name: &str, noreplace: bool) -> Result<Option<String>, MetaError> {
        match self { MetaStore::Sqlite(m) => m.rename(parent, name, new_parent, new_name, noreplace), MetaStore::Pg(p) => p.rename(parent, name, new_parent, new_name, noreplace) }
    }
    fn set_mode(&self, ino: i64, mode: u32) {
        match self { MetaStore::Sqlite(m) => m.set_mode(ino, mode), MetaStore::Pg(p) => p.set_mode(ino, mode) }
    }
    fn set_uid_gid(&self, ino: i64, uid: u32, gid: u32) {
        match self { MetaStore::Sqlite(m) => m.set_uid_gid(ino, uid, gid), MetaStore::Pg(p) => p.set_uid_gid(ino, uid, gid) }
    }
    fn set_times(&self, ino: i64, atime: Option<i64>, mtime: Option<i64>) {
        match self { MetaStore::Sqlite(m) => m.set_times(ino, atime, mtime), MetaStore::Pg(p) => p.set_times(ino, atime, mtime) }
    }
    fn touch_mtime(&self, ino: i64) {
        match self { MetaStore::Sqlite(m) => m.touch_mtime(ino), MetaStore::Pg(p) => p.touch_mtime(ino) }
    }
    #[allow(clippy::too_many_arguments)]
    fn lock_op(&self, ino: i64, owner: u64, start: u64, end: u64, ty: LockType, pid: u32, sid: u64) -> Result<Option<LockConflict>, MetaError> {
        match self { MetaStore::Sqlite(m) => m.lock_op(ino, owner, start, end, ty, pid, sid), MetaStore::Pg(p) => p.lock_op(ino, owner, start, end, ty, pid, sid) }
    }
    fn get_lock(&self, ino: i64, owner: u64, start: u64, end: u64, ty: LockType) -> Option<LockConflict> {
        match self { MetaStore::Sqlite(m) => m.get_lock(ino, owner, start, end, ty), MetaStore::Pg(p) => p.get_lock(ino, owner, start, end, ty) }
    }
    fn release_locks_of_owner(&self, owner: u64) {
        match self { MetaStore::Sqlite(m) => m.release_locks_of_owner(owner), MetaStore::Pg(p) => p.release_locks_of_owner(owner) }
    }
    fn renew_locks(&self, sid: u64) {
        match self { MetaStore::Sqlite(m) => m.renew_locks(sid), MetaStore::Pg(p) => p.renew_locks(sid) }
    }
    fn fence_handle(&self) -> std::sync::Arc<std::sync::atomic::AtomicU64> {
        match self { MetaStore::Sqlite(m) => m.fence_handle(), MetaStore::Pg(p) => p.fence_handle() }
    }
}


struct RawNode {
    id: i64,
    parent_id: Option<i64>,
    name: String,
    is_dir: i64,
    data_ref: Option<String>,
    link_target: Option<String>,
    mode: i64,
    uid: i64,
    gid: i64,
    mtime: i64,
    atime: i64,
    ctime: i64,
    ino: i64,
}

fn row_to_raw(r: &rusqlite::Row) -> rusqlite::Result<RawNode> {
    Ok(RawNode {
        id: r.get(0)?,
        parent_id: r.get(1)?,
        name: r.get(2)?,
        is_dir: r.get(3)?,
        data_ref: r.get(4)?,
        link_target: r.get(5)?,
        mode: r.get(6)?,
        uid: r.get(7)?,
        gid: r.get(8)?,
        mtime: r.get(9)?,
        atime: r.get(10)?,
        ctime: r.get(11)?,
        ino: r.get(12)?,
    })
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

impl From<rusqlite::Error> for MetaError {
    fn from(e: rusqlite::Error) -> Self {
        MetaError::Db(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// rename(a, a) — same directory entry — is a POSIX no-op and must NOT
    /// destroy the file. Guards the bug where the overwrite target lookup
    /// returned the source row, the DELETE removed it, and the trailing UPDATE
    /// matched nothing, silently deleting the entry.
    #[test]
    fn rename_onto_itself_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let e = MetaEngine::open(&dir.path().join("view.db")).unwrap();
        let id = e
            .create_node(ROOT_INO, "f", false, Some("datak"), 0o644, 1000, 1000)
            .unwrap();
        assert!(e.rename(ROOT_INO, "f", ROOT_INO, "f", false).is_ok(), "rename(a,a) must succeed");
        let n = e.lookup(ROOT_INO, "f").expect("entry must survive rename(a,a)");
        assert_eq!(n.id, id, "no-op must keep the same inode");
    }

    /// Renaming a directory over an EXISTING NON-EMPTY directory must fail
    /// ENOTEMPTY and leave both subtrees intact. Guards the bug where the
    /// overwrite path DELETEd the target dir row with no emptiness check,
    /// orphaning its children.
    #[test]
    fn rename_over_nonempty_directory_returns_enotempty() {
        let dir = tempfile::tempdir().unwrap();
        let e = MetaEngine::open(&dir.path().join("view.db")).unwrap();
        let _a = e.create_node(ROOT_INO, "a", true, None, 0o755, 1000, 1000).unwrap();
        let b = e.create_node(ROOT_INO, "b", true, None, 0o755, 1000, 1000).unwrap();
        e.create_node(b, "inside_b", false, Some("k2"), 0o644, 1000, 1000).unwrap();
        let r = e.rename(ROOT_INO, "a", ROOT_INO, "b", false);
        assert!(matches!(r, Err(MetaError::NotEmpty)), "expected NotEmpty, got {r:?}");
        assert!(e.lookup(ROOT_INO, "a").is_some(), "source dir must survive the refused rename");
        assert!(e.lookup(b, "inside_b").is_some(), "target's child must not be orphaned");
    }

    /// Correctness contrast: a file over a dir is TypeMismatch, and a dir over
    /// an EMPTY dir is allowed (target removed, source moved into its place).
    #[test]
    fn rename_over_empty_directory_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let e = MetaEngine::open(&dir.path().join("view.db")).unwrap();
        let _a = e.create_node(ROOT_INO, "a", false, Some("ka"), 0o644, 1000, 1000).unwrap();
        let _b = e.create_node(ROOT_INO, "b", true, None, 0o755, 1000, 1000).unwrap();
        assert!(matches!(
            e.rename(ROOT_INO, "a", ROOT_INO, "b", false),
            Err(MetaError::TypeMismatch)
        ));
        let c = e.create_node(ROOT_INO, "c", true, None, 0o755, 1000, 1000).unwrap();
        assert!(e.rename(ROOT_INO, "c", ROOT_INO, "b", false).is_ok(), "dir over empty dir ok");
        assert!(e.lookup(ROOT_INO, "c").is_none(), "source name gone after move");
        assert_eq!(e.lookup(ROOT_INO, "b").map(|n| n.id), Some(c), "moved dir now holds the name");
    }

    /// Renaming a file over an EXISTING file must report the overwritten target's
    /// data_ref so the caller GCs the orphaned blob (returns it as `Some(key)`).
    /// Guards the space-leak bug where the target row was DELETEd but its storage
    /// object was never freed. A hard-linked target (still-referenced data) must
    /// NOT be reported for GC.
    #[test]
    fn rename_over_file_returns_freed_blob_key() {
        let dir = tempfile::tempdir().unwrap();
        let e = MetaEngine::open(&dir.path().join("view.db")).unwrap();
        e.create_node(ROOT_INO, "src", false, Some("ksrc"), 0o644, 1000, 1000).unwrap();
        e.create_node(ROOT_INO, "dst", false, Some("kdst"), 0o644, 1000, 1000).unwrap();
        let gc = e.rename(ROOT_INO, "src", ROOT_INO, "dst", false).unwrap();
        assert_eq!(gc.as_deref(), Some("kdst"), "overwritten file's blob must be freed");
        assert!(e.lookup(ROOT_INO, "src").is_none(), "source name gone");
        assert_eq!(e.lookup(ROOT_INO, "dst").and_then(|n| n.data_ref).as_deref(), Some("ksrc"), "dst now holds src's data");
    }

    /// A target that is a hard link (its data_ref still has another name after the
    /// overwrite) must NOT be GCd — renaming over it returns None.
    #[test]
    fn rename_over_hardlinked_target_does_not_gc() {
        let dir = tempfile::tempdir().unwrap();
        let e = MetaEngine::open(&dir.path().join("view.db")).unwrap();
        let dst = e.create_node(ROOT_INO, "dst", false, Some("kdst"), 0o644, 1000, 1000).unwrap();
        e.link_node(dst, ROOT_INO, "dst_alias").unwrap(); // second name -> same data_ref
        e.create_node(ROOT_INO, "src", false, Some("ksrc"), 0o644, 1000, 1000).unwrap();
        let gc = e.rename(ROOT_INO, "src", ROOT_INO, "dst", false).unwrap();
        assert_eq!(gc, None, "still-referenced blob must not be freed");
    }

    /// RENAME_NOREPLACE onto an existing name must fail EEXIST and change nothing.
    #[test]
    fn rename_noreplace_over_existing_fails() {
        let dir = tempfile::tempdir().unwrap();
        let e = MetaEngine::open(&dir.path().join("view.db")).unwrap();
        e.create_node(ROOT_INO, "src", false, Some("ksrc"), 0o644, 1000, 1000).unwrap();
        e.create_node(ROOT_INO, "dst", false, Some("kdst"), 0o644, 1000, 1000).unwrap();
        let r = e.rename(ROOT_INO, "src", ROOT_INO, "dst", true);
        assert!(matches!(r, Err(MetaError::Exists)), "expected Exists, got {r:?}");
        assert!(e.lookup(ROOT_INO, "src").is_some(), "noreplace must not move src");
        assert_eq!(e.lookup(ROOT_INO, "dst").and_then(|n| n.data_ref).as_deref(), Some("kdst"), "dst untouched");
    }

    /// RENAME_NOREPLACE onto a free name is a normal move (Ok, no GC).
    #[test]
    fn rename_noreplace_to_free_name_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let e = MetaEngine::open(&dir.path().join("view.db")).unwrap();
        e.create_node(ROOT_INO, "src", false, Some("ksrc"), 0o644, 1000, 1000).unwrap();
        let gc = e.rename(ROOT_INO, "src", ROOT_INO, "fresh", true).unwrap();
        assert_eq!(gc, None, "move to a free name frees nothing");
        assert!(e.lookup(ROOT_INO, "src").is_none());
        assert!(e.lookup(ROOT_INO, "fresh").is_some());
    }

    /// `readdir_page` must stitch back into exactly the same `ORDER BY name`
    /// sequence as the full `readdir`, whatever chunk sizes are used: the FUSE
    /// directory callbacks stream pages to the kernel and resume from a
    /// caller-supplied index, so a boundary mismatch here would drop or
    /// duplicate entries in `ls`.
    #[test]
    fn readdir_page_stitches_into_the_full_sorted_listing() {
        let dir = tempfile::tempdir().unwrap();
        let e = MetaEngine::open(&dir.path().join("view.db")).unwrap();
        for i in 0..100 {
            e.create_node(ROOT_INO, &format!("f{i:03}"), false, Some(&format!("k{i}")), 0o644, 1000, 1000)
                .unwrap();
        }
        e.create_node(ROOT_INO, "sub", true, None, 0o755, 1000, 1000).unwrap();
        let full: Vec<String> = e.readdir(ROOT_INO).into_iter().map(|n| n.name).collect();
        assert_eq!(full.len(), 101, "100 files + 1 subdir");
        for chunk in [1usize, 7, 32, 100, 1024] {
            let mut paged: Vec<String> = Vec::new();
            let mut off = 0usize;
            loop {
                let page = e.readdir_page(ROOT_INO, off, chunk);
                if page.is_empty() {
                    break;
                }
                paged.extend(page.into_iter().map(|n| n.name));
                off += chunk;
            }
            assert_eq!(paged, full, "chunk size {chunk} must reassemble the listing");
        }
        // An offset past the end and a zero limit are empty pages, not panics.
        assert!(e.readdir_page(ROOT_INO, full.len(), 10).is_empty());
        assert!(e.readdir_page(ROOT_INO, 0, 0).is_empty());
    }

    /// Metadata reads are served from pooled side connections, so they must
    /// still observe this engine's own committed writes immediately — `create`
    /// followed by `ls` from another FUSE worker thread is exactly this path.
    #[test]
    fn reads_through_the_pool_observe_committed_writes() {
        let dir = tempfile::tempdir().unwrap();
        let e = MetaEngine::open(&dir.path().join("view.db")).unwrap();
        let id = e.create_node(ROOT_INO, "fresh", false, Some("kfresh"), 0o644, 1000, 1000).unwrap();
        assert_eq!(
            e.lookup(ROOT_INO, "fresh").map(|n| n.id),
            Some(id),
            "lookup must see the insert"
        );
        assert_eq!(
            e.readdir_page(ROOT_INO, 0, 8).into_iter().map(|n| n.name).collect::<Vec<_>>(),
            vec!["fresh".to_string()],
            "paged readdir must see the insert"
        );
        e.unlink(ROOT_INO, "fresh").unwrap();
        assert!(e.lookup(ROOT_INO, "fresh").is_none(), "lookup must see the delete");
        assert!(e.readdir_page(ROOT_INO, 0, 8).is_empty(), "paged readdir must see the delete");
    }

    /// `GroveFS` keys both the READDIRPLUS negotiation and its fetch chunking off
    /// `supports_paged_readdir`, so that flag is a contract, not a hint: an
    /// engine that pages *inside its own store* must advertise it, and one that
    /// cannot must keep the safe default (one full listing per kernel buffer, no
    /// READDIRPLUS) instead of silently multiplying full fetches. The SQLite
    /// engine pages; the server-side `MetaStore` wrapper forwards that so its
    /// `M_READDIRP` RPC handler answers a real page; a derived [`LayoutMeta`]
    /// lens cannot express its hidden/mapped shape as one SQL page, so it stays
    /// on the default.
    #[test]
    fn store_level_paging_is_advertised_only_by_providers_that_page_in_sql() {
        let dir = tempfile::tempdir().unwrap();
        let eng: std::sync::Arc<dyn MetaProvider> = std::sync::Arc::new(
            MetaEngine::open(&dir.path().join("view.db")).unwrap(),
        );
        assert!(eng.supports_paged_readdir(), "SQLite pages with LIMIT/OFFSET");

        // The server-side wrapper now forwards paging method-by-method, so the
        // meta-server can serve M_READDIRP as a real sub-query — and, gated on
        // this flag, offer READDIRPLUS to its clients.
        let store: std::sync::Arc<dyn MetaProvider> = std::sync::Arc::new(MetaStore::Sqlite(
            MetaEngine::open(&dir.path().join("other.db")).unwrap(),
        ));
        assert!(store.supports_paged_readdir(), "wrapper forwards the SQLite engine's paging");
        // Forwarding is not just the flag: the pages must stitch back into the
        // exact full listing `readdir` returns.
        for _ in 0..5 {
            store.create_node(ROOT_INO, "n", false, None, 0o644, 0, 0).ok();
        }
        let full: Vec<String> = store.readdir(ROOT_INO).into_iter().map(|n| n.name).collect();
        let mut stitched = Vec::new();
        let mut off = 0usize;
        loop {
            let page = store.readdir_page(ROOT_INO, off, 2);
            if page.is_empty() {
                break;
            }
            stitched.extend(page.into_iter().map(|n| n.name));
            off += 2;
        }
        assert_eq!(stitched, full, "forwarded readdir_page stitches into readdir");

        // A derived lens over the same engine keeps the trait default (false):
        // its shaped view has no single SQL page equivalent, so it is not
        // offered READDIRPLUS and answers each buffer with one whole listing.
        let lens: std::sync::Arc<dyn MetaProvider> = std::sync::Arc::new(
            crate::layout::LayoutMeta::new(eng.clone(), crate::layout::LayoutRules::default()),
        );
        assert!(!lens.supports_paged_readdir(), "a derived lens cannot page in SQL");
    }
}

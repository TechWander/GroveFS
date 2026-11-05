//! PostgreSQL metadata backend — a second authoritative store beside the
//! embedded SQLite [`crate::meta::MetaEngine`].
//!
//! It implements the *same* [`crate::meta::MetaProvider`] surface the FUSE layer
//! and the central `meta-server` consume, so switching a `meta-server` to PG (or
//! a local `mount` to PG) is invisible to clients. The view tree, hard links,
//! refcount GC and POSIX advisory locks behave exactly as on SQLite; only the
//! storage engine differs — and, on this backend, the **write concurrency
//! model**.
//!
//! Mapping from the SQLite schema:
//! - one `<home>/meta/<view>.db` file  ->  one schema `grovefs_v_<hex(view)>`
//!   (a deterministic, injection-safe identifier; the per-view sequence keeps
//!   `id`/`ino` small exactly like the file-per-view model, and the root row is
//!   pinned at id 1 so [`crate::meta::ROOT_INO`] stays valid);
//! - `INTEGER PRIMARY KEY` (rowid) -> `BIGSERIAL PRIMARY KEY`;
//! - `INSERT OR IGNORE` -> `ON CONFLICT DO NOTHING`;
//! - `TransactionBehavior::Immediate` + single-writer mutex -> per-op
//!   `SERIALIZABLE` transaction on a pooled writer connection, with automatic
//!   retry on `SQLSTATE 40001 serialization_failure` / `40P01
//!   deadlock_detected`. This is the HopsFS route (Sharma et al., FAST'18)
//!   implemented on top of PostgreSQL's own SSI (Ports & O'Neil, OSDI'12),
//!   adapted to a single-cluster deployment: no namespace partitioning, no
//!   cross-server 2PC.
//! - `busy_timeout` cross-process wait -> SSI retry with bounded exponential
//!   backoff (200us..25ms, up to 12 attempts).
//!
//! Why SERIALIZABLE and not READ COMMITTED: GroveFS's governance invariants
//! I1–I4 (see §5 of the TOS manuscript) are safety properties about the final
//! name-space shape — one object per identity, no orphan data_ref, atomically
//! applied renames, and post-commit visibility. Under a serialisable schedule
//! every such invariant follows from its truth in *some* serial order — the
//! standard proof pattern that lets HopsFS/Spanner drop the single-writer
//! assumption without giving up correctness. Concurrent `create_node` calls on
//! different parents touch disjoint `(parent_id, name)` rows and never abort;
//! `rename` walks the source/target rows plus the ancestor chain in one tx and
//! loses one side of a cycle against a concurrent `rename`, which then retries.
//!
//! Honest boundary: the GroveFS-native HA (`--role/--peer/--witness` snapshot
//! shipping, built on SQLite `VACUUM INTO`) is NOT offered on the PG path —
//! Postgres replicates itself (streaming replication / PITR), which is strictly
//! better than the file-snapshot mechanism. So [`PgEngine::backup_to_bytes`]
//! reports unsupported and a `meta-server --store postgres://...` refuses the HA
//! flags. Everything else is a drop-in.

use crate::meta::{LockConflict, LockType, MetaError, MetaProvider, NodeInfo, NodeKind};
use crate::stats;
use postgres::types::ToSql;
use postgres::{Client, NoTls};
use std::collections::HashMap;
use std::path::Path;

/// Default lease for a lock row (seconds), matching [`crate::meta::LockLease`].
fn lease_secs() -> i64 {
    crate::meta::LockLease::secs()
}

fn now() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Deterministic, injection-safe PG schema name for a view: `grovefs_v_` +
/// lowercase hex of the raw view bytes. Hex guarantees only `[0-9a-f]` appear,
/// so the identifier never needs quoting and can't be forged with quotes/dots.
/// A view name longer than 25 bytes (-> schema name > 63) is Postgres's
/// identifier-length limit; such names are rejected at open with a clear error.
fn schema_for(view: &str) -> Option<String> {
    if view.is_empty() || view.len() > 25 {
        return None;
    }
    let mut hex = String::with_capacity(view.len() * 2);
    for b in view.as_bytes() {
        hex.push_str(&format!("{b:02x}"));
    }
    Some(format!("grovefs_v_{hex}"))
}

/// One row of the `node` table before nlink/size are computed (the `finalize`
/// step). Mirrors the SQLite engine's private RawNode.
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

const COLS: &str =
    "id, parent_id, name, is_dir, data_ref, link_target, mode, uid, gid, mtime, atime, ctime, ino";

/// Shared cross-view holder ledger, living in its own schema so all views book
/// one set of counts (see `ensure_schema`). Mirrors SQLite's `<meta>/refs.db`.
const PG_REFS_SCHEMA: &str = "grovefs_refs";
const PG_REFS_TABLE: &str = "grovefs_refs.holders";

fn parse_node(r: &postgres::Row) -> RawNode {
    RawNode {
        id: r.get(0),
        parent_id: r.get(1),
        name: r.get(2),
        is_dir: r.get(3),
        data_ref: r.get(4),
        link_target: r.get(5),
        mode: r.get(6),
        uid: r.get(7),
        gid: r.get(8),
        mtime: r.get(9),
        atime: r.get(10),
        ctime: r.get(11),
        ino: r.get::<_, Option<i64>>(12).unwrap_or(0),
    }
}

/// A [`MetaProvider`] backed by PostgreSQL, with **per-op SERIALIZABLE
/// transactions** checked out from a bounded writer pool. Two writers on
/// disjoint `(parent_id, name)` rows never block each other; two writers whose
/// reads/writes overlap get an SSI abort on one side and transparently retry
/// (see [`PgEngine::with_serial_retry`]). This is the professional scalable-
/// metadata route pioneered by HopsFS (Sharma et al., FAST'18) and made
/// practical here by delegating the transactional substrate to PostgreSQL's
/// own SSI (Ports & O'Neil, OSDI'12) instead of a bespoke FoundationDB stack.
///
/// The SQLite [`crate::meta::MetaEngine`] keeps its single-writer mutex
/// unchanged: a `meta-server` running without `--store` still serialises all
/// writes for its one view's `.db`, so this change is strictly opt-in and
/// never regress the embedded path.
pub struct PgEngine {
    view: String,
    schema: String,
    /// Kept so we can transparently re-establish a connection after a
    /// server-side close / network blip; without this the view becomes
    /// permanently unusable until the meta-server process restarts.
    conn_uri: String,
    /// Pool of writer connections, each carrying
    /// `default_transaction_isolation = 'serializable'` at the session level.
    /// A [`Self::checkout_writer`] hands one to the calling thread; every
    /// mutation runs its per-op `Transaction` on that borrowed client, so
    /// concurrent mutators on disjoint rows genuinely run in parallel PG
    /// backends. Bounded so a busy meta-server does not exhaust `max_connections`.
    write_pool: parking_lot::Mutex<Vec<Client>>,
    /// Pooled read connections so concurrent metadata reads (lookup/getattr/
    /// readdir) fan out at READ COMMITTED (cheaper — no SSI predicate locks)
    /// instead of queueing behind an SSI writer. A fresh PostgreSQL backend
    /// started by `Client::connect` sees committed writes in its own read
    /// transaction, so read-your-writes holds.
    read_pool: parking_lot::Mutex<Vec<Client>>,
    /// Keyset cursors for sequential READDIR paging, mirroring the SQLite
    /// engine: `(parent, checkpoint offset)` -> the name of the row before it.
    /// Cleared by any name mutation; a miss falls back to `LIMIT/OFFSET`.
    cursors: parking_lot::Mutex<HashMap<(i64, usize), String>>,
}

/// Ceiling on those cursors (one checkpoint per [`READDIR_CURSOR_STRIDE`] rows
/// per open directory).
const READDIR_CURSOR_CAP: usize = 65_536;
/// Checkpoint granularity: the kernel resumes wherever its reply buffer filled,
/// which rarely lands on our own page boundary (see the SQLite engine).
const READDIR_CURSOR_STRIDE: usize = 32;

/// Number of pooled writer connections per PG view engine. Bounded by
/// `max_connections` on the server (default 100); a small per-view pool keeps
/// total backend pressure predictable when the meta-server hosts many views.
const PG_WRITE_POOL_SIZE: usize = 8;

/// Number of pooled read connections opened per PG view engine, matching the
/// SQLite engine's [`crate::meta::READ_POOL_SIZE`].
const PG_READ_POOL_SIZE: usize = 4;

/// Upper bound on SSI retries per write op. Chosen so that a hot rename
/// contention across 8 writers still terminates within ~1 second while
/// allowing genuinely-conflicting schedules (usually 2–3 rounds suffice).
const SERIALIZATION_MAX_ATTEMPTS: u32 = 12;

/// Initial SSI-retry backoff, doubling up to [`SERIALIZATION_BACKOFF_MAX`].
/// Starting at 200us keeps uncontended retries sub-millisecond; the cap at
/// 25ms prevents the tail latency from blowing up under sustained contention.
const SERIALIZATION_BACKOFF_MIN: std::time::Duration = std::time::Duration::from_micros(200);
const SERIALIZATION_BACKOFF_MAX: std::time::Duration = std::time::Duration::from_millis(25);

/// Session-level pragma applied to every writer connection so PG's default
/// `BEGIN` (implicit or via `Client::transaction()`) opens under SSI.
const WRITER_ISOLATION_SQL: &str = "SET default_transaction_isolation = 'serializable'";

impl PgEngine {
    /// Connect to a PostgreSQL server (from a `postgres://user:pw@host/db`
    /// URI), ensure this view's schema exists (idempotent), and seed the root
    /// row (id=1). The URI is passed through verbatim to [`postgres::Client`].
    ///
    /// `readonly` is accepted for API parity with the SQLite engine but has no
    /// effect on the PG path (GroveFS-native HA is disabled on PG, so a
    /// "standby reads only" mode is meaningless here).
    pub fn open(conn_uri: &str, view: &str, _readonly: bool) -> std::io::Result<Self> {
        let schema = schema_for(view).ok_or_else(|| {
            std::io::Error::other(format!(
                "view name '{view}' incompatible with PG schema naming (empty, or longer than 25 bytes)"
            ))
        })?;
        // Pre-fill the writer pool: every entry carries the session-level
        // SERIALIZABLE pragma so any `Client::transaction()` inherits SSI.
        // A failed extra connection just shrinks the pool (writes fall back to
        // a fresh connect in `checkout_writer`), never fails open().
        let mut write_pool = Vec::new();
        for _ in 0..PG_WRITE_POOL_SIZE {
            if let Ok(mut wc) = Client::connect(conn_uri, NoTls) {
                if wc.batch_execute(WRITER_ISOLATION_SQL).is_ok() {
                    write_pool.push(wc);
                }
            }
        }
        // Pre-fill the read pool so the first big-directory scan doesn't pay
        // connect latency per page. Reads stay at READ COMMITTED (the PG
        // default) — cheaper and no SSI predicate-lock overhead.
        let mut read_pool = Vec::new();
        for _ in 0..PG_READ_POOL_SIZE {
            if let Ok(rc) = Client::connect(conn_uri, NoTls) {
                read_pool.push(rc);
            }
        }
        let this = Self {
            view: view.to_string(),
            schema,
            conn_uri: conn_uri.to_string(),
            write_pool: parking_lot::Mutex::new(write_pool),
            read_pool: parking_lot::Mutex::new(read_pool),
            cursors: parking_lot::Mutex::new(HashMap::new()),
        };
        // Ensure the schema exists through the normal writer checkout: if
        // every pool connect above failed, `checkout_writer` opens a fresh
        // one; a real outage then surfaces as the mapped io_error the caller
        // already expects.
        this.ensure_schema()?;
        Ok(this)
    }

    /// Convenience for tests/CLI: build the URI from pieces (host:port, user,
    /// password, database). Same effect as [`Self::open`] with a URI string.
    pub fn open_parts(
        host: &str,
        port: u16,
        user: &str,
        password: &str,
        database: &str,
        view: &str,
    ) -> std::io::Result<Self> {
        let uri = format!("postgres://{user}:{password}@{host}:{port}/{database}");
        Self::open(&uri, view, false)
    }

    /// The view this engine serves (used by the central server's per-view log
    /// lines / error reporting).
    pub fn view(&self) -> &str {
        &self.view
    }

    /// The path this engine would use if it were SQLite — used by the server
    /// for cache-eviction bookkeeping when a fresh HA snapshot lands. Not
    /// meaningful on PG (no per-view file), but present so callers can log it.
    pub fn pseudo_path(&self, base: &Path) -> std::path::PathBuf {
        base.join("meta").join(format!("{}.pg", self.view))
    }

    fn t(&self, table: &str) -> String {
        format!("{}.{}", self.schema, table)
    }

    /// Check out a writer connection: prefer one from the pool (transparently
    /// reconnecting if the pooled client got closed by an idle timeout / PG
    /// restart), else open a fresh session with the SERIALIZABLE pragma. On
    /// total outage returns a mapped MetaError so the caller surfaces it.
    ///
    /// The returned `Client` is *owned* by the caller and must be handed back
    /// via [`Self::requeue_writer`] to keep the pool bounded. Not doing so
    /// leaks one backend per dropped checkout until PG's `max_connections`
    /// trips — bounded usage makes that pattern visible at review time.
    fn checkout_writer(&self) -> Result<Client, MetaError> {
        // Pool-wait window: covers popping a warm writer AND the cold path
        // where the pool is drained and we pay a full `Client::connect` — that
        // fresh-connect latency shows up as the PgPoolWait tail, telling an
        // operator the writer pool is too small for the current concurrency.
        let _pw = stats::start(stats::Stage::PgPoolWait);
        let pooled = {
            let mut p = self.write_pool.lock();
            loop {
                match p.pop() {
                    Some(c) if c.is_closed() => continue, // discard stale, try next
                    Some(c) => break Some(c),
                    None => break None,
                }
            }
        };
        match pooled {
            Some(c) => Ok(c),
            None => {
                let mut c = Client::connect(&self.conn_uri, NoTls).map_err(|e| map_db_err(&e))?;
                // Fresh connect may bypass the pooled PRAGMA path; re-arm so
                // every writer session is guaranteed SERIALIZABLE regardless
                // of how it entered the pool.
                let _ = c.batch_execute(WRITER_ISOLATION_SQL);
                Ok(c)
            }
        }
    }

    /// Return a writer connection to the pool (bounded); drop it if the
    /// server closed the socket so a stale handle never gets handed out again.
    fn requeue_writer(&self, c: Client) {
        if c.is_closed() {
            return;
        }
        let mut p = self.write_pool.lock();
        if p.len() < PG_WRITE_POOL_SIZE {
            p.push(c);
        }
    }

    /// Borrow a pooled read connection if one is free (skipping any closed ones
    /// left by a server restart), else open a fresh one. Returns `None` only
    /// when connecting also fails — the caller then falls back to a checked-out
    /// writer (see [`Self::with_read`]).
    fn try_read_conn(&self) -> Option<Client> {
        let _pw = stats::start(stats::Stage::PgPoolWait);
        let pooled = {
            let mut p = self.read_pool.lock();
            loop {
                match p.pop() {
                    Some(c) if c.is_closed() => continue, // discard stale, try next
                    Some(c) => break Some(c),
                    None => break None,
                }
            }
        };
        match pooled {
            Some(c) => Some(c),
            None => Client::connect(&self.conn_uri, NoTls).ok(),
        }
    }

    /// Return a used read connection to the pool (bounded); drop it if the
    /// server closed it so a stale socket never gets handed out again.
    fn requeue_read(&self, c: Client) {
        if c.is_closed() {
            return;
        }
        let mut p = self.read_pool.lock();
        if p.len() < PG_READ_POOL_SIZE {
            p.push(c);
        }
    }

    /// Run one SELECT-only op on a read-pool connection, fanning concurrent
    /// reads out across the pool. Falls back to a checked-out writer when no
    /// read connection can be obtained (server down on the read path), so
    /// behaviour degrades to the pre-pool path rather than failing reads that
    /// would otherwise work. `T: Default` lets the total-outage branch return
    /// an empty/None result rather than propagating an error type that would
    /// need threading through every read helper.
    fn with_read<T: Default>(&self, f: impl FnOnce(&mut Client) -> T) -> T {
        if let Some(mut c) = self.try_read_conn() {
            let _q = stats::start(stats::Stage::PgQuery);
            let r = f(&mut c);
            self.requeue_read(c);
            return r;
        }
        match self.checkout_writer() {
            Ok(mut c) => {
                let _q = stats::start(stats::Stage::PgQuery);
                let r = f(&mut c);
                self.requeue_writer(c);
                r
            }
            Err(_) => T::default(),
        }
    }

    fn ensure_schema(&self) -> std::io::Result<()> {
        let mut c = self
            .checkout_writer()
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        // CREATE SCHEMA takes the raw identifier (no dots).
        c.batch_execute(&format!("CREATE SCHEMA IF NOT EXISTS {}", self.schema))
            .map_err(io_e)?;
        let node = self.t("node");
        let locks = self.t("locks");
        let ctrl = self.t("ctrl");
        c.batch_execute(&format!(
            "CREATE TABLE IF NOT EXISTS {node} (
                 id           BIGSERIAL   PRIMARY KEY,
                 parent_id    BIGINT,
                 name         TEXT        NOT NULL,
                 is_dir       BIGINT      NOT NULL,
                 data_ref     TEXT,
                 link_target  TEXT,
                 mode         BIGINT      NOT NULL,
                 uid          BIGINT      NOT NULL,
                 gid          BIGINT      NOT NULL,
                 mtime        BIGINT      NOT NULL,
                 atime        BIGINT      NOT NULL,
                 ctime        BIGINT      NOT NULL,
                 ino          BIGINT,
                 UNIQUE(parent_id, name)
             );
             CREATE INDEX IF NOT EXISTS node_parent  ON {node}(parent_id);
             -- Directory nlink (2 + subdirs) counts the parent's rows filtered
             -- by is_dir; carrying is_dir in the key lets that COUNT stay inside
             -- the index instead of re-checking every child heap row.
             CREATE INDEX IF NOT EXISTS node_parent_kind ON {node}(parent_id, is_dir);
             CREATE INDEX IF NOT EXISTS node_dataref ON {node}(data_ref);
             CREATE INDEX IF NOT EXISTS node_ino     ON {node}(ino);
             CREATE TABLE IF NOT EXISTS {ctrl} (k TEXT PRIMARY KEY, v BIGINT NOT NULL);
             CREATE TABLE IF NOT EXISTS {locks} (
                 id          BIGSERIAL PRIMARY KEY,
                 ino         BIGINT NOT NULL,
                 owner       BIGINT NOT NULL,
                 sid         BIGINT NOT NULL DEFAULT 0,
                 l_start     BIGINT NOT NULL,
                 l_end       BIGINT NOT NULL,
                 pid         BIGINT NOT NULL,
                 ltype       BIGINT NOT NULL,
                 expires_at  BIGINT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS locks_ino    ON {locks}(ino, l_start, l_end);
             CREATE INDEX IF NOT EXISTS locks_owner  ON {locks}(owner);
             CREATE INDEX IF NOT EXISTS locks_sid    ON {locks}(sid);
             CREATE INDEX IF NOT EXISTS locks_expire ON {locks}(expires_at);
             -- Shared cross-view blob holder ledger: one row per data_ref that
             -- SOME view holds with its last name. Lives in its own schema, not
             -- the per-view schema, so every view books/unbooks the same counts
             -- (mirrors SQLite's <meta>/refs.db). GC is only safe when a view's
             -- last name for a key leaves AND this count reaches zero.
             CREATE SCHEMA IF NOT EXISTS {PG_REFS_SCHEMA};
             CREATE TABLE IF NOT EXISTS {PG_REFS_TABLE} (
                 data_ref TEXT PRIMARY KEY,
                 holders  BIGINT NOT NULL
             );"
        ))
        .map_err(io_e)?;
        // Seed the root (id 1) if absent, then align the BIGSERIAL sequence so
        // the next INSERT starts at max(id)+1 (the explicit id=1 didn't advance
        // the sequence and would otherwise collide with the next default).
        let t = now();
        c.execute(
            &format!(
                "INSERT INTO {node} (id, parent_id, name, is_dir, data_ref, link_target, mode, uid, gid, mtime, atime, ctime, ino)
                 VALUES (1, NULL, '/', 1, NULL, NULL, 511, 0, 0, $1, $1, $1, 1)
                 ON CONFLICT (id) DO NOTHING",
            ),
            &[&t],
        )
        .map_err(io_e)?;
        c.execute(
            &format!(
                "SELECT setval(pg_get_serial_sequence('{node}','id'), (SELECT COALESCE(MAX(id),1) FROM {node}))"
            ),
            &[],
        )
        .map_err(io_e)?;
        self.requeue_writer(c);
        Ok(())
    }

    /// Classify a `postgres::Error` as an SSI retry trigger. Only the two
    /// server signalled codes are retried transparently: 40001
    /// `serialization_failure` (SSI abort) and 40P01 `deadlock_detected`
    /// (traditional deadlock, PG picks one side). Everything else (unique
    /// violation, foreign key, syntax, connection loss) propagates so callers
    /// see POSIX-visible errors like `EEXIST` unchanged.
    fn is_serialization_failure(e: &postgres::Error) -> bool {
        matches!(
            e.code(),
            Some(&postgres::error::SqlState::T_R_SERIALIZATION_FAILURE)
                | Some(&postgres::error::SqlState::T_R_DEADLOCK_DETECTED)
        )
    }

    /// Run one write op as a per-attempt `SERIALIZABLE` transaction on a
    /// pooled writer, with bounded exponential-backoff retry on SSI abort.
    ///
    /// `body` receives a `&mut Transaction` (already opened under the
    /// session-level SERIALIZABLE pragma applied at pool-entry creation time)
    /// and returns either:
    /// - `Ok(v)`             — the helper `commit()`s and returns `Ok(v)`;
    ///   if commit itself hits 40001 the whole op is retried;
    /// - `Err(WriteErr::Serialization)` — the tx is dropped (auto-rollback) and
    ///   the whole body is retried;
    /// - `Err(WriteErr::Hard(me))`      — the tx is dropped and `me` is
    ///   propagated unchanged (POSIX-visible failures like `EEXIST`/`ENOTEMPTY`).
    ///
    /// Retry budget is [`SERIALIZATION_MAX_ATTEMPTS`] with 200us..25ms
    /// doubling backoff; on exhaustion a `MetaError::Db` describes the last
    /// serialization state so operators see a real outage rather than a
    /// silent stall.
    fn with_serial_retry<T, F>(&self, mut body: F) -> Result<T, MetaError>
    where
        F: FnMut(&mut postgres::Transaction<'_>) -> Result<T, WriteErr>,
    {
        let mut delay = SERIALIZATION_BACKOFF_MIN;
        // Total write-transaction latency, retries included: the number that
        // actually lands on the caller's tail. SSI aborts inflate this well
        // above PgQuery/PgPoolWait, so comparing the two pinpoints contention.
        let _txn = stats::start(stats::Stage::PgWriteTxn);
        for attempt in 0..SERIALIZATION_MAX_ATTEMPTS {
            let mut c = self.checkout_writer()?;
            let out: Result<T, WriteErr> = (|| {
                let mut tx = c.transaction().map_err(|e| {
                    if Self::is_serialization_failure(&e) {
                        WriteErr::Serialization
                    } else {
                        WriteErr::Hard(map_db_err(&e))
                    }
                })?;
                let v = body(&mut tx)?;
                tx.commit().map_err(|e| {
                    if Self::is_serialization_failure(&e) {
                        WriteErr::Serialization
                    } else {
                        WriteErr::Hard(map_db_err(&e))
                    }
                })?;
                Ok(v)
            })();
            self.requeue_writer(c);
            match out {
                Ok(v) => return Ok(v),
                Err(WriteErr::Hard(me)) => return Err(me),
                Err(WriteErr::Serialization) => {
                    // Each SSI/deadlock abort is both a count (how often
                    // optimistic concurrency is contending) and a timed backoff
                    // window (how much retry stall that contention costs).
                    stats::count(stats::Counter::PgSsiRetry, 1);
                    let _rt = stats::start(stats::Stage::PgSsiRetry);
                    std::thread::sleep(delay);
                    delay = (delay * 2).min(SERIALIZATION_BACKOFF_MAX);
                    let _ = attempt; // attempts exhausted -> terminal MetaError below
                }
            }
        }
        Err(MetaError::Db(format!(
            "serialization retry exhausted after {SERIALIZATION_MAX_ATTEMPTS} attempts (view '{}')",
            self.view
        )))
    }

    // ---- read paths -------------------------------------------------------

    /// Classify a raw row into a node kind. Shared by the scalar and batched
    /// finalize paths so the two can never diverge.
    fn node_kind(raw: &RawNode) -> NodeKind {
        if raw.is_dir == 1 {
            NodeKind::Dir
        } else if raw.link_target.is_some() {
            NodeKind::Symlink
        } else {
            NodeKind::Regular
        }
    }

    /// Assemble a `NodeInfo` from a raw row plus the already-computed nlink and
    /// size. Pure (no store access); the single source of truth for the field
    /// mapping, used by both finalize paths.
    fn build_info(raw: &RawNode, kind: NodeKind, nlink: u64, size: u64) -> NodeInfo {
        NodeInfo {
            id: raw.ino,
            parent_id: raw.parent_id.unwrap_or(0),
            name: raw.name.clone(),
            kind,
            data_ref: raw.data_ref.clone(),
            link_target: raw.link_target.clone(),
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

    /// Compute nlink/size for ONE raw row and produce the NodeInfo. Runs on the
    /// caller's already-held connection to avoid re-locking. The directory-
    /// listing path uses [`Self::finalize_batch`] instead, which collapses the
    /// per-row `COUNT` this issues into a single grouped query.
    fn finalize(&self, c: &mut Client, raw: RawNode) -> NodeInfo {
        let kind = Self::node_kind(&raw);
        let node = self.t("node");
        let (nlink, size) = match kind {
            NodeKind::Dir => {
                let subdirs: i64 = c
                    .query(
                        &format!("SELECT COUNT(*) FROM {node} WHERE parent_id=$1 AND is_dir=1"),
                        &[&raw.id],
                    )
                    .map(|rows| rows.first().map(|r| r.get::<_, i64>(0)).unwrap_or(0))
                    .unwrap_or(0);
                (2 + subdirs as u64, 4096u64)
            }
            NodeKind::Symlink => (
                1,
                raw.link_target
                    .as_ref()
                    .map(|s| s.len() as u64)
                    .unwrap_or(0),
            ),
            NodeKind::Regular => {
                let links: i64 = c
                    .query(
                        &format!("SELECT COUNT(*) FROM {node} WHERE ino=$1"),
                        &[&raw.ino],
                    )
                    .map(|rows| rows.first().map(|r| r.get::<_, i64>(0)).unwrap_or(1))
                    .unwrap_or(1);
                (links.max(1) as u64, 0u64)
            }
        };
        Self::build_info(&raw, kind, nlink, size)
    }

    /// Finalize a whole directory listing with at most two grouped `COUNT`
    /// queries — one for every regular file's hard-link count, one for every
    /// directory's sub-directory count — instead of one `COUNT` per entry. Runs
    /// on the caller's already-held connection. The key sets are passed as
    /// arrays via `= ANY($1)`, so a large directory needs no client-side chunking
    /// (unlike the SQLite engine's `IN (...)`). Symlinks need no query. Output
    /// is identical to mapping the scalar [`Self::finalize`] over the same rows:
    /// an inode's hard-link count is global, a directory's nlink is `2 +` its
    /// count of directory children.
    fn finalize_batch(&self, c: &mut Client, raws: &[RawNode]) -> Vec<NodeInfo> {
        let node = self.t("node");
        let mut dir_ids: Vec<i64> = Vec::new();
        let mut reg_inos: Vec<i64> = Vec::new();
        for r in raws {
            match Self::node_kind(r) {
                NodeKind::Dir => dir_ids.push(r.id),
                NodeKind::Regular => reg_inos.push(r.ino),
                NodeKind::Symlink => {}
            }
        }
        let mut nlink_map: HashMap<i64, i64> = HashMap::new();
        let mut subdir_map: HashMap<i64, i64> = HashMap::new();
        if !reg_inos.is_empty() {
            if let Ok(rows) = c.query(
                &format!("SELECT ino, COUNT(*) FROM {node} WHERE ino = ANY($1) GROUP BY ino"),
                &[&reg_inos],
            ) {
                for r in &rows {
                    nlink_map.insert(r.get::<_, i64>(0), r.get::<_, i64>(1));
                }
            }
        }
        if !dir_ids.is_empty() {
            if let Ok(rows) = c.query(
                &format!(
                    "SELECT parent_id, COUNT(*) FROM {node} WHERE parent_id = ANY($1) AND is_dir=1 GROUP BY parent_id"
                ),
                &[&dir_ids],
            ) {
                for r in &rows {
                    subdir_map.insert(r.get::<_, i64>(0), r.get::<_, i64>(1));
                }
            }
        }
        raws.iter()
            .map(|r| {
                let kind = Self::node_kind(r);
                let (nlink, size) = match kind {
                    NodeKind::Dir => (2 + *subdir_map.get(&r.id).unwrap_or(&0) as u64, 4096u64),
                    NodeKind::Symlink => (
                        1,
                        r.link_target.as_ref().map(|s| s.len() as u64).unwrap_or(0),
                    ),
                    NodeKind::Regular => {
                        let links = *nlink_map.get(&r.ino).unwrap_or(&1);
                        (links.max(1) as u64, 0u64)
                    }
                };
                Self::build_info(r, kind, nlink, size)
            })
            .collect()
    }

    pub fn lookup(&self, parent: i64, name: &str) -> Option<NodeInfo> {
        let _t = stats::start(stats::Stage::MetaLookup);
        let node = self.t("node");
        let sql = format!("SELECT {COLS} FROM {node} WHERE parent_id=$1 AND name=$2");
        self.with_read(|c| {
            let raw = c
                .query(&sql, &[&parent, &name])
                .ok()
                .and_then(|rows| rows.into_iter().next())
                .map(|r| parse_node(&r))?;
            Some(self.finalize(c, raw))
        })
    }

    pub fn getattr(&self, ino: i64) -> Option<NodeInfo> {
        let _t = stats::start(stats::Stage::MetaGetattr);
        let node = self.t("node");
        let sql = format!("SELECT {COLS} FROM {node} WHERE ino=$1 LIMIT 1");
        self.with_read(|c| {
            let raw = c
                .query(&sql, &[&ino])
                .ok()
                .and_then(|rows| rows.into_iter().next())
                .map(|r| parse_node(&r))?;
            Some(self.finalize(c, raw))
        })
    }

    pub fn readdir(&self, parent: i64) -> Vec<NodeInfo> {
        let _t = stats::start(stats::Stage::MetaReaddir);
        let node = self.t("node");
        let sql = format!("SELECT {COLS} FROM {node} WHERE parent_id=$1 AND name>'' ORDER BY name");
        self.with_read(|c| {
            let rows = match c.query(&sql, &[&parent]) {
                Ok(r) => r,
                Err(_) => return Vec::new(),
            };
            let raws: Vec<RawNode> = rows.iter().map(parse_node).collect();
            self.finalize_batch(c, &raws)
        })
    }

    /// Page `offset..offset+limit` of the `ORDER BY name` listing, pushed into
    /// SQL with `LIMIT/OFFSET`. Same O(buffer) win as the SQLite engine: one
    /// small query per kernel READDIR buffer (not a full re-scan of the whole
    /// directory), with the page's entries finalized together by
    /// [`Self::finalize_batch`] — a couple of grouped `COUNT`s, not one per
    /// entry.
    pub fn readdir_page(&self, parent: i64, offset: usize, limit: usize) -> Vec<NodeInfo> {
        let _t = stats::start(stats::Stage::MetaReaddirPage);
        // Same strided-checkpoint trick as the SQLite engine: resume from the
        // nearest checkpoint at or before `offset` so the walk costs
        // O((limit+stride)*log n) per buffer instead of O(offset); a missing
        // checkpoint keeps the OFFSET form (seekdir).
        let base = offset - offset % READDIR_CURSOR_STRIDE;
        let after = if offset == 0 {
            None
        } else {
            self.cursors.lock().get(&(parent, base)).cloned()
        };
        let start = if after.is_some() { base } else { offset };
        let skip = offset - start;
        let fetch = limit.saturating_add(skip);
        let node = self.t("node");
        let fetch_i = fetch as i64;
        let start_i = start as i64;
        let (sql, args): (String, Vec<&(dyn ToSql + Sync)>) = match &after {
            Some(last) => (
                format!(
                    "SELECT {COLS} FROM {node} WHERE parent_id=$1 AND name>$2 ORDER BY name LIMIT $3"
                ),
                vec![&parent, last, &fetch_i],
            ),
            None => (
                // Same plan-pinning no-op as the SQLite engine: a name is never
                // empty, and the range keeps the walk on the
                // UNIQUE(parent_id,name) index in `name` order.
                format!(
                    "SELECT {COLS} FROM {node} WHERE parent_id=$1 AND name>'' ORDER BY name LIMIT $2 OFFSET $3"
                ),
                vec![&parent, &fetch_i, &start_i],
            ),
        };
        let mut out = self.with_read(|c| {
            let rows = match c.query(&sql, &args[..]) {
                Ok(r) => r,
                Err(_) => return Vec::new(),
            };
            let raws: Vec<RawNode> = rows.iter().map(parse_node).collect();
            self.finalize_batch(c, &raws)
        });
        if skip > 0 {
            out.drain(..skip.min(out.len()));
        }
        // Checkpoint `(parent, n)` -> the name of row n-1 every STRIDE rows.
        {
            let mut c = self.cursors.lock();
            for (i, row) in out.iter().enumerate() {
                let next = offset + i + 1;
                if next.is_multiple_of(READDIR_CURSOR_STRIDE) {
                    if c.len() >= READDIR_CURSOR_CAP {
                        c.clear();
                    }
                    c.insert((parent, next), row.name.clone());
                }
            }
        }
        out
    }

    /// Drop the paging cursors: any name change can invalidate the "resume
    /// after this name" position a reader is holding.
    fn drop_cursors(&self) {
        let mut c = self.cursors.lock();
        if !c.is_empty() {
            c.clear();
        }
    }

    // ---- write paths ------------------------------------------------------
    //
    // Every mutation below runs through [`Self::with_serial_retry`]: one
    // `SERIALIZABLE` transaction per op, checked out from the writer pool,
    // retried transparently on `SQLSTATE 40001/40P01`. Two concurrent
    // `create_node` calls on DIFFERENT parents commit in parallel because
    // they touch disjoint `(parent_id, name)` rows and PG's SSI does not
    // consider non-overlapping write sets in conflict; two `rename` calls
    // whose source/target sets intersect abort one side and re-run it.

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
        self.drop_cursors();
        let t = now();
        let node = self.t("node");
        // Insert + set `ino = id` must be atomic: otherwise a mid-op PG
        // disconnect or SSI abort would leave the row with `ino IS NULL`,
        // which `parse_node` reads as 0 and breaks every FUSE lookup on
        // that name. Two simultaneous INSERTs on the same (parent, name) get
        // either the UNIQUE-violation path (hard `EEXIST`, propagated) or an
        // SSI abort on the loser (retried — it will then see the row and
        // hit `EEXIST` on the second attempt).
        self.with_serial_retry(|tx| {
            // Is this the view's FIRST name for the key (count before INSERT)?
            // Only then does the shared cross-view ledger take a new hold.
            let first_name = match data_ref {
                Some(dr) => view_name_count(tx, &node, dr)? == 0,
                None => false,
            };
            let id: i64 = tx
                .query(
                    &format!(
                        "INSERT INTO {node} (parent_id,name,is_dir,data_ref,link_target,mode,uid,gid,mtime,atime,ctime)
                         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$9,$9) RETURNING id"
                    ),
                    &[
                        &parent,
                        &name,
                        &(is_dir as i64),
                        &data_ref,
                        &link_target,
                        &(mode as i64),
                        &(uid as i64),
                        &(gid as i64),
                        &t,
                    ],
                )?
                .into_iter()
                .next()
                .map(|r| r.get(0))
                .ok_or_else(|| WriteErr::Hard(MetaError::Db("no RETURNING row".to_string())))?;
            // Brand-new name owns a fresh inode: ino == id.
            tx.execute(
                &format!("UPDATE {node} SET ino=$1 WHERE id=$1"),
                &[&id],
            )?;
            if first_name {
                if let Some(dr) = data_ref {
                    ledger_add_tx(tx, dr)?;
                }
            }
            Ok(id)
        })
    }

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
        let _t = stats::start(stats::Stage::MetaCreate);
        self.insert(parent, name, is_dir, data_ref, None, mode, uid, gid)
    }

    pub fn create_symlink(
        &self,
        parent: i64,
        name: &str,
        target: &str,
        uid: u32,
        gid: u32,
    ) -> Result<i64, MetaError> {
        let _t = stats::start(stats::Stage::MetaCreate);
        self.insert(parent, name, false, None, Some(target), 0o0777, uid, gid)
    }

    pub fn link_node(
        &self,
        src_ino: i64,
        new_parent: i64,
        new_name: &str,
    ) -> Result<i64, MetaError> {
        let _t = stats::start(stats::Stage::MetaCreate);
        self.drop_cursors();
        let node = self.t("node");
        let t = now();
        // Read-of-source + INSERT of new name now share one SERIALIZABLE tx
        // (the pre-SSI impl ran them as two autocommit statements on the
        // mutex-guarded writer, which was safe only because of the mutex).
        // Under SSI a concurrent `unlink` of the last other name for the same
        // `src_ino` that commits between our SELECT and our INSERT would be
        // seen as a rw-dependency and abort one side — the retry then observes
        // the (now-different) state and correctly reports `ENOENT` or succeeds.
        self.with_serial_retry(|tx| {
            let row = tx
                .query(
                    &format!("SELECT is_dir, data_ref, mode, uid, gid FROM {node} WHERE ino=$1 LIMIT 1"),
                    &[&src_ino],
                )?
                .into_iter()
                .next()
                .ok_or(WriteErr::Hard(MetaError::NotFound))?;
            let is_dir: i64 = row.get::<_, i64>(0);
            let data_ref: Option<String> = row.get(1);
            let mode: i64 = row.get(2);
            let uid: i64 = row.get(3);
            let gid: i64 = row.get(4);
            if is_dir == 1 || data_ref.is_none() {
                return Err(WriteErr::Hard(MetaError::OpNotPermitted));
            }
            // First name for the key in this view (count before the INSERT)?
            let first_name = view_name_count(tx, &node, data_ref.as_deref().unwrap())? == 0;
            tx.execute(
                &format!(
                    "INSERT INTO {node} (parent_id,name,is_dir,data_ref,link_target,mode,uid,gid,mtime,atime,ctime,ino)
                     VALUES ($1,$2,0,$3,NULL,$4,$5,$6,$7,$7,$7,$8)"
                ),
                &[
                    &new_parent,
                    &new_name,
                    &data_ref,
                    &mode,
                    &uid,
                    &gid,
                    &t,
                    &src_ino,
                ],
            )?;
            if first_name {
                ledger_add_tx(tx, data_ref.as_deref().unwrap())?;
            }
            Ok(src_ino)
        })
    }

    pub fn rename(
        &self,
        parent: i64,
        name: &str,
        new_parent: i64,
        new_name: &str,
        noreplace: bool,
    ) -> Result<Option<String>, MetaError> {
        let _t = stats::start(stats::Stage::MetaRename);
        self.drop_cursors();
        let node = self.t("node");
        let t = now();
        // Full rename stays one transaction end-to-end: SELECT src -> cycle
        // check -> SELECT tgt -> same-inode no-op -> optional overwrite
        // DELETE -> UPDATE parent/name. A concurrent rename whose source or
        // target rows intersect ours will get an SSI abort on one side and
        // retry, at which point the second attempt sees the committed shape
        // and returns the correct POSIX outcome (ENOENT / ENOTEMPTY /
        // EEXIST / success). I1 (single identity) and I2 (visible atomicity)
        // follow from the schedule being serialisable — no per-op global
        // writer needed.
        self.with_serial_retry(|tx| {
            let (src_id, src_ino): (i64, Option<i64>) = tx
                .query(
                    &format!("SELECT id, ino FROM {node} WHERE parent_id=$1 AND name=$2"),
                    &[&parent, &name],
                )?
                .into_iter()
                .next()
                .map(|r| (r.get(0), r.get(1)))
                .ok_or(WriteErr::Hard(MetaError::NotFound))?;
            if is_descendant_tx(tx, &node, new_parent, src_id)? {
                return Err(WriteErr::Hard(MetaError::BadParentCycle));
            }
            let tgt: Option<(i64, i64, Option<String>, Option<i64>)> = tx
                .query(
                    &format!("SELECT id, is_dir, data_ref, ino FROM {node} WHERE parent_id=$1 AND name=$2"),
                    &[&new_parent, &new_name],
                )?
                .into_iter()
                .next()
                .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3)));
            // Same-inode renames (rename(a,a) AND a rename between two hard
            // link names of one file) are POSIX no-ops: without the ino
            // comparison the DELETE below would drop the target link and
            // shrink nlink. Mirrors the SQLite engine's guard in
            // [`crate::meta::MetaEngine::rename`].
            if let Some((tid, _, _, tino)) = &tgt {
                if *tid == src_id || (tino.is_some() && *tino == src_ino) {
                    return Ok(None);
                }
            }
            if tgt.is_some() && noreplace {
                return Err(WriteErr::Hard(MetaError::Exists));
            }
            let mut gc: Option<String> = None;
            if let Some((tid, is_dir, tgt_ref, _)) = tgt {
                let src_is_dir: i64 = tx
                    .query(
                        &format!("SELECT is_dir FROM {node} WHERE id=$1"),
                        &[&src_id],
                    )?
                    .into_iter()
                    .next()
                    .map(|r| r.get(0))
                    .unwrap_or(0);
                if src_is_dir != is_dir {
                    return Err(WriteErr::Hard(MetaError::TypeMismatch));
                }
                if is_dir == 1 {
                    let kids: i64 = tx
                        .query(
                            &format!("SELECT COUNT(*) FROM {node} WHERE parent_id=$1"),
                            &[&tid],
                        )?
                        .first()
                        .map(|r| r.get::<_, i64>(0))
                        .unwrap_or(0);
                    if kids > 0 {
                        return Err(WriteErr::Hard(MetaError::NotEmpty));
                    }
                }
                tx.execute(
                    &format!("DELETE FROM {node} WHERE id=$1"),
                    &[&tid],
                )?;
                if let Some(dr) = tgt_ref {
                    // Blob is freeable only once this view holds no other name
                    // AND the cross-view ledger confirms no other view does.
                    if view_name_count(tx, &node, &dr)? == 0 {
                        gc = ledger_remove_tx(tx, &dr)?;
                    }
                }
            }
            tx.execute(
                &format!("UPDATE {node} SET parent_id=$1, name=$2, ctime=$3 WHERE id=$4"),
                &[&new_parent, &new_name, &t, &src_id],
            )?;
            Ok(gc)
        })
    }

    pub fn unlink(&self, parent: i64, name: &str) -> Result<Option<String>, MetaError> {
        let _t = stats::start(stats::Stage::MetaUnlink);
        self.drop_cursors();
        let node = self.t("node");
        // Find -> emptiness check (dirs) -> DELETE -> refcount read is one
        // tx, so two hardlink names being removed concurrently can never
        // interleave and either GC a still-referenced object or miss GC of
        // the last name. The final `refs == 0` read is inside the same
        // snapshot as the DELETE, so SSI sees the full write set as one
        // atomic transition.
        self.with_serial_retry(|tx| {
            let row = tx
                .query(
                    &format!(
                        "SELECT id, is_dir, data_ref FROM {node} WHERE parent_id=$1 AND name=$2"
                    ),
                    &[&parent, &name],
                )?
                .into_iter()
                .next()
                .ok_or(WriteErr::Hard(MetaError::NotFound))?;
            let id: i64 = row.get(0);
            let is_dir: i64 = row.get(1);
            let data_ref: Option<String> = row.get(2);
            if is_dir == 1 {
                let kids: i64 = tx
                    .query(
                        &format!("SELECT COUNT(*) FROM {node} WHERE parent_id=$1"),
                        &[&id],
                    )?
                    .first()
                    .map(|r| r.get::<_, i64>(0))
                    .unwrap_or(0);
                if kids > 0 {
                    return Err(WriteErr::Hard(MetaError::NotEmpty));
                }
            }
            tx.execute(&format!("DELETE FROM {node} WHERE id=$1"), &[&id])?;
            let gc = match &data_ref {
                Some(dr) => {
                    // Last name in this view AND no other view (ledger) still
                    // references it -> freeable; otherwise kept.
                    if view_name_count(tx, &node, dr)? == 0 {
                        ledger_remove_tx(tx, dr)?
                    } else {
                        None
                    }
                }
                None => None,
            };
            Ok(gc)
        })
    }

    pub fn touch_mtime(&self, ino: i64) {
        let t = now();
        let node = self.t("node");
        // Single-statement UPDATE. SSI can still abort one side of a
        // concurrent touch on the same row while a reader holds a
        // conflicting predicate window, so run it through the retry helper
        // and drop the mapped error (mirrors the pre-SSI `let _ = ...`
        // tolerance: a stale mtime is a soft failure, not a POSIX error).
        let _ = self.with_serial_retry(|tx| {
            tx.execute(
                &format!("UPDATE {node} SET mtime=$1, ctime=$1 WHERE ino=$2"),
                &[&t, &ino],
            )?;
            Ok(())
        });
    }

    pub fn set_times(&self, ino: i64, atime: Option<i64>, mtime: Option<i64>) {
        let t = now();
        let node = self.t("node");
        let _ = self.with_serial_retry(|tx| {
            if let Some(a) = atime {
                tx.execute(
                    &format!("UPDATE {node} SET atime=$1, ctime=$2 WHERE ino=$3"),
                    &[&a, &t, &ino],
                )?;
            }
            if let Some(m) = mtime {
                tx.execute(
                    &format!("UPDATE {node} SET mtime=$1, ctime=$2 WHERE ino=$3"),
                    &[&m, &t, &ino],
                )?;
            }
            Ok(())
        });
    }

    pub fn set_mode(&self, ino: i64, mode: u32) {
        let t = now();
        let node = self.t("node");
        let _ = self.with_serial_retry(|tx| {
            tx.execute(
                &format!("UPDATE {node} SET mode=$1, ctime=$2 WHERE ino=$3"),
                &[&(mode as i64), &t, &ino],
            )?;
            Ok(())
        });
    }

    pub fn set_uid_gid(&self, ino: i64, uid: u32, gid: u32) {
        let t = now();
        let node = self.t("node");
        let _ = self.with_serial_retry(|tx| {
            tx.execute(
                &format!("UPDATE {node} SET uid=$1, gid=$2, ctime=$3 WHERE ino=$4"),
                &[&(uid as i64), &(gid as i64), &t, &ino],
            )?;
            Ok(())
        });
    }

    // ---- HA epoch (present for API parity; GroveFS HA runs SQLite-only) ----

    pub fn get_epoch(&self) -> i64 {
        let ctrl = self.t("ctrl");
        // Read path: fan out on the read pool (READ COMMITTED is fine here —
        // the value is only consulted by the SQLite-only HA code and never
        // gates a governance decision on the PG path).
        self.with_read(|c| {
            c.query(&format!("SELECT v FROM {ctrl} WHERE k='epoch'"), &[])
                .ok()
                .and_then(|rows| rows.first().map(|r| r.get::<_, i64>(0)))
                .unwrap_or(0)
        })
    }

    pub fn set_epoch(&self, e: i64) -> std::io::Result<()> {
        let ctrl = self.t("ctrl");
        // Admin path (only reachable from SQLite-HA code that PG rejects at
        // startup, so it is effectively dead here): checkout/requeue directly,
        // no need to build an SSI tx for a one-shot UPSERT.
        let mut c = self
            .checkout_writer()
            .map_err(|me| std::io::Error::other(me.to_string()))?;
        let r = c.execute(
            &format!(
                "INSERT INTO {ctrl} (k,v) VALUES ('epoch',$1) ON CONFLICT(k) DO UPDATE SET v=excluded.v"
            ),
            &[&e],
        );
        self.requeue_writer(c);
        r.map(|_| ()).map_err(io_e)
    }

    /// GroveFS-native HA snapshot is not offered on PG — Postgres replicates
    /// via its own mechanisms (streaming replication / PITR), and a `VACUUM
    /// INTO`-equivalent (a whole-database dump per view) is not worth
    /// duplicating here. `meta-server --store postgres://...` therefore rejects
    /// `--role/--peer/--witness` at CLI parse time and never reaches this
    /// method; the impl is only present to satisfy the MetaStore enum's
    /// uniform interface.
    pub fn backup_to_bytes(&self) -> std::io::Result<Vec<u8>> {
        Err(std::io::Error::other(
            "GroveFS-native HA snapshot is SQLite-only; use PostgreSQL's own replication for a PG-backed meta-server",
        ))
    }

    // ---- POSIX advisory locks ---------------------------------------------

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
        let locks = self.t("locks");
        let now = now();
        // Lease sweep + conflict SELECT + own-range DELETE + INSERT run in
        // one SSI tx. Two clients racing for the same (ino, range) get one
        // side aborted and re-run: the retry then sees the winner's INSERT
        // and correctly returns the LockConflict (or succeeds if the winner
        // was for a disjoint range). Locks across DIFFERENT inodes never
        // conflict because their rows are disjoint.
        self.with_serial_retry(|tx| {
            tx.execute(
                &format!("DELETE FROM {locks} WHERE expires_at < $1"),
                &[&now],
            )?;
            let s = start.min(i64::MAX as u64) as i64;
            let e = end.min(i64::MAX as u64) as i64;
            let owner_i = owner as i64;
            match ty {
                LockType::Unlock => {
                    tx.execute(
                        &format!("DELETE FROM {locks} WHERE ino=$1 AND owner=$2 AND l_start<=$3 AND l_end>=$4"),
                        &[&ino, &owner_i, &e, &s],
                    )?;
                }
                LockType::Read | LockType::Write => {
                    let want_write = ty.to_u8() as i64 == 1;
                    let conflict_sql = format!(
                        "SELECT l_start, l_end, ltype, pid FROM {locks}
                         WHERE ino=$1 AND owner<>$2 AND l_start<=$3 AND l_end>=$4
                           AND ($5::bigint = 1 OR ltype = 1) LIMIT 1"
                    );
                    let conflict: Option<(i64, i64, i64, i64)> = tx
                        .query(
                            &conflict_sql,
                            &[&ino, &owner_i, &e, &s, &(want_write as i64)],
                        )?
                        .into_iter()
                        .next()
                        .map(|r| (r.get(0), r.get(1), r.get::<_, i64>(2), r.get::<_, i64>(3)));
                    if let Some((cs, ce, ct, cp)) = conflict {
                        return Ok(Some(LockConflict {
                            start: cs.max(0) as u64,
                            end: ce.max(0) as u64,
                            ty: LockType::from_u8(ct as u8).unwrap_or(LockType::Read),
                            pid: cp as u32,
                        }));
                    }
                    tx.execute(
                        &format!("DELETE FROM {locks} WHERE ino=$1 AND owner=$2 AND l_start<=$3 AND l_end>=$4"),
                        &[&ino, &owner_i, &e, &s],
                    )?;
                    tx.execute(
                        &format!(
                            "INSERT INTO {locks} (ino,owner,sid,l_start,l_end,pid,ltype,expires_at)
                             VALUES ($1,$2,$3,$4,$5,$6,$7,$8)"
                        ),
                        &[
                            &ino,
                            &owner_i,
                            &(sid as i64),
                            &s,
                            &e,
                            &(pid as i64),
                            &(ty.to_u8() as i64),
                            &(now + lease_secs()),
                        ],
                    )?;
                }
            }
            Ok(None)
        })
    }

    pub fn get_lock(
        &self,
        ino: i64,
        owner: u64,
        start: u64,
        end: u64,
        ty: LockType,
    ) -> Option<LockConflict> {
        let locks = self.t("locks");
        let now = now();
        let s = start.min(i64::MAX as u64) as i64;
        let e = end.min(i64::MAX as u64) as i64;
        let want_write = ty.to_u8() as i64 == 1;
        // Pure read: fan out on the read pool at READ COMMITTED.
        self.with_read(|c| {
            c.query(
                &format!(
                    "SELECT l_start, l_end, ltype, pid FROM {locks}
                     WHERE ino=$1 AND owner<>$2 AND expires_at>=$3 AND l_start<=$4 AND l_end>=$5
                       AND ($6::bigint = 1 OR ltype = 1) LIMIT 1"
                ),
                &[&ino, &(owner as i64), &now, &e, &s, &want_write],
            )
            .ok()
            .and_then(|rows| {
                rows.into_iter().next().map(|r| {
                    let cs: i64 = r.get(0);
                    let ce: i64 = r.get(1);
                    let ct: i64 = r.get(2);
                    let cp: i64 = r.get(3);
                    LockConflict {
                        start: cs.max(0) as u64,
                        end: ce.max(0) as u64,
                        ty: LockType::from_u8(ct as u8).unwrap_or(LockType::Read),
                        pid: cp as u32,
                    }
                })
            })
        })
    }

    pub fn release_locks_of_owner(&self, owner: u64) {
        let locks = self.t("locks");
        let _ = self.with_serial_retry(|tx| {
            tx.execute(
                &format!("DELETE FROM {locks} WHERE owner=$1"),
                &[&(owner as i64)],
            )?;
            Ok(())
        });
    }

    pub fn renew_locks(&self, sid: u64) {
        let locks = self.t("locks");
        let now = now();
        let _ = self.with_serial_retry(|tx| {
            tx.execute(
                &format!("UPDATE {locks} SET expires_at=$1 WHERE sid=$2"),
                &[&(now + lease_secs()), &(sid as i64)],
            )?;
            Ok(())
        });
    }

    pub fn release_locks_of_session(&self, sid: u64) {
        let locks = self.t("locks");
        let _ = self.with_serial_retry(|tx| {
            tx.execute(
                &format!("DELETE FROM {locks} WHERE sid=$1"),
                &[&(sid as i64)],
            )?;
            Ok(())
        });
    }

    pub fn sweep_expired_locks(&self, now_secs: i64) {
        let locks = self.t("locks");
        let _ = self.with_serial_retry(|tx| {
            tx.execute(
                &format!("DELETE FROM {locks} WHERE expires_at < $1"),
                &[&now_secs],
            )?;
            Ok(())
        });
    }
}

/// Walk ancestors of `new_parent` looking for `src_id`; if we reach it, moving
/// would create a cycle. Runs inside the rename transaction.
///
/// Errors are typed as [`WriteErr`] so the caller's `with_serial_retry` body
/// can distinguish an SSI abort on the ancestor walk (retry the whole rename)
/// from a genuine `BadParentCycle` / db failure (propagate to the client).
fn is_descendant_tx(
    tx: &mut postgres::Transaction<'_>,
    node: &str,
    new_parent: i64,
    src_id: i64,
) -> Result<bool, WriteErr> {
    let mut cur = new_parent;
    for _ in 0..4096 {
        if cur == src_id {
            return Ok(true);
        }
        let parent: Option<i64> = tx
            .query(
                &format!("SELECT parent_id FROM {node} WHERE id=$1"),
                &[&cur],
            )?
            .into_iter()
            .next()
            .and_then(|r| r.get(0));
        match parent {
            Some(p) => cur = p,
            None => return Ok(false),
        }
    }
    Ok(false)
}

/// How many names THIS view still holds for `dr` (inside the caller's tx,
/// before/after the node write it brackets). Used to detect the 0->1 and 1->0
/// transitions that decide when the shared ledger is booked/released.
fn view_name_count(
    tx: &mut postgres::Transaction<'_>,
    node: &str,
    dr: &str,
) -> Result<i64, WriteErr> {
    Ok(tx
        .query(
            &format!("SELECT COUNT(*) FROM {node} WHERE data_ref=$1"),
            &[&dr],
        )?
        .first()
        .map(|r| r.get::<_, i64>(0))
        .unwrap_or(0))
}

/// Book this view as a holder of `dr` (call when its name count goes 0->1, in
/// the same tx as the node INSERT). `ON CONFLICT DO UPDATE` keeps the row
/// atomic under concurrent holders from other views; an SSI clash retries the
/// whole body.
fn ledger_add_tx(tx: &mut postgres::Transaction<'_>, dr: &str) -> Result<(), WriteErr> {
    tx.execute(
        &format!(
            "INSERT INTO {PG_REFS_TABLE} (data_ref, holders) VALUES ($1, 1)
             ON CONFLICT (data_ref) DO UPDATE SET holders = {PG_REFS_TABLE}.holders + 1"
        ),
        &[&dr],
    )?;
    Ok(())
}

/// Drop this view's hold on `dr` (call when its LAST name in the view is
/// removed, same tx as the node DELETE). Returns `Some(dr)` when no view holds
/// it anymore (caller GCs) and `None` when another still does. A missing row is
/// "no other holder", matching the pre-ledger single-view behaviour.
fn ledger_remove_tx(
    tx: &mut postgres::Transaction<'_>,
    dr: &str,
) -> Result<Option<String>, WriteErr> {
    let left: Option<i64> = tx
        .query(
            &format!("UPDATE {PG_REFS_TABLE} SET holders = holders - 1 WHERE data_ref=$1 RETURNING holders"),
            &[&dr],
        )?
        .into_iter()
        .next()
        .map(|r| r.get::<_, i64>(0));
    match left {
        Some(h) if h <= 0 => {
            tx.execute(
                &format!("DELETE FROM {PG_REFS_TABLE} WHERE data_ref=$1"),
                &[&dr],
            )?;
            Ok(Some(dr.to_string()))
        }
        Some(_) => Ok(None),
        // No ledger row: never booked against a shared key -> free it as the
        // pre-ledger engine did for every private object.
        None => Ok(Some(dr.to_string())),
    }
}

/// Internal marker returned by [`PgEngine::with_serial_retry`] bodies so the
/// driver can decide between "retry the whole body with backoff" (SSI abort)
/// and "propagate to the caller" (POSIX-visible failures such as `EEXIST`,
/// `ENOTEMPTY`, `EXDEV`, db outage). Not exposed outside this module.
enum WriteErr {
    /// SQLSTATE 40001 `serialization_failure` or 40P01 `deadlock_detected`.
    Serialization,
    /// Any other error, already mapped to the caller-facing [`MetaError`].
    Hard(MetaError),
}

impl From<postgres::Error> for WriteErr {
    fn from(e: postgres::Error) -> Self {
        if PgEngine::is_serialization_failure(&e) {
            WriteErr::Serialization
        } else {
            WriteErr::Hard(map_db_err(&e))
        }
    }
}

impl From<MetaError> for WriteErr {
    fn from(me: MetaError) -> Self {
        WriteErr::Hard(me)
    }
}

fn io_e(e: postgres::Error) -> std::io::Error {
    // `postgres::Error` Display shows only the ErrorKind (e.g. "db error"),
    // which is useless on its own. Surface the server's SQLSTATE / message /
    // detail / cause chain so a failed open or query is diagnosable from the
    // `meta-server` log line alone.
    let mut s = format!("{}", e);
    if let Some(db) = e.as_db_error() {
        s = format!("{s}: [{}] {}", db.code().code(), db.message());
        if let Some(d) = db.detail() {
            s = format!("{s}  detail={d}");
        }
        if let Some(h) = db.hint() {
            s = format!("{s}  hint={h}");
        }
    }
    if let Some(src) = std::error::Error::source(&e) {
        s = format!("{s}  cause={src}");
    }
    std::io::Error::other(s)
}

/// Map a `postgres::Error` to a MetaError. Unique-violation (23505) -> Exists,
/// everything else -> Db.
fn map_db_err(e: &postgres::Error) -> MetaError {
    if e.code() == Some(&postgres::error::SqlState::UNIQUE_VIOLATION) {
        MetaError::Exists
    } else {
        let mut s = format!("{}", e);
        if let Some(db) = e.as_db_error() {
            s = format!("{s}: [{}] {}", db.code().code(), db.message());
        }
        MetaError::Db(s)
    }
}

// The MetaStore enum in meta.rs drives PgEngine through its *inherent* methods
// (`readdir_page`/`readdir`/...), not this trait impl. This impl exists so a
// caller holding an `Arc<PgEngine>` directly (unit tests) can also use it as a
// MetaProvider, and so `supports_paged_readdir` reports PG's store-level paging
// capability to anything that asks through the trait.
impl MetaProvider for PgEngine {
    fn lookup(&self, parent: i64, name: &str) -> Option<NodeInfo> {
        PgEngine::lookup(self, parent, name)
    }
    fn getattr(&self, ino: i64) -> Option<NodeInfo> {
        PgEngine::getattr(self, ino)
    }
    fn readdir(&self, parent: i64) -> Vec<NodeInfo> {
        PgEngine::readdir(self, parent)
    }
    fn readdir_page(&self, parent: i64, offset: usize, limit: usize) -> Vec<NodeInfo> {
        PgEngine::readdir_page(self, parent, offset, limit)
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
        PgEngine::create_node(self, parent, name, is_dir, data_ref, mode, uid, gid)
    }
    fn create_symlink(
        &self,
        parent: i64,
        name: &str,
        target: &str,
        uid: u32,
        gid: u32,
    ) -> Result<i64, MetaError> {
        PgEngine::create_symlink(self, parent, name, target, uid, gid)
    }
    fn link_node(&self, src_ino: i64, new_parent: i64, new_name: &str) -> Result<i64, MetaError> {
        PgEngine::link_node(self, src_ino, new_parent, new_name)
    }
    fn unlink(&self, parent: i64, name: &str) -> Result<Option<String>, MetaError> {
        PgEngine::unlink(self, parent, name)
    }
    fn rename(
        &self,
        parent: i64,
        name: &str,
        new_parent: i64,
        new_name: &str,
        noreplace: bool,
    ) -> Result<Option<String>, MetaError> {
        PgEngine::rename(self, parent, name, new_parent, new_name, noreplace)
    }
    fn set_mode(&self, ino: i64, mode: u32) {
        PgEngine::set_mode(self, ino, mode)
    }
    fn set_uid_gid(&self, ino: i64, uid: u32, gid: u32) {
        PgEngine::set_uid_gid(self, ino, uid, gid)
    }
    fn set_times(&self, ino: i64, atime: Option<i64>, mtime: Option<i64>) {
        PgEngine::set_times(self, ino, atime, mtime)
    }
    fn touch_mtime(&self, ino: i64) {
        PgEngine::touch_mtime(self, ino)
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
        PgEngine::lock_op(self, ino, owner, start, end, ty, pid, sid)
    }
    fn get_lock(
        &self,
        ino: i64,
        owner: u64,
        start: u64,
        end: u64,
        ty: LockType,
    ) -> Option<LockConflict> {
        PgEngine::get_lock(self, ino, owner, start, end, ty)
    }
    fn release_locks_of_owner(&self, owner: u64) {
        PgEngine::release_locks_of_owner(self, owner)
    }
    fn renew_locks(&self, sid: u64) {
        PgEngine::renew_locks(self, sid)
    }
    fn dump_tree(&self) -> Vec<(i64, i64, String, bool, Option<String>)> {
        let node = self.t("node");
        let sql = format!("SELECT id, parent_id, name, is_dir, data_ref FROM {node} ORDER BY id");
        self.with_read(|c| {
            c.query(&sql, &[])
                .map(|rows| {
                    rows.iter()
                        .map(|r| {
                            (
                                r.get::<_, i64>(0),
                                r.get::<_, Option<i64>>(1).unwrap_or(0),
                                r.get::<_, String>(2),
                                r.get::<_, i64>(3) != 0,
                                r.get::<_, Option<String>>(4),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default()
        })
    }
}

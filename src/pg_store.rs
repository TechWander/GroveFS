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
//! reports unsupported and a `meta-server --meta-store postgres://...` refuses the HA
//! flags. Everything else is a drop-in.

use crate::meta::{LockConflict, LockType, MetaError, MetaProvider, NodeInfo, NodeKind};
use crate::stats;
use postgres::types::ToSql;
use postgres::{Client, IsolationLevel, NoTls, Row, Statement};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::ops::{Deref, DerefMut};
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

/// Route a store specification that may name several PostgreSQL instances
/// (`uri,uri,...`) to the one that owns `view`.
///
/// This is the seed of view sharding: each view's `grovefs_v_<hex>` schema
/// lives on exactly one instance, chosen by a stable hash of the view name, so
/// `view create`, every `meta-server` serving it and cross-view work
/// (graft/export/rm by name) all agree without a registry. A single URI is
/// returned verbatim, so nothing changes for a one-instance deployment.
///
/// Boundaries (deliberate, see USAGE): the control plane (`grovefs_control`:
/// keys, view definitions, audit) stays on the FIRST instance —
/// [`control_store`]; `du`/`gc`/`refs rebuild` still expect one URI. Changing
/// the instance list re-maps views (the mapping is a pure function of the
/// names), so re-sharding is a data move, not a flag flip.
pub fn route_store(store: &str, view: &str) -> String {
    let uris = store_instances(store);
    if uris.len() <= 1 {
        return uris.first().copied().unwrap_or(store.trim()).to_string();
    }
    // FNV-1a over the raw view name: stable across processes and rebuilds, and
    // independent of how many views exist.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in view.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    uris[(h % uris.len() as u64) as usize].to_string()
}

/// The distinct PostgreSQL instances a store spec names, in the order given.
/// A single URI yields a one-element list, so callers need no special case.
pub fn store_instances(store: &str) -> Vec<&str> {
    store
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}

/// Resolve a store spec for an operation that reads/writes SEVERAL trees inside
/// one transaction (fork: base+target, graft: source+destination). Such a pair
/// must live on ONE instance — a `postgres://` connection cannot span two
/// databases — so this returns the routed instance when every name agrees and
/// refuses with a readable error when they do not.
pub fn route_multi(store: &str, views: &[&str]) -> Result<String, String> {
    let mut picked: Option<String> = None;
    let mut split: Vec<String> = Vec::new();
    for v in views {
        let uri = route_store(store, v);
        match &picked {
            None => picked = Some(uri),
            Some(p) if p == &uri => {}
            Some(p) => split.push(format!("'{}' -> {}, '{}' -> {}", views[0], p, v, uri)),
        }
    }
    match (picked, split.is_empty()) {
        (Some(uri), true) => Ok(uri),
        _ => Err(format!(
            "these trees route to different PostgreSQL instances ({}); a fork/graft copies between schemas of ONE database, so both views must hash to the same instance — rename one, or use a single-instance store for this pair",
            split.join("; ")
        )),
    }
}

/// The instance that holds the shared control schema of a (possibly sharded)
/// store spec. Pinned to the first URI: the control plane is ONE authoritative
/// record shared by every instance (exactly as in a single-instance setup).
pub fn control_store(store: &str) -> &str {
    store.split(',').next().unwrap_or(store).trim()
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

/// One node row of a PG tree being read for a graft. Same shape as the file
/// arm's private `GraftRow` (see `meta::engine`), because a graft copies NAMES
/// between trees and must give the destination the same metadata a SQLite graft
/// would.
struct GraftPgRow {
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

/// Mutable state of one graft transaction: the id/ino allocator, the merge
/// rule, the running totals, the blobs to book and the source-ino ->
/// destination-ino map that keeps hard links hard links.
struct GraftCtx {
    next: i64,
    skip_existing: bool,
    stats: crate::meta::GraftStats,
    blobs: std::collections::HashSet<String>,
    inos: HashMap<i64, i64>,
}

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

/// Upper bound on the statements one read connection keeps prepared. A view's
/// read surface is a handful of texts (`lookup` by `(parent,name)`, `getattr` by
/// `ino`, the two READDIR pages, the lock probe, the epoch read); the spare room
/// absorbs the `LIMIT/OFFSET` variant a seekdir miss builds. Bounded on purpose:
/// a pathological caller must not turn server-side statement memory into a leak.
const STMT_CACHE_CAP: usize = 16;

/// Upper bound on the statements one writer connection keeps prepared. A view's
/// write surface is larger than its read surface (insert/lookup-by-name/delete/
/// set-ino/the four setattr variants/rename/lock ops/three ledger texts), so the
/// cap is double; overflowing it only means the extra text keeps being parsed
/// per op, never an error.
const WRITE_STMT_CACHE_CAP: usize = 32;

/// A pooled read connection plus the statements it has already parsed.
///
/// [`Client::query`] sends an *unnamed* statement on every call, so PG re-parses
/// and re-plans the same SQL text on every `lookup`/`getattr`/`readdir`. Those
/// texts are stable for the life of a view (only the schema-qualified table name
/// varies), and a prepared statement is session-scoped, which is why the cache
/// lives on the connection rather than on the engine: each pooled connection
/// pays the parse once for its own session and then reuses it.
struct ReadConn {
    client: Client,
    stmts: HashMap<String, Statement>,
}

impl ReadConn {
    fn new(client: Client) -> Self {
        Self {
            client,
            stmts: HashMap::new(),
        }
    }

    /// Same contract as [`Client::query`], but served from a cached prepared
    /// statement. A failed `prepare` propagates whatever the parse error was —
    /// callers already treat a query error as "empty result, degrade quietly".
    fn query(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Vec<Row>, postgres::Error> {
        if let Some(st) = self.stmts.get(sql) {
            let st = st.clone();
            return self.client.query(&st, params);
        }
        let st = self.client.prepare(sql)?;
        if self.stmts.len() < STMT_CACHE_CAP {
            self.stmts.insert(sql.to_string(), st.clone());
        }
        self.client.query(&st, params)
    }
}

/// A pooled writer connection plus the statements it has already parsed.
///
/// [`postgres::Transaction`]'s `query(&str)`/`execute(&str)` send an *unnamed*
/// statement, so the server re-parses and re-plans the same write text on every
/// op. Those parses are not free under SERIALIZABLE: a single-writer insert
/// loop measured 577us/op unnamed vs 316us/op prepared on this box (-45%), and
/// 8 concurrent writers went 8.3k -> 9.6k ops/s (+15%). The texts are stable for
/// the life of a view and a prepared statement is session-scoped, so each
/// pooled connection carries its own cache, exactly like [`ReadConn`] does for
/// the read surface.
struct WriteConn {
    client: Client,
    stmts: HashMap<String, Statement>,
}

impl WriteConn {
    fn new(client: Client) -> Self {
        Self {
            client,
            stmts: HashMap::new(),
        }
    }
}

impl Deref for WriteConn {
    type Target = Client;
    fn deref(&self) -> &Client {
        &self.client
    }
}

impl DerefMut for WriteConn {
    fn deref_mut(&mut self) -> &mut Client {
        &mut self.client
    }
}

/// A write transaction plus the connection's statement cache.
///
/// The inherent `query`/`execute` shadow the ones a `Transaction` provides via
/// [`Deref`], so a body written as `tx.execute("...", &[..])` transparently
/// goes through the cached statement — no call site has to opt in. A cache miss
/// prepares on first use (a prepared statement survives both COMMIT and
/// ROLLBACK, verified against the server) and keeps it for the session.
struct WriteTx<'a, 'c> {
    tx: &'a mut postgres::Transaction<'c>,
    stmts: &'a mut HashMap<String, Statement>,
}

impl WriteTx<'_, '_> {
    fn query(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Vec<Row>, postgres::Error> {
        let st = self.stmt(sql)?;
        match self.tx.query(&st, params) {
            // DDL between prepare and execute (a view tree rebuilt by
            // import/graft) or a server-side DEALLOCATE invalidates the cached
            // plan; evict and let the server parse the text once more rather
            // than turning a legitimate op into a hard error.
            Err(e) if Self::stale_statement(&e) => {
                self.stmts.remove(sql);
                self.tx.query(sql, params)
            }
            other => other,
        }
    }

    fn execute(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<u64, postgres::Error> {
        let st = self.stmt(sql)?;
        match self.tx.execute(&st, params) {
            Err(e) if Self::stale_statement(&e) => {
                self.stmts.remove(sql);
                self.tx.execute(sql, params)
            }
            other => other,
        }
    }

    fn stmt(&mut self, sql: &str) -> Result<Statement, postgres::Error> {
        if let Some(st) = self.stmts.get(sql) {
            return Ok(st.clone());
        }
        let st = self.tx.prepare(sql)?;
        if self.stmts.len() < WRITE_STMT_CACHE_CAP {
            self.stmts.insert(sql.to_string(), st.clone());
        }
        Ok(st)
    }

    /// `0A000` = cached plan must not change result type, `26000` = prepared
    /// statement no longer exists. Both mean "the cached copy is stale", never
    /// "the op failed", so they are the only errors worth a re-parse.
    fn stale_statement(e: &postgres::Error) -> bool {
        matches!(
            e.code(),
            Some(&postgres::error::SqlState::FEATURE_NOT_SUPPORTED)
                | Some(&postgres::error::SqlState::INVALID_SQL_STATEMENT_NAME)
        )
    }
}

// Inherent `query`/`execute` shadow the `Transaction` ones, but the ledger and
// count helpers further down take a transaction-like argument; deref lets them
// keep accepting a bare `Transaction` (the cold graft/import path) while the
// hot path hands them a [`WriteTx`] and keeps the cache.
impl<'a, 'c> Deref for WriteTx<'a, 'c> {
    type Target = postgres::Transaction<'c>;
    fn deref(&self) -> &Self::Target {
        self.tx
    }
}

impl DerefMut for WriteTx<'_, '_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.tx
    }
}

/// The statement-execution surface the ledger/count helpers are written
/// against: [`WriteTx`] serves it from the connection's statement cache, a bare
/// transaction lets the server parse each time (cold paths only).
trait TxSql {
    fn tq(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Vec<Row>, postgres::Error>;
    fn te(&mut self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<u64, postgres::Error>;
}

impl TxSql for WriteTx<'_, '_> {
    fn tq(&mut self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<Vec<Row>, postgres::Error> {
        self.query(sql, params)
    }
    fn te(&mut self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<u64, postgres::Error> {
        self.execute(sql, params)
    }
}

impl TxSql for postgres::Transaction<'_> {
    fn tq(&mut self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<Vec<Row>, postgres::Error> {
        self.query(sql, params)
    }
    fn te(&mut self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<u64, postgres::Error> {
        self.execute(sql, params)
    }
}

// The write-side helpers ([`PgEngine::finalize`] / [`PgEngine::finalize_batch`])
// take `&mut Client` and run on both paths; these impls let a read closure hand
// its `ReadConn` straight to them instead of duplicating the SQL.
impl Deref for ReadConn {
    type Target = Client;
    fn deref(&self) -> &Client {
        &self.client
    }
}

impl DerefMut for ReadConn {
    fn deref_mut(&mut self) -> &mut Client {
        &mut self.client
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
/// unchanged: a `meta-server` running without `--meta-store` still serialises all
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
    /// `default_transaction_isolation = 'serializable'` at the session level and
    /// its own prepared-statement cache. A [`Self::checkout_writer`] hands one
    /// to the calling thread; every mutation runs its per-op `Transaction` on
    /// that borrowed connection, so concurrent mutators on disjoint rows
    /// genuinely run in parallel PG backends. Bounded so a busy meta-server does
    /// not exhaust `max_connections`.
    write_pool: parking_lot::Mutex<Vec<WriteConn>>,
    /// Pooled read connections so concurrent metadata reads (lookup/getattr/
    /// readdir) fan out at READ COMMITTED (cheaper — no SSI predicate locks)
    /// instead of queueing behind an SSI writer. A fresh PostgreSQL backend
    /// started by `Client::connect` sees committed writes in its own read
    /// transaction, so read-your-writes holds.
    read_pool: parking_lot::Mutex<Vec<ReadConn>>,
    /// Keyset cursors for sequential READDIR paging, mirroring the SQLite
    /// engine: `(parent, checkpoint offset)` -> the name of the row before it.
    /// Cleared by any name mutation; a miss falls back to `LIMIT/OFFSET`.
    cursors: parking_lot::Mutex<HashMap<(i64, usize), String>>,
}

/// What a whole-base-view teardown found and did (see [`PgEngine::remove_view`]).
#[derive(Debug)]
pub struct PgPurgeReport {
    /// Node rows the tree held, the root included.
    pub entries: u64,
    /// Data refs whose holder count reached zero: no surviving view names those
    /// bytes, so the caller may free them through its data root.
    pub unbooked: usize,
    /// The same refs by name, so a caller that owns a data root can act on
    /// them instead of only reporting a number.
    pub unbooked_refs: Vec<String>,
    /// Data refs another view still names — decremented by one, kept.
    pub still_held: usize,
}

/// What a ledger recount found and corrected (see [`rebuild_ledger`]).
#[derive(Debug)]
pub struct PgLedgerReport {
    /// How many `grovefs_v_*` trees were actually read.
    pub trees: usize,
    /// Distinct content keys those trees name.
    pub refs: usize,
    /// Holder units the recount booked (one per tree per key).
    pub holders: i64,
    /// Ledger rows whose count was wrong and has been rewritten.
    pub rows_fixed: usize,
    /// Ledger rows no surviving tree names (booked for bytes nobody owns).
    pub rows_deleted: usize,
}

/// Ceiling on those cursors (one checkpoint per [`READDIR_CURSOR_STRIDE`] rows
/// per open directory).
const READDIR_CURSOR_CAP: usize = 65_536;
/// Checkpoint granularity: the kernel resumes wherever its reply buffer filled,
/// which rarely lands on our own page boundary (see the SQLite engine).
const READDIR_CURSOR_STRIDE: usize = 32;

/// Default number of pooled writer connections per PG view engine. Bounded by
/// `max_connections` on the server (default 100); a small per-view pool keeps
/// total backend pressure predictable when the meta-server hosts many views.
const PG_WRITE_POOL_SIZE: usize = 8;

/// Default number of pooled read connections opened per PG view engine, matching
/// the SQLite engine's [`crate::meta::READ_POOL_SIZE`].
const PG_READ_POOL_SIZE: usize = 4;

/// Runtime knobs for the two per-view pools: `GROVEFS_PG_WRITE_POOL` and
/// `GROVEFS_PG_READ_POOL` (`0..=64` each). The pool size is what the capacity
/// formula (active views x pool + burst <= `max_connections`) is planned
/// against, so an operator who tuned `max_connections` upward can widen the
/// pool without rebuilding; `0` means "no warm pool" (every checkout connects
/// fresh), which is legal but pays connect latency per op. Internal tuning
/// knobs (like `GROVEFS_SQLITE_READ_POOL`), cached on first read; the CLI
/// surface is unchanged.
fn pg_write_pool_size() -> usize {
    static SLOT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(usize::MAX);
    let cur = SLOT.load(std::sync::atomic::Ordering::Relaxed);
    if cur != usize::MAX {
        return cur;
    }
    let v = std::env::var("GROVEFS_PG_WRITE_POOL")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .map(|n| n.min(64))
        .unwrap_or(PG_WRITE_POOL_SIZE);
    SLOT.store(v, std::sync::atomic::Ordering::Relaxed);
    v
}

fn pg_read_pool_size() -> usize {
    static SLOT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(usize::MAX);
    let cur = SLOT.load(std::sync::atomic::Ordering::Relaxed);
    if cur != usize::MAX {
        return cur;
    }
    let v = std::env::var("GROVEFS_PG_READ_POOL")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .map(|n| n.min(64))
        .unwrap_or(PG_READ_POOL_SIZE);
    SLOT.store(v, std::sync::atomic::Ordering::Relaxed);
    v
}

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

/// Opt-in per-session durability for writer connections:
/// `GROVEFS_PG_SYNC_COMMIT=off` makes each COMMIT return once the WAL record is
/// written, without waiting for the local flush. The visible effect is that the
/// per-op commit latency stops carrying a disk flush, which is the knob an
/// operator reaches for when metadata commits are the wall and a crash losing
/// the last few milliseconds of namespace operations is acceptable.
///
/// Default `on` — PostgreSQL's own default. This is a *durability* choice, not a
/// consistency one: SSI still serialises conflicting writers and no GroveFS
/// invariant changes. It does reach further than the local disk, though: with
/// `synchronous_standby_names` configured, `off` also stops waiting for a
/// standby's acknowledgement, so an RPO=0 deployment must leave it on. GroveFS's
/// own `--sync-repl` snapshot forwarding is unaffected (it predates the commit
/// and is mutually exclusive with the PG backend anyway).
fn pg_sync_commit_off() -> bool {
    static SLOT: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(u8::MAX);
    let cur = SLOT.load(std::sync::atomic::Ordering::Relaxed);
    if cur != u8::MAX {
        return cur != 0;
    }
    let off = std::env::var("GROVEFS_PG_SYNC_COMMIT")
        .map(|v| {
            let v = v.trim().to_ascii_lowercase();
            v == "off" || v == "false" || v == "0" || v == "async"
        })
        .unwrap_or(false);
    SLOT.store(off as u8, std::sync::atomic::Ordering::Relaxed);
    off
}

/// Arm a freshly-connected writer session: SSI isolation, plus the optional
/// async-commit durability downgrade. `false` means the session could not be
/// configured, so the pool drops it rather than handing out a connection that
/// silently runs under the wrong isolation.
fn tune_writer(c: &mut Client) -> bool {
    if c.batch_execute(WRITER_ISOLATION_SQL).is_err() {
        return false;
    }
    if pg_sync_commit_off() && c.batch_execute("SET synchronous_commit = off").is_err() {
        return false;
    }
    true
}

impl PgEngine {
    /// Connect to a PostgreSQL server (from a `postgres://user:pw@host/db`
    /// URI), ensure this view's schema exists (idempotent), and seed the root
    /// row (id=1). The URI is passed through verbatim to [`postgres::Client`].
    ///
    /// `readonly` is accepted for API parity with the SQLite engine but has no
    /// effect on the PG path (GroveFS-native HA is disabled on PG, so a
    /// "standby reads only" mode is meaningless here).
    pub fn open(conn_uri: &str, view: &str, _readonly: bool) -> std::io::Result<Self> {
        // A comma-separated spec (`uri,uri,...`) names several instances; this
        // view's tree lives on exactly one of them (see [`route_store`]).
        let conn_uri = route_store(conn_uri, view);
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
        for _ in 0..pg_write_pool_size() {
            if let Ok(mut wc) = Client::connect(&conn_uri, NoTls) {
                if tune_writer(&mut wc) {
                    write_pool.push(WriteConn::new(wc));
                }
            }
        }
        // Pre-fill the read pool so the first big-directory scan doesn't pay
        // connect latency per page. Reads stay at READ COMMITTED (the PG
        // default) — cheaper and no SSI predicate-lock overhead.
        let mut read_pool = Vec::new();
        for _ in 0..pg_read_pool_size() {
            if let Ok(rc) = Client::connect(&conn_uri, NoTls) {
                read_pool.push(ReadConn::new(rc));
            }
        }
        let this = Self {
            view: view.to_string(),
            schema,
            conn_uri,
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
    fn checkout_writer(&self) -> Result<WriteConn, MetaError> {
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
                // every writer session is guaranteed SERIALIZABLE (and the same
                // commit durability) regardless of how it entered the pool.
                let _ = tune_writer(&mut c);
                Ok(WriteConn::new(c))
            }
        }
    }

    /// Return a writer connection to the pool (bounded); drop it if the
    /// server closed the socket so a stale handle never gets handed out again.
    fn requeue_writer(&self, c: WriteConn) {
        if c.is_closed() {
            return;
        }
        let mut p = self.write_pool.lock();
        if p.len() < pg_write_pool_size() {
            p.push(c);
        }
    }

    /// Borrow a pooled read connection if one is free (skipping any closed ones
    /// left by a server restart), else open a fresh one. Returns `None` only
    /// when connecting also fails — the caller then falls back to a checked-out
    /// writer (see [`Self::with_read`]).
    fn try_read_conn(&self) -> Option<ReadConn> {
        let _pw = stats::start(stats::Stage::PgPoolWait);
        let pooled = {
            let mut p = self.read_pool.lock();
            loop {
                match p.pop() {
                    Some(c) if c.client.is_closed() => continue, // discard stale, try next
                    Some(c) => break Some(c),
                    None => break None,
                }
            }
        };
        match pooled {
            Some(c) => Some(c),
            None => Client::connect(&self.conn_uri, NoTls)
                .ok()
                .map(ReadConn::new),
        }
    }

    /// Return a used read connection to the pool (bounded); drop it if the
    /// server closed it so a stale socket never gets handed out again.
    fn requeue_read(&self, c: ReadConn) {
        if c.client.is_closed() {
            return;
        }
        let mut p = self.read_pool.lock();
        if p.len() < pg_read_pool_size() {
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
    fn with_read<T: Default>(&self, f: impl FnOnce(&mut ReadConn) -> T) -> T {
        if let Some(mut c) = self.try_read_conn() {
            let _q = stats::start(stats::Stage::PgQuery);
            let r = f(&mut c);
            self.requeue_read(c);
            return r;
        }
        match self.checkout_writer() {
            Ok(c) => {
                let _q = stats::start(stats::Stage::PgQuery);
                let mut c = ReadConn::new(c.client);
                let r = f(&mut c);
                self.requeue_writer(WriteConn::new(c.client));
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
        F: FnMut(&mut WriteTx<'_, '_>) -> Result<T, WriteErr>,
    {
        let mut delay = SERIALIZATION_BACKOFF_MIN;
        // Total write-transaction latency, retries included: the number that
        // actually lands on the caller's tail. SSI aborts inflate this well
        // above PgQuery/PgPoolWait, so comparing the two pinpoints contention.
        let _txn = stats::start(stats::Stage::PgWriteTxn);
        for attempt in 0..SERIALIZATION_MAX_ATTEMPTS {
            let mut wc = self.checkout_writer()?;
            let out: Result<T, WriteErr> = (|| {
                // Split the pooled connection so the transaction holds `client`
                // while the body keeps the statement cache: they are disjoint
                // fields and `WriteTx` re-joins them for the duration of one op.
                let WriteConn { client, stmts } = &mut wc;
                let mut tx = client.transaction().map_err(|e| {
                    if Self::is_serialization_failure(&e) {
                        WriteErr::Serialization
                    } else {
                        WriteErr::Hard(map_db_err(&e))
                    }
                })?;
                let v = {
                    let mut wtx = WriteTx {
                        tx: &mut tx,
                        stmts,
                    };
                    body(&mut wtx)?
                };
                tx.commit().map_err(|e| {
                    if Self::is_serialization_failure(&e) {
                        WriteErr::Serialization
                    } else {
                        WriteErr::Hard(map_db_err(&e))
                    }
                })?;
                Ok(v)
            })();
            self.requeue_writer(wc);
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

    /// Compute nlink/size for ONE raw row from counts the caller already
    /// fetched, and produce the NodeInfo. This is the single source of truth for
    /// the kind -> nlink/size mapping: `getattr`/`lookup` reach it by folding the
    /// two `COUNT`s into the row `SELECT` as correlated scalar subqueries (one
    /// server round-trip per stat instead of two), and `finalize_batch` by
    /// looking each row's count up in a grouped `COUNT ... GROUP BY` it fetched
    /// for the whole page. Only the count the row's kind needs is applied: a
    /// directory's nlink is `2 +` its child-directory count, a regular file's is
    /// its hard-link (holder) count, a symlink needs neither.
    fn finalize_counts(raw: &RawNode, subdir_count: i64, holder_count: i64) -> NodeInfo {
        let kind = Self::node_kind(raw);
        let (nlink, size) = match kind {
            NodeKind::Dir => (2 + subdir_count as u64, 4096u64),
            NodeKind::Symlink => (
                1,
                raw.link_target
                    .as_ref()
                    .map(|s| s.len() as u64)
                    .unwrap_or(0),
            ),
            NodeKind::Regular => (holder_count.max(1) as u64, 0u64),
        };
        Self::build_info(raw, kind, nlink, size)
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
                // Look up the row's page-batched counts and hand them to the one
                // scalar mapping — identical output to the old per-kind inline
                // match, with the formula kept in a single place.
                let subdir_count = *subdir_map.get(&r.id).unwrap_or(&0);
                let holder_count = *nlink_map.get(&r.ino).unwrap_or(&1);
                Self::finalize_counts(r, subdir_count, holder_count)
            })
            .collect()
    }

    pub fn lookup(&self, parent: i64, name: &str) -> Option<NodeInfo> {
        let _t = stats::start(stats::Stage::MetaLookup);
        let node = self.t("node");
        // Row + both candidate counts in ONE round-trip; `finalize_counts` applies
        // only the one the kind needs. Folding the counts here is what keeps a
        // `lookup` a single server trip instead of a `SELECT` plus a follow-up
        // `COUNT`.
        let sql = format!(
            "SELECT {COLS},
               (SELECT COUNT(*) FROM {node} d WHERE d.parent_id = nd.id AND d.is_dir = 1),
               (SELECT COUNT(*) FROM {node} h WHERE h.ino = nd.ino)
             FROM {node} nd WHERE nd.parent_id = $1 AND nd.name = $2"
        );
        self.with_read(|c| {
            let row = c.query(&sql, &[&parent, &name]).ok()?.into_iter().next()?;
            let raw = parse_node(&row);
            let subdir_count: i64 = row.get(13);
            let holder_count: i64 = row.get(14);
            Some(Self::finalize_counts(&raw, subdir_count, holder_count))
        })
    }

    pub fn getattr(&self, ino: i64) -> Option<NodeInfo> {
        let _t = stats::start(stats::Stage::MetaGetattr);
        let node = self.t("node");
        // One round-trip, as in `lookup`. `ino` is shared by a hard link's names,
        // so `LIMIT 1` picks one row while the holder `COUNT` still sees all N —
        // matching the pre-fold `finalize` that counted on the row's own `ino`.
        let sql = format!(
            "SELECT {COLS},
               (SELECT COUNT(*) FROM {node} d WHERE d.parent_id = nd.id AND d.is_dir = 1),
               (SELECT COUNT(*) FROM {node} h WHERE h.ino = nd.ino)
             FROM {node} nd WHERE nd.ino = $1 LIMIT 1"
        );
        self.with_read(|c| {
            let row = c.query(&sql, &[&ino]).ok()?.into_iter().next()?;
            let raw = parse_node(&row);
            let subdir_count: i64 = row.get(13);
            let holder_count: i64 = row.get(14);
            Some(Self::finalize_counts(&raw, subdir_count, holder_count))
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
    ) -> Result<RawNode, MetaError> {
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
            // Return the row as just written. Every scalar column is known from
            // the INSERT arguments and the generated id (a brand-new name owns a
            // brand-new inode, ino == id), and mtime/atime/ctime are the single
            // `t` written above — so a caller that needs the full `NodeInfo` can
            // finalize it in memory instead of paying a second `getattr` round
            // trip to the server. `nlink`/`size` for a fresh node are derived by
            // [`Self::fresh_info`].
            Ok(RawNode {
                id,
                parent_id: Some(parent),
                name: name.to_string(),
                is_dir: is_dir as i64,
                data_ref: data_ref.map(|s| s.to_string()),
                link_target: link_target.map(|s| s.to_string()),
                mode: mode as i64,
                uid: uid as i64,
                gid: gid as i64,
                mtime: t,
                atime: t,
                ctime: t,
                ino: id,
            })
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
            .map(|r| r.id)
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
            .map(|r| r.id)
    }

    /// Create and return the full `NodeInfo` in ONE write round-trip.
    ///
    /// The trait default would follow the INSERT with a `getattr`, i.e. check
    /// out a read connection and re-SELECT + `COUNT` the row back — a second
    /// network round-trip to the database server on every `CREATE`. Since a
    /// fresh node's every field is known from the write (see [`PgEngine::insert`]`
    /// ’s returned `RawNode`) and its `nlink`/`size` are deterministic (see
    /// [`Self::fresh_info`]), we assemble the reply in memory and skip the
    /// read trip entirely — the same contract the `RemoteMeta` client override
    /// ships. Reporting the created node (rather than re-reading) also matches
    /// that override: the create committed, so it succeeded.
    #[allow(clippy::too_many_arguments)]
    pub fn create_node_with_attr(
        &self,
        parent: i64,
        name: &str,
        is_dir: bool,
        data_ref: Option<&str>,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<NodeInfo, MetaError> {
        let _t = stats::start(stats::Stage::MetaCreate);
        let raw = self.insert(parent, name, is_dir, data_ref, None, mode, uid, gid)?;
        Ok(Self::fresh_info(&raw))
    }

    pub fn create_symlink_with_attr(
        &self,
        parent: i64,
        name: &str,
        target: &str,
        uid: u32,
        gid: u32,
    ) -> Result<NodeInfo, MetaError> {
        let _t = stats::start(stats::Stage::MetaCreate);
        let raw = self.insert(parent, name, false, None, Some(target), 0o0777, uid, gid)?;
        Ok(Self::fresh_info(&raw))
    }

    /// `NodeInfo` for a just-created node, computed WITHOUT a store read. Its
    /// `nlink`/`size` are exactly what [`Self::finalize`] derives for a node
    /// that has no other names and no children yet — which is guaranteed for
    /// a brand-new inode (`create` always mints a fresh inode; hard links go
    /// through `link_node`): a fresh directory is `2 + 0` subdirs / size 4096,
    /// a fresh regular file is 1 name / size 0 (bytes land later, via write),
    /// a fresh symlink is 1 name / size = target length.
    fn fresh_info(raw: &RawNode) -> NodeInfo {
        let kind = Self::node_kind(raw);
        let (nlink, size) = match kind {
            NodeKind::Dir => (2u64, 4096u64),
            NodeKind::Symlink => (
                1,
                raw.link_target
                    .as_ref()
                    .map(|s| s.len() as u64)
                    .unwrap_or(0),
            ),
            NodeKind::Regular => (1, 0),
        };
        Self::build_info(raw, kind, nlink, size)
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
    /// duplicating here. `meta-server --meta-store postgres://...` therefore rejects
    /// `--role/--peer/--witness` at CLI parse time and never reaches this
    /// method; the impl is only present to satisfy the MetaStore enum's
    /// uniform interface.
    pub fn backup_to_bytes(&self) -> std::io::Result<Vec<u8>> {
        Err(std::io::Error::other(
            "GroveFS-native HA snapshot is SQLite-only; use PostgreSQL's own replication for a PG-backed meta-server",
        ))
    }

    // ---- base-view teardown and ledger repair (governance plane) -----------
    //
    // The SQLite arm unlinks `meta/<view>.db` and then rebuilds a file ledger;
    // the FDB arm clears a subspace and unwinds the cluster ledger. On PG the
    // tree is a `grovefs_v_<hex>` schema and the ledger is
    // `grovefs_refs.holders`, so a teardown is "unbook what the tree held, then
    // drop the schema" — and because PostgreSQL's DDL is TRANSACTIONAL both
    // halves fit in ONE transaction. That is strictly stronger than the file
    // arm, which needs a rebuild pass afterwards: there is no instant where the
    // tree is gone but its holds are still booked.

    /// Remove a whole BASE tree: unbook every content key `view` held, then
    /// drop its schema, atomically. Reclaiming the bytes of the refs that
    /// reached zero is the CALLER's job (it owns a data root); this reports
    /// which refs those are.
    ///
    /// A missing schema is an error, never a cheerful zero: a typo must not
    /// read as a successful removal (the same rule the file arm applies to a
    /// missing `meta/<view>.db`).
    pub fn remove_view(conn_uri: &str, view: &str) -> Result<PgPurgeReport, String> {
        // A sharded spec names several instances; this tree lives on one of
        // them, and so does the ledger it has to unbook (see [`route_store`]).
        let conn_uri = &route_store(conn_uri, view);
        let schema = schema_for(view).ok_or_else(|| {
            format!(
                "view name '{view}' incompatible with PG schema naming (empty, or longer than 25 bytes)"
            )
        })?;
        let mut c = Client::connect(conn_uri, NoTls).map_err(|e| format!("postgres connect {e}"))?;
        let mut tx = c.transaction().map_err(|e| format!("postgres begin {e}"))?;
        let there = tx
            .query(
                "SELECT 1 FROM information_schema.schemata WHERE schema_name = $1",
                &[&schema],
            )
            .map_err(|e| format!("postgres {e}"))?;
        if there.is_empty() {
            return Err(format!(
                "no schema {schema} in the database — '{view}' has no base tree here (a governed name is removed with `view rm` WITHOUT --base; a name this store never had cannot be removed twice)"
            ));
        }
        // Read the refs BEFORE the drop: once the schema is gone the database
        // has no record of what it held (the same reason the file arm reads the
        // tree while the db still exists).
        let entries: i64 = tx
            .query(&format!("SELECT COUNT(*) FROM {schema}.node"), &[])
            .map_err(|e| format!("postgres {e}"))?
            .first()
            .map(|r| r.get::<_, i64>(0))
            .unwrap_or(0);
        let refs: Vec<String> = tx
            .query(
                &format!("SELECT DISTINCT data_ref FROM {schema}.node WHERE data_ref IS NOT NULL"),
                &[],
            )
            .map_err(|e| format!("postgres {e}"))?
            .iter()
            .map(|r| r.get::<_, String>(0))
            .collect();
        let (mut unbooked_refs, mut still_held) = (Vec::new(), 0usize);
        for dr in &refs {
            // One ledger row per content key per VIEW, so a whole-tree teardown
            // decrements once — not once per name. A missing row reads as "no
            // other holder" (it was never booked), matching `ledger_remove_tx`.
            let left: Option<i64> = tx
                .query(
                    &format!(
                        "UPDATE {PG_REFS_TABLE} SET holders = holders - 1 WHERE data_ref=$1 RETURNING holders"
                    ),
                    &[dr],
                )
                .map_err(|e| format!("postgres {e}"))?
                .into_iter()
                .next()
                .map(|r| r.get::<_, i64>(0));
            match left {
                Some(h) if h > 0 => still_held += 1,
                _ => {
                    tx.execute(
                        &format!("DELETE FROM {PG_REFS_TABLE} WHERE data_ref=$1"),
                        &[dr],
                    )
                    .map_err(|e| format!("postgres {e}"))?;
                    unbooked_refs.push(dr.clone());
                }
            }
        }
        // CASCADE is required, not incidental: the schema's three tables ARE
        // the tree, so dropping the schema without them is not a teardown.
        tx.batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
            .map_err(|e| format!("postgres drop schema {schema}: {e}"))?;
        tx.commit()
            .map_err(|e| format!("postgres commit (schema and ledger rolled back): {e}"))?;
        Ok(PgPurgeReport {
            entries: entries.max(0) as u64,
            unbooked: unbooked_refs.len(),
            unbooked_refs,
            still_held,
        })
    }

    // ---- tree copies (fork / graft) ---------------------------------------
    //
    // The file arm's copies (`meta::fork_view_with` / `meta::graft_view`) read
    // and write SEPARATE SQLite databases. Under PG the two trees are two
    // schemas of one database, so the same semantics land as SQL: `fork` is one
    // `INSERT ... SELECT` plus one ledger statement, and `graft` walks the
    // source rows in Rust and inserts fresh rows exactly like the file arm.
    // Because PostgreSQL's DDL is transactional, the fork's copy+booking and
    // the graft's rows+booking each live in ONE transaction: a crash leaves
    // either no tree or a fully copied one, never a half tree with unbooked
    // blobs.
    //
    // Shared on purpose with the SQLite arm: a fork preserves ids AND inode
    // numbers (hard links in the base stay hard links), `seed == false` creates
    // the fork's own empty tree (root only) for a later graft, and both book
    // ONE ledger holder per DISTINCT blob the destination now names.

    /// Does `schema` exist? Asked of `information_schema`, never of a cached
    /// engine: a fork/graft must answer about the store as it is NOW.
    pub fn tree_exists(conn_uri: &str, view: &str) -> Result<bool, String> {
        let conn_uri = &route_store(conn_uri, view);
        let schema = schema_for(view).ok_or_else(|| {
            format!(
                "view name '{view}' incompatible with PG schema naming (empty, or longer than 25 bytes)"
            )
        })?;
        let mut c =
            Client::connect(conn_uri, NoTls).map_err(|e| format!("postgres connect {e}"))?;
        schema_exists(&mut c, &schema)
    }

    /// Seed an independent tree `name` as a point-in-time copy of the physical
    /// view `base`'s tree. Content bytes are SHARED, not copied: every distinct
    /// `data_ref` in the copy is booked once against the new view in the
    /// cross-view ledger, so a delete in either tree frees a blob only when its
    /// last name in the last holding view disappears.
    ///
    /// Not copied on purpose: `ctrl` (the HA fencing epoch — a fork is a new
    /// view, not a standby replica) and `locks` (transient leases of the source
    /// mount). Inode numbers ARE copied, so hard links inside the base stay hard
    /// links in the fork; new nodes in the fork allocate `ino == row id` off the
    /// sequence this function re-points above the copied ids.
    ///
    /// `seed == false` creates the fork's OWN tree (schema + root) and copies
    /// nothing, so the caller can place a curated subset with [`graft_tree`].
    pub fn fork_tree(
        conn_uri: &str,
        base: &str,
        name: &str,
        seed: bool,
    ) -> Result<crate::meta::ForkStats, String> {
        // Both trees must sit on ONE instance: the copy is a single
        // `INSERT ... SELECT` between two schemas (see [`route_multi`]).
        let conn_uri = &route_multi(conn_uri, &[base, name])?;
        let src = schema_for(base).ok_or_else(|| {
            format!(
                "view name '{base}' incompatible with PG schema naming (empty, or longer than 25 bytes)"
            )
        })?;
        let dst = schema_for(name).ok_or_else(|| {
            format!(
                "view name '{name}' incompatible with PG schema naming (empty, or longer than 25 bytes)"
            )
        })?;
        let mut c =
            Client::connect(conn_uri, NoTls).map_err(|e| format!("postgres connect {e}"))?;
        if !schema_exists(&mut c, &src)? {
            return Err(format!(
                "no physical base tree '{base}' in {conn_uri} — a fork copies a physical view tree"
            ));
        }
        if schema_exists(&mut c, &dst)? {
            return Err(format!(
                "fork target '{name}' already exists in {conn_uri}"
            ));
        }
        // Parity with the file arm: an empty source (no root row) is a store
        // shape a fork must refuse, not copy as a tree with no root.
        let has_root: bool = !c
            .query(
                &format!("SELECT 1 FROM {src}.node WHERE parent_id IS NULL LIMIT 1"),
                &[],
            )
            .map_err(|e| format!("postgres {e}"))?
            .is_empty();
        if !has_root {
            return Err(format!(
                "base tree '{base}' has no root row — refusing to fork an empty store"
            ));
        }
        // Create the destination (schema + root + ledger attached), exactly the
        // shape `view create` makes; `seed == false` stops here.
        {
            let eng = PgEngine::open(conn_uri, name, false)
                .map_err(|e| format!("cannot create fork tree '{name}': {e}"))?;
            drop(eng);
        }
        if !seed {
            return Ok(crate::meta::ForkStats { nodes: 1, blobs: 0 });
        }
        let mut tx = c
            .transaction()
            .map_err(|e| format!("postgres begin {e}"))?;
        // REPLACE the seeded root with the copied rows: ids and inos travel, so
        // the copy is the same tree, not a re-numbered imitation.
        tx.execute(&format!("DELETE FROM {dst}.node"), &[])
            .map_err(|e| format!("postgres {e}"))?;
        tx.execute(
            &format!(
                "INSERT INTO {dst}.node ({COLS}) SELECT {COLS} FROM {src}.node ORDER BY id"
            ),
            &[],
        )
        .map_err(|e| format!("postgres copy into {name}: {e}"))?;
        // Explicit ids do not advance BIGSERIAL; re-point it above the copied
        // maximum or the first new node in the fork collides with a copy.
        let _ = tx
            .query(
                &format!(
                    "SELECT setval(pg_get_serial_sequence('{dst}.node','id'), \
                     COALESCE((SELECT MAX(id) FROM {dst}.node), 1))"
                ),
                &[],
            )
            .map_err(|e| format!("postgres sequence {name}: {e}"))?;
        let nodes: i64 = tx
            .query(&format!("SELECT COUNT(*) FROM {dst}.node"), &[])
            .map_err(|e| format!("postgres {e}"))?[0]
            .get(0);
        let blobs: i64 = tx
            .query(
                &format!("SELECT COUNT(DISTINCT data_ref) FROM {dst}.node WHERE data_ref IS NOT NULL"),
                &[],
            )
            .map_err(|e| format!("postgres {e}"))?[0]
            .get(0);
        // Book the fork's own hold on every distinct blob. One row per content
        // key per VIEW (the base already holds its own), exactly like
        // `ledger_add_tx` on a 0->1 name count.
        tx.execute(
            &format!(
                "INSERT INTO {PG_REFS_TABLE} (data_ref, holders) \
                 SELECT data_ref, 1 FROM (SELECT DISTINCT data_ref FROM {dst}.node \
                 WHERE data_ref IS NOT NULL) s \
                 ON CONFLICT (data_ref) DO UPDATE SET holders = {PG_REFS_TABLE}.holders + 1"
            ),
            &[],
        )
        .map_err(|e| format!("cannot book the fork's blobs in the ledger: {e}"))?;
        tx.commit()
            .map_err(|e| format!("postgres commit (fork and ledger rolled back): {e}"))?;
        Ok(crate::meta::ForkStats { nodes, blobs })
    }

    /// One node row of a PG tree being read for a graft (same shape as the file
    /// arm's private `GraftRow`): see the module-level [`GraftPgRow`].
    ///
    /// Mutable state of one graft transaction: see the module-level
    /// [`GraftCtx`].

    /// Graft a curated subset of `src`'s tree into the tree of `dst`: for every
    /// [`crate::meta::GraftItem`], copy that entry (a directory brings its
    /// subtree) to `dest/<dst>`, renaming it to the destination basename.
    /// Bytes are shared — every distinct `data_ref` is booked exactly like a
    /// fork's, so the destination never owns a delete the source does not know
    /// about and the object is freed only when no view names it.
    ///
    /// All-or-nothing: the source is read first (one statement, so one
    /// snapshot) and the whole plan lands in ONE transaction; a bad line writes
    /// nothing. Missing parent directories under `dest` are created (0755).
    /// `skip_existing` turns a conflict into "leave what is there and keep
    /// going", merging into an existing directory and skipping colliding files.
    pub fn graft_tree(
        conn_uri: &str,
        dst: &str,
        src: &str,
        items: &[crate::meta::GraftItem],
        dest: &str,
        skip_existing: bool,
    ) -> Result<crate::meta::GraftStats, String> {
        if items.is_empty() {
            return Err("empty graft plan — nothing to copy".to_string());
        }
        // Same one-instance rule as a fork: the plan is read from one schema and
        // written into another schema of the SAME database (see [`route_multi`]).
        let conn_uri = &route_multi(conn_uri, &[src, dst])?;
        let src_s = schema_for(src).ok_or_else(|| {
            format!(
                "view name '{src}' incompatible with PG schema naming (empty, or longer than 25 bytes)"
            )
        })?;
        let dst_s = schema_for(dst).ok_or_else(|| {
            format!(
                "view name '{dst}' incompatible with PG schema naming (empty, or longer than 25 bytes)"
            )
        })?;
        let mut c =
            Client::connect(conn_uri, NoTls).map_err(|e| format!("postgres connect {e}"))?;
        if !schema_exists(&mut c, &src_s)? {
            return Err(format!(
                "no source tree '{src}' in {conn_uri} — a graft copies names out of a physical view tree"
            ));
        }
        if !schema_exists(&mut c, &dst_s)? {
            return Err(format!(
                "no destination tree '{dst}' in {conn_uri} — a graft lands in a base view or a fork (a lens/alias has no tree of its own)"
            ));
        }
        let dest_prefix = if dest.trim().is_empty() {
            String::new()
        } else {
            crate::meta::normalize_plan_path(dest)?
        };
        let rows = read_graft_rows(&mut c, &src_s)?;
        let root = rows
            .iter()
            .find(|r| r.parent_id.is_none())
            .map(|r| r.id)
            .ok_or_else(|| {
                format!("source tree '{src}' has no root row — refusing to graft from an empty store")
            })?;
        let children = graft_children(&rows);
        // Resolve every source path BEFORE writing anything, so a typo on line
        // 900 of a 100k-line plan cannot leave a half-grafted tree behind.
        let mut plan: Vec<(usize, String)> = Vec::with_capacity(items.len());
        for it in items {
            let idx = graft_resolve(&rows, &children, root, &it.src)
                .ok_or_else(|| format!("'{}' does not exist in view '{src}'", it.src))?;
            let full = if dest_prefix.is_empty() {
                it.dst.clone()
            } else {
                format!("{dest_prefix}/{}", it.dst)
            };
            plan.push((idx, full));
        }
        let mut tx = c
            .transaction()
            .map_err(|e| format!("postgres begin {e}"))?;
        let dst_root: i64 = tx
            .query(
                &format!("SELECT id FROM {dst_s}.node WHERE parent_id IS NULL ORDER BY id LIMIT 1"),
                &[],
            )
            .map_err(|e| format!("postgres {e}"))?
            .first()
            .map(|r| r.get::<_, i64>(0))
            .ok_or_else(|| {
                format!("destination tree '{dst}' has no root row — refusing to graft into it")
            })?;
        // Fresh ids/ino numbers start ABOVE both existing id space and existing
        // inode numbers, so a graft never collides with rows copied by an
        // earlier fork (whose inos came from another tree).
        let max_id: i64 = tx
            .query(&format!("SELECT COALESCE(MAX(id),0) FROM {dst_s}.node"), &[])
            .map_err(|e| format!("postgres {e}"))?[0]
            .get(0);
        let max_ino: i64 = tx
            .query(&format!("SELECT COALESCE(MAX(ino),0) FROM {dst_s}.node"), &[])
            .map_err(|e| format!("postgres {e}"))?[0]
            .get(0);
        let mut ctx = GraftCtx {
            next: max_id.max(max_ino) + 1,
            skip_existing,
            stats: crate::meta::GraftStats::default(),
            blobs: std::collections::HashSet::new(),
            inos: HashMap::new(),
        };
        // What the destination ALREADY names, read before a single grafted row
        // lands: the ledger counts holders per view, so these blobs must not be
        // booked again by this graft (see the booking loop below).
        let held_before: std::collections::HashSet<String> = tx
            .query(
                &format!("SELECT DISTINCT data_ref FROM {dst_s}.node WHERE data_ref IS NOT NULL"),
                &[],
            )
            .map_err(|e| format!("cannot size the destination tree: {e}"))?
            .iter()
            .map(|r| r.get::<_, String>(0))
            .collect();
        for (idx, full) in &plan {
            let (parent_path, name) = match full.rfind('/') {
                Some(k) => (&full[..k], &full[k + 1..]),
                None => ("", full.as_str()),
            };
            let parent =
                Self::graft_ensure_dir(&mut tx, &dst_s, parent_path, dst_root, &mut ctx)?;
            Self::graft_copy(
                &mut tx, &dst_s, &rows, &children, *idx, parent, name, &mut ctx,
            )?;
        }
        // Book the destination's hold on every blob it now names. The ledger
        // counts HOLDERS PER VIEW, not names, so a blob the destination already
        // named before this graft (a second graft into the same fork, or a
        // curated copy beside an earlier one) must NOT be booked again — that
        // would leave a holder nobody ever unbooks and the object would never be
        // freed. If the SOURCE view was never booked (a store predating the
        // ledger and never rebuilt), book it first — otherwise its last-name
        // delete would free a blob this graft still names.
        for dr in &ctx.blobs {
            let booked: bool = !tx
                .query(
                    &format!("SELECT 1 FROM {PG_REFS_TABLE} WHERE data_ref=$1 LIMIT 1"),
                    &[dr],
                )
                .map_err(|e| format!("postgres {e}"))?
                .is_empty();
            if !booked {
                ledger_add_tx(&mut tx, dr).map_err(write_err_str)?;
                ledger_add_tx(&mut tx, dr).map_err(write_err_str)?;
            } else if !held_before.contains(dr) {
                ledger_add_tx(&mut tx, dr).map_err(write_err_str)?;
            }
        }
        // The rows carried explicit ids: re-point the sequence so the next
        // create_node in the destination cannot collide with a grafted row.
        let _ = tx
            .query(
                &format!(
                    "SELECT setval(pg_get_serial_sequence('{dst_s}.node','id'), \
                     COALESCE((SELECT MAX(id) FROM {dst_s}.node), 1))"
                ),
                &[],
            )
            .map_err(|e| format!("postgres sequence {dst}: {e}"))?;
        tx.commit()
            .map_err(|e| format!("postgres commit (graft and ledger rolled back): {e}"))?;
        ctx.stats.blobs = ctx.blobs.len() as i64;
        Ok(ctx.stats)
    }

    /// `mkdir -p` inside the destination tree: walk `path` from `root`, creating
    /// any missing directory. Returns the directory's row id.
    fn graft_ensure_dir(
        tx: &mut postgres::Transaction<'_>,
        schema: &str,
        path: &str,
        root: i64,
        ctx: &mut GraftCtx,
    ) -> Result<i64, String> {
        let mut cur = root;
        if path.is_empty() {
            return Ok(cur);
        }
        for comp in path.split('/') {
            let found: Option<(i64, i64)> = tx
                .query(
                    &format!("SELECT id,is_dir FROM {schema}.node WHERE parent_id=$1 AND name=$2"),
                    &[&cur, &comp],
                )
                .map_err(|e| format!("cannot read the destination tree: {e}"))?
                .first()
                .map(|r| (r.get(0), r.get(1)));
            match found {
                Some((id, is_dir)) if is_dir != 0 => cur = id,
                Some(_) => {
                    return Err(format!(
                        "'{comp}' in the destination path is a file, not a directory"
                    ))
                }
                None => {
                    let id = ctx.next;
                    ctx.next += 1;
                    let t = now();
                    let uid = crate::platform::getuid() as i64;
                    let gid = crate::platform::getgid() as i64;
                    let no_ref: Option<String> = None;
                    let no_link: Option<String> = None;
                    tx.execute(
                        &format!(
                            "INSERT INTO {schema}.node ({COLS}) \
                             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)"
                        ),
                        &[
                            &id, &cur, &comp, &1i64, &no_ref, &no_link, &0o0755i64, &uid, &gid,
                            &t, &t, &t, &id,
                        ],
                    )
                    .map_err(|e| {
                        format!("cannot create directory '{comp}' in the destination: {e}")
                    })?;
                    ctx.stats.nodes += 1;
                    ctx.stats.dirs += 1;
                    cur = id;
                }
            }
        }
        Ok(cur)
    }

    /// Copy one source entry (and, for a directory, its whole subtree) under
    /// `parent` as `name`. Rows get fresh ids/ino numbers in the destination;
    /// the source's bytes are referenced, never copied.
    #[allow(clippy::too_many_arguments)]
    fn graft_copy(
        tx: &mut postgres::Transaction<'_>,
        schema: &str,
        rows: &[GraftPgRow],
        children: &HashMap<i64, Vec<usize>>,
        idx: usize,
        parent: i64,
        name: &str,
        ctx: &mut GraftCtx,
    ) -> Result<(), String> {
        let r = &rows[idx];
        let existing: Option<(i64, i64)> = tx
            .query(
                &format!("SELECT id,is_dir FROM {schema}.node WHERE parent_id=$1 AND name=$2"),
                &[&parent, &name],
            )
            .map_err(|e| format!("cannot read the destination tree: {e}"))?
            .first()
            .map(|x| (x.get(0), x.get(1)));
        let dir_id = match existing {
            None => {
                let id = ctx.next;
                ctx.next += 1;
                let ino = *ctx.inos.entry(r.ino).or_insert(id);
                tx.execute(
                    &format!(
                        "INSERT INTO {schema}.node ({COLS}) \
                         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)"
                    ),
                    &[
                        &id,
                        &parent,
                        &name,
                        &r.is_dir,
                        &r.data_ref,
                        &r.link_target,
                        &r.mode,
                        &r.uid,
                        &r.gid,
                        &r.mtime,
                        &r.atime,
                        &r.ctime,
                        &ino,
                    ],
                )
                .map_err(|e| format!("cannot graft '{name}': {e}"))?;
                ctx.stats.nodes += 1;
                if r.is_dir != 0 {
                    ctx.stats.dirs += 1;
                }
                if let Some(dr) = &r.data_ref {
                    ctx.blobs.insert(dr.clone());
                }
                if r.is_dir == 0 {
                    return Ok(());
                }
                id
            }
            Some((id, is_dir)) => {
                if !ctx.skip_existing {
                    return Err(format!(
                        "'{name}' already exists in the destination — rename it in the plan or pass --skip-existing"
                    ));
                }
                if is_dir == 0 || r.is_dir == 0 {
                    ctx.stats.skipped += 1;
                    return Ok(());
                }
                // Both are directories: keep the one already there and merge the
                // missing children into it (re-running a curation script must not
                // fail on the skeleton it made last time).
                id
            }
        };
        if let Some(kids) = children.get(&r.id) {
            for k in kids {
                let child_name = rows[*k].name.clone();
                Self::graft_copy(tx, schema, rows, children, *k, dir_id, &child_name, ctx)?;
            }
        }
        Ok(())
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
fn view_name_count<T: TxSql>(
    tx: &mut T,
    node: &str,
    dr: &str,
) -> Result<i64, WriteErr> {
    Ok(tx
        .tq(
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
fn ledger_add_tx<T: TxSql>(tx: &mut T, dr: &str) -> Result<(), WriteErr> {
    tx.te(
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
fn ledger_remove_tx<T: TxSql>(
    tx: &mut T,
    dr: &str,
) -> Result<Option<String>, WriteErr> {
    let left: Option<i64> = tx
        .tq(
            &format!("UPDATE {PG_REFS_TABLE} SET holders = holders - 1 WHERE data_ref=$1 RETURNING holders"),
            &[&dr],
        )?
        .into_iter()
        .next()
        .map(|r| r.get::<_, i64>(0));
    match left {
        Some(h) if h <= 0 => {
            tx.te(
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

/// What the surviving PG trees name and what the shared ledger books, for an
/// audit that runs OUTSIDE this process's view cache (`gc` served over the
/// control plane).
///
/// A pure READ: no transaction, nothing written. Only our own deterministic
/// `grovefs_v_<hex>` schemas are read as trees (a foreign schema that merely
/// starts with the prefix is not one), and an unreadable tree ABORTS the
/// picture: a skipped tree would make every blob it names look unreferenced,
/// which is exactly the direction that deletes live bytes.
#[derive(Clone, Debug, Default)]
pub struct PgPicture {
    /// View names decoded from the schemas that were actually read.
    pub trees: Vec<String>,
    /// Content key → number of trees naming it.
    pub refs: std::collections::BTreeMap<String, i64>,
    /// Content key → holder count currently booked (a missing ledger = none).
    pub booked: std::collections::BTreeMap<String, i64>,
}

pub fn reference_picture(conn_uri: &str) -> Result<PgPicture, String> {
    let mut c = Client::connect(conn_uri, NoTls).map_err(|e| format!("postgres connect {e}"))?;
    let schemas: Vec<String> = c
        .query(
            "SELECT schema_name FROM information_schema.schemata \
             WHERE schema_name LIKE 'grovefs_v_%' ORDER BY schema_name",
            &[],
        )
        .map_err(|e| format!("postgres {e}"))?
        .iter()
        .map(|r| r.get::<_, String>(0))
        .collect();
    let mut trees = Vec::new();
    let mut refs: std::collections::BTreeMap<String, i64> = Default::default();
    for s in &schemas {
        let Some(view) = crate::control::view_from_schema_name(s) else {
            continue;
        };
        let rows = c
            .query(
                &format!("SELECT DISTINCT data_ref FROM {s}.node WHERE data_ref IS NOT NULL"),
                &[],
            )
            .map_err(|e| {
                format!(
                    "cannot read tree {s}: {e} — refusing to audit from a partial view of the store (a skipped tree would make every blob it names look unreferenced)"
                )
            })?;
        trees.push(view);
        for r in rows {
            *refs.entry(r.get::<_, String>(0)).or_insert(0) += 1;
        }
    }
    // A database that never opened a view has no ledger table yet: that is
    // "nothing booked", not an error (the same rule the local scanner applies
    // to a missing `refs.db`).
    let booked: std::collections::BTreeMap<String, i64> = match c.query(
        &format!("SELECT data_ref, holders FROM {PG_REFS_TABLE}"),
        &[],
    ) {
        Ok(rows) => rows
            .iter()
            .map(|r| (r.get::<_, String>(0), r.get::<_, i64>(1)))
            .collect(),
        Err(e) if is_undefined_table(&e) => Default::default(),
        Err(e) => return Err(format!("postgres read {PG_REFS_TABLE}: {e}")),
    };
    Ok(PgPicture {
        trees,
        refs,
        booked,
    })
}

/// Recompute `grovefs_refs.holders` from the `grovefs_v_*` TREES.
///
/// This is the PG counterpart of `grovefs::meta::rebuild_ledger` (file store)
/// and `grovefs::fdb_store::rebuild_ledger` (cluster). The ledger is DERIVED
/// state: the only authoritative record of what a tree holds is the tree. A
/// schema removed out of band (a manual `DROP SCHEMA`, a restore, a database
/// cloned from an older backup) never unbooked its holds, so the counts drift
/// upward and bytes stay alive for nobody.
///
/// The recount reads every tree and then rewrites the ledger in one
/// transaction, so a tree that cannot be read ABORTS the whole run: a skipped
/// tree would make content it still names look unreferenced (the unsafe
/// direction, the same rule the offline `gc` scan applies). Run it QUIESCED —
/// a booking committed between the read pass and the write pass would be
/// overwritten with the pre-booking count. Nothing here frees object bytes;
/// that belongs to the data servers.
pub fn rebuild_ledger(conn_uri: &str) -> Result<PgLedgerReport, String> {
    // One database holds one `grovefs_refs.holders`. A sharded spec names
    // several databases with one ledger EACH, so a single run would recount
    // only the first — say it instead of silently half-repairing the store.
    if store_instances(conn_uri).len() > 1 {
        return Err(format!(
            "{conn_uri} names {} PostgreSQL instances; the cross-view ledger lives INSIDE each database, so recount one instance at a time — run `grovefs refs rebuild --meta-store <one uri>` once per shard",
            store_instances(conn_uri).len()
        ));
    }
    let mut c = Client::connect(conn_uri, NoTls).map_err(|e| format!("postgres connect {e}"))?;
    let schemas: Vec<String> = c
        .query(
            "SELECT schema_name FROM information_schema.schemata \
             WHERE schema_name LIKE 'grovefs_v_%' ORDER BY schema_name",
            &[],
        )
        .map_err(|e| format!("postgres {e}"))?
        .iter()
        .map(|r| r.get::<_, String>(0))
        .collect();
    let mut truth: std::collections::BTreeMap<String, i64> = Default::default();
    let mut trees = 0usize;
    for s in &schemas {
        // Only our own deterministic encoding: a foreign schema that merely
        // begins with the prefix is never read as a tree.
        if crate::control::view_from_schema_name(s).is_none() {
            continue;
        }
        let rows = c
            .query(
                &format!("SELECT DISTINCT data_ref FROM {s}.node WHERE data_ref IS NOT NULL"),
                &[],
            )
            .map_err(|e| {
                format!(
                    "cannot read tree {s}: {e} — refusing to recount from a partial view of the store (a skipped tree would make every blob it names look unreferenced)"
                )
            })?;
        trees += 1;
        for r in rows {
            *truth.entry(r.get::<_, String>(0)).or_insert(0) += 1;
        }
    }
    // A database that never opened a view has no ledger table yet; create it
    // (idempotent) instead of failing a recount that has nothing to correct.
    let mut current: Vec<(String, i64)> = match c.query(
        &format!("SELECT data_ref, holders FROM {PG_REFS_TABLE}"),
        &[],
    ) {
        Ok(rows) => rows
            .iter()
            .map(|r| (r.get::<_, String>(0), r.get::<_, i64>(1)))
            .collect(),
        Err(e) if is_undefined_table(&e) => {
            c.batch_execute(&format!(
                "CREATE SCHEMA IF NOT EXISTS {PG_REFS_SCHEMA};
                 CREATE TABLE IF NOT EXISTS {PG_REFS_TABLE} (
                     data_ref TEXT PRIMARY KEY,
                     holders  BIGINT NOT NULL
                 );"
            ))
            .map_err(|e| format!("postgres create {PG_REFS_TABLE}: {e}"))?;
            Vec::new()
        }
        Err(e) => return Err(format!("postgres read {PG_REFS_TABLE}: {e}")),
    };
    current.sort();
    let booked: std::collections::HashSet<String> =
        current.iter().map(|(d, _)| d.clone()).collect();
    let (mut rows_fixed, mut rows_deleted) = (0usize, 0usize);
    let mut tx = c.transaction().map_err(|e| format!("postgres begin {e}"))?;
    for (dr, held) in &current {
        match truth.get(dr) {
            Some(&t) if t == *held => {}
            Some(&t) => {
                tx.execute(
                    &format!("UPDATE {PG_REFS_TABLE} SET holders = $2 WHERE data_ref = $1"),
                    &[dr as &(dyn ToSql + Sync), &t],
                )
                .map_err(|e| format!("postgres {e}"))?;
                rows_fixed += 1;
            }
            None => {
                tx.execute(
                    &format!("DELETE FROM {PG_REFS_TABLE} WHERE data_ref = $1"),
                    &[dr as &(dyn ToSql + Sync)],
                )
                .map_err(|e| format!("postgres {e}"))?;
                rows_deleted += 1;
            }
        }
    }
    for (dr, &t) in &truth {
        if booked.contains(dr) {
            continue;
        }
        // The unsafe direction the recount exists to repair: a surviving tree
        // still names this key but nothing is booked against it.
        tx.execute(
            &format!(
                "INSERT INTO {PG_REFS_TABLE} (data_ref, holders) VALUES ($1, $2)
                 ON CONFLICT (data_ref) DO UPDATE SET holders = EXCLUDED.holders"
            ),
            &[dr as &(dyn ToSql + Sync), &t],
        )
        .map_err(|e| format!("postgres {e}"))?;
        rows_fixed += 1;
    }
    tx.commit()
        .map_err(|e| format!("postgres commit (ledger unchanged): {e}"))?;
    Ok(PgLedgerReport {
        trees,
        refs: truth.len(),
        holders: truth.values().sum(),
        rows_fixed,
        rows_deleted,
    })
}

// ---- portable dumps (export / import) ------------------------------------
//
// The SAME `grovefs-view-metadata` v1 line format the SQLite arm
// (`meta::export_view`/`import_view`) and the cluster arm
// (`FdbStore::export_jsonl`/`import_jsonl`) speak: a header line, then one JSON
// object per node. What travels is the NAME SPACE (ids, inode numbers, modes,
// times, `data_ref` keys) — never bytes. Content is located by `data_ref` alone,
// whose bucket path is a pure function of the key, so the same dump loads into
// any of the three backends and the bytes are found (or missing) the same way.

/// One row of `node`, in the export's column order (`COLS`).
fn pg_node_row_json(r: &postgres::Row) -> serde_json::Value {
    serde_json::json!({
        "id": r.get::<_, i64>(0),
        "parent_id": r.get::<_, Option<i64>>(1),
        "name": r.get::<_, String>(2),
        "is_dir": r.get::<_, i64>(3),
        "data_ref": r.get::<_, Option<String>>(4),
        "link_target": r.get::<_, Option<String>>(5),
        "mode": r.get::<_, i64>(6),
        "uid": r.get::<_, i64>(7),
        "gid": r.get::<_, i64>(8),
        "mtime": r.get::<_, i64>(9),
        "atime": r.get::<_, i64>(10),
        "ctime": r.get::<_, i64>(11),
        "ino": r.get::<_, Option<i64>>(12).unwrap_or(0),
    })
}

/// Dump one view's tree to `out` in the portable format. The read is one
/// `REPEATABLE READ` transaction, i.e. a real point-in-time snapshot: a mount
/// writing to this view meanwhile cannot make the dump tear (SQLite gets the
/// same from a read-only connection inside one snapshot; FoundationDB from one
/// read version).
///
/// A name with no schema is REFUSED rather than exported as an empty tree — the
/// exact rule both other arms apply, because a dump claiming `nodes:0` would
/// later import as a provisioned-but-empty tree that reads as success.
pub fn export_view(
    conn_uri: &str,
    view: &str,
    out: &Path,
    force: bool,
    definition: Option<String>,
) -> Result<crate::meta::ViewExportStats, String> {
    let schema = schema_for(view).ok_or_else(|| {
        format!(
            "view name '{view}' incompatible with PG schema naming (empty, or longer than 25 bytes)"
        )
    })?;
    let mut c = Client::connect(conn_uri, NoTls).map_err(|e| format!("postgres connect {e}"))?;
    if !schema_exists(&mut c, &schema)? {
        return Err(format!(
            "no base tree '{view}' in {conn_uri} (schema {schema}) — a PostgreSQL-backed view is \
             created with `view create`; a name the database never had is not exported, it is refused"
        ));
    }
    if out.exists() && !force {
        return Err(format!(
            "output {} already exists — pass --force to overwrite it",
            out.display()
        ));
    }
    let kind = match definition.as_deref() {
        Some(t) if t.lines().any(|l| l.trim_start().starts_with("fork ")) => "fork",
        _ => "physical",
    };
    let mut tx = c
        .build_transaction()
        .isolation_level(IsolationLevel::RepeatableRead)
        .read_only(true)
        .start()
        .map_err(|e| format!("cannot start export snapshot: {e}"))?;
    let rows = tx
        .query(
            &format!("SELECT {COLS} FROM {schema}.node ORDER BY id"),
            &[],
        )
        .map_err(|e| format!("cannot read tree '{view}': {e}"))?;
    let blobs = {
        let mut set: std::collections::HashSet<String> = Default::default();
        for r in &rows {
            if let Some(dr) = r.get::<_, Option<String>>(4) {
                set.insert(dr);
            }
        }
        set.len() as i64
    };
    // Write beside the destination and rename, exactly like the two other arms:
    // an interrupted export never leaves a half file at the requested path.
    let mut tmp = out.to_path_buf();
    let name = out.file_name().and_then(|n| n.to_str()).unwrap_or("export");
    tmp.set_file_name(format!(".{name}.tmp-{}", std::process::id()));
    let write = (|| -> Result<(), String> {
        let f = std::fs::File::create(&tmp).map_err(|e| format!("cannot create {}: {e}", tmp.display()))?;
        let mut w = BufWriter::new(f);
        let header = serde_json::json!({
            "format": crate::meta::VIEW_EXPORT_FORMAT,
            "version": crate::meta::VIEW_EXPORT_VERSION,
            "view": view,
            "kind": kind,
            "definition": definition,
            "exported_at": now(),
            "nodes": rows.len() as i64,
            "blobs": blobs,
        });
        writeln!(w, "{header}").map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
        for r in &rows {
            let row = pg_node_row_json(r);
            writeln!(w, "{row}").map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
        }
        w.flush()
            .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
        w.get_ref()
            .sync_all()
            .map_err(|e| format!("cannot flush {}: {e}", tmp.display()))
    })();
    let _ = tx.commit();
    if let Err(e) = write {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, out).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("cannot move export into {}: {e}", out.display())
    })?;
    Ok(crate::meta::ViewExportStats {
        nodes: rows.len() as i64,
        blobs,
        kind: kind.to_string(),
    })
}

/// Install a portable dump as this view's tree.
///
/// Unlike the file arm (build a scratch db, rename it) PostgreSQL has
/// transactional DDL, so the whole install is ONE transaction: the tree is at
/// every instant either the old one or the new one. The dump's ids and inode
/// numbers are kept (hard links stay hard links, no cached handle is
/// invalidated by renumbering), the ``id`` sequence is re-pointed above the
/// imported maximum, and the cross-view ledger is adjusted from the OLD refs to
/// the NEW ones in the same transaction.
///
/// A schema that did not exist before is provisioned first (that is the only
/// tree-building entry besides `view create`); if the dump then turns out to be
/// invalid the schema is DROPPED again, so a refused import never leaves a
/// provisioned-but-empty tree behind — the shape both other arms avoid.
pub fn import_view(
    conn_uri: &str,
    view: &str,
    input: &Path,
    replace: bool,
) -> Result<crate::meta::ViewImportStats, String> {
    let schema = schema_for(view).ok_or_else(|| {
        format!(
            "view name '{view}' incompatible with PG schema naming (empty, or longer than 25 bytes)"
        )
    })?;
    let f = std::fs::File::open(input).map_err(|e| format!("cannot open {}: {e}", input.display()))?;
    let mut rdr = BufReader::new(f);
    let mut header_line = String::new();
    if rdr
        .read_line(&mut header_line)
        .map_err(|e| format!("cannot read {}: {e}", input.display()))?
        == 0
    {
        return Err(format!(
            "{} is empty — not a grovefs view export",
            input.display()
        ));
    }
    if header_line.len() > (1 << 20) {
        return Err(format!("{}:1: header line too long", input.display()));
    }
    let header: serde_json::Value = serde_json::from_str(header_line.trim_end()).map_err(|e| {
        format!(
            "{}:1: not a grovefs view export header ({e})",
            input.display()
        )
    })?;
    if header["format"] != crate::meta::VIEW_EXPORT_FORMAT {
        return Err(format!(
            "{}:1: unknown format '{}' (expected '{}')",
            input.display(),
            header["format"].as_str().unwrap_or("<missing>"),
            crate::meta::VIEW_EXPORT_FORMAT
        ));
    }
    let version = header["version"].as_u64().unwrap_or(0);
    if version != crate::meta::VIEW_EXPORT_VERSION {
        return Err(format!(
            "{}:1: unsupported export version {version} (this build reads {})",
            input.display(),
            crate::meta::VIEW_EXPORT_VERSION
        ));
    }
    let claimed_nodes = header["nodes"].as_i64();
    let claimed_blobs = header["blobs"].as_i64();
    let kind = header["kind"].as_str().unwrap_or("physical").to_string();

    let mut c = Client::connect(conn_uri, NoTls).map_err(|e| format!("postgres connect {e}"))?;
    let existed = schema_exists(&mut c, &schema)?;
    if existed && !replace {
        return Err(format!(
            "target tree '{view}' already exists in {conn_uri} (schema {schema}) — pass --replace to \
             overwrite it (stop every mount/meta-server using this store first)"
        ));
    }
    let created = !existed;
    if created {
        // Provisions the schema, its tables, the root row and the ledger table.
        drop(
            PgEngine::open(conn_uri, view, false)
                .map_err(|e| format!("cannot create target tree '{view}': {e}"))?,
        );
    }
    let built = (|| -> Result<(i64, i64), String> {
        let mut tx = c
            .transaction()
            .map_err(|e| format!("cannot start import transaction: {e}"))?;
        // Read what the tree held BEFORE clearing it: those holds are about to
        // become false, and the ledger is adjusted (not rebuilt) so a
        // concurrent `fork`/`graft` elsewhere is not overwritten.
        let old_refs: Vec<String> = tx
            .query(
                &format!(
                    "SELECT DISTINCT data_ref FROM {schema}.node WHERE data_ref IS NOT NULL"
                ),
                &[],
            )
            .map_err(|e| format!("cannot read the target tree: {e}"))?
            .iter()
            .map(|r| r.get::<_, String>(0))
            .collect();
        tx.execute(&format!("DELETE FROM {schema}.node"), &[])
            .map_err(|e| format!("cannot clear the target tree: {e}"))?;
        let mut nodes = 0i64;
        let mut blobs: std::collections::HashSet<String> = Default::default();
        {
            let sql = format!(
                "INSERT INTO {schema}.node ({COLS}) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)"
            );
            let mut line = String::new();
            let mut lineno = 1usize;
            loop {
                line.clear();
                let n = rdr
                    .read_line(&mut line)
                    .map_err(|e| format!("cannot read {}: {e}", input.display()))?;
                if n == 0 {
                    break;
                }
                lineno += 1;
                if line.len() > (4 << 20) {
                    return Err(format!("{}:{lineno}: line too long", input.display()));
                }
                if line.trim().is_empty() {
                    continue;
                }
                let v: serde_json::Value = serde_json::from_str(line.trim_end())
                    .map_err(|e| format!("{}:{lineno}: invalid JSON: {e}", input.display()))?;
                let id = v["id"].as_i64().ok_or_else(|| {
                    format!("{}:{lineno}: missing integer \"id\"", input.display())
                })?;
                let parent_id = match v.get("parent_id") {
                    None | Some(serde_json::Value::Null) => None,
                    Some(x) => Some(x.as_i64().ok_or_else(|| {
                        format!(
                            "{}:{lineno}: \"parent_id\" must be an integer or null",
                            input.display()
                        )
                    })?),
                };
                let name = v["name"].as_str().ok_or_else(|| {
                    format!("{}:{lineno}: missing string \"name\"", input.display())
                })?;
                if name.is_empty() {
                    return Err(format!("{}:{lineno}: empty name", input.display()));
                }
                let is_dir = v["is_dir"].as_i64().ok_or_else(|| {
                    format!("{}:{lineno}: missing integer \"is_dir\"", input.display())
                })?;
                if is_dir != 0 && is_dir != 1 {
                    return Err(format!(
                        "{}:{lineno}: \"is_dir\" must be 0 or 1, got {is_dir}",
                        input.display()
                    ));
                }
                let data_ref = v["data_ref"].as_str().map(str::to_string);
                let link_target = v["link_target"].as_str().map(str::to_string);
                let mode = v["mode"].as_i64().unwrap_or(if is_dir == 1 { 0o755 } else { 0o644 });
                let uid = v["uid"].as_i64().unwrap_or(0);
                let gid = v["gid"].as_i64().unwrap_or(0);
                let t = now();
                let mtime = v["mtime"].as_i64().unwrap_or(t);
                let atime = v["atime"].as_i64().unwrap_or(mtime);
                let ctime = v["ctime"].as_i64().unwrap_or(mtime);
                let ino = v["ino"].as_i64().ok_or_else(|| {
                    format!("{}:{lineno}: missing integer \"ino\"", input.display())
                })?;
                tx.execute(
                    &sql,
                    &[
                        &id,
                        &parent_id,
                        &name,
                        &is_dir,
                        &data_ref,
                        &link_target,
                        &mode,
                        &uid,
                        &gid,
                        &mtime,
                        &atime,
                        &ctime,
                        &ino,
                    ],
                )
                .map_err(|e| {
                    format!("{}:{lineno}: cannot insert node {id}: {e}", input.display())
                })?;
                if let Some(dr) = &data_ref {
                    blobs.insert(dr.clone());
                }
                nodes += 1;
            }
        }
        // The same three structural checks the file arm runs before it lets the
        // imported tree become the view: one root, it IS the root, no dangling
        // parent, and the header's counts must match what the file carried.
        let root: Vec<(i64, i64)> = tx
            .query(
                &format!("SELECT id, is_dir FROM {schema}.node WHERE parent_id IS NULL"),
                &[],
            )
            .map_err(|e| format!("cannot validate the imported tree: {e}"))?
            .iter()
            .map(|r| (r.get(0), r.get(1)))
            .collect();
        if root.len() != 1 {
            return Err(format!(
                "import rejected: expected exactly one root row (parent_id NULL), found {}",
                root.len()
            ));
        }
        if root[0].0 != crate::meta::ROOT_INO || root[0].1 != 1 {
            return Err(format!(
                "import rejected: root row must be id {} and a directory (got id {}, is_dir {})",
                crate::meta::ROOT_INO,
                root[0].0,
                root[0].1
            ));
        }
        let dangling: i64 = tx
            .query(
                &format!(
                    "SELECT COUNT(*) FROM {schema}.node c LEFT JOIN {schema}.node p ON c.parent_id=p.id \
                     WHERE c.parent_id IS NOT NULL AND (p.id IS NULL OR p.is_dir=0)"
                ),
                &[],
            )
            .map_err(|e| format!("cannot validate the imported tree: {e}"))?[0]
            .get(0);
        if dangling != 0 {
            return Err(format!(
                "import rejected: {dangling} node(s) point at a missing or non-directory parent"
            ));
        }
        if let Some(cn) = claimed_nodes {
            if cn != nodes {
                return Err(format!(
                    "import rejected: header declares {cn} node(s), file carries {nodes}"
                ));
            }
        }
        if let Some(cb) = claimed_blobs {
            if cb != blobs.len() as i64 {
                return Err(format!(
                    "import rejected: header declares {cb} blob reference(s), file carries {}",
                    blobs.len()
                ));
            }
        }
        // Explicit ids do not advance BIGSERIAL: re-point it above the imported
        // maximum or the next create collides with a row the dump brought.
        let _ = tx
            .query(
                &format!(
                    "SELECT setval(pg_get_serial_sequence('{schema}.node','id'), \
                     COALESCE((SELECT MAX(id) FROM {schema}.node), 1))"
                ),
                &[],
            )
            .map_err(|e| format!("postgres sequence {view}: {e}"))?;
        // Ledger: the holds the OLD tree had leave, the NEW tree's distinct refs
        // book one holder each — the same "per view, per content key" rule a
        // fork books with, so a blob shared with another view never drops to 0.
        for dr in &old_refs {
            let left: Option<i64> = tx
                .query(
                    &format!(
                        "UPDATE {PG_REFS_TABLE} SET holders = holders - 1 WHERE data_ref=$1 RETURNING holders"
                    ),
                    &[dr],
                )
                .map_err(|e| format!("postgres {e}"))?
                .into_iter()
                .next()
                .map(|r| r.get::<_, i64>(0));
            if !matches!(left, Some(h) if h > 0) {
                tx.execute(
                    &format!("DELETE FROM {PG_REFS_TABLE} WHERE data_ref=$1"),
                    &[dr],
                )
                .map_err(|e| format!("postgres {e}"))?;
            }
        }
        for dr in &blobs {
            tx.execute(
                &format!(
                    "INSERT INTO {PG_REFS_TABLE} (data_ref, holders) VALUES ($1, 1) \
                     ON CONFLICT (data_ref) DO UPDATE SET holders = {PG_REFS_TABLE}.holders + 1"
                ),
                &[dr],
            )
            .map_err(|e| format!("cannot book the imported tree's blobs: {e}"))?;
        }
        tx.commit()
            .map_err(|e| format!("cannot commit the imported tree: {e}"))?;
        Ok((nodes, blobs.len() as i64))
    })();
    let (nodes, blobs) = match built {
        Ok(v) => v,
        Err(e) => {
            if created {
                // A refused import must not leave the schema it provisioned: the
                // next mount would read it as a real (empty) view.
                let _ = c.batch_execute(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"));
            }
            return Err(e);
        }
    };
    Ok(crate::meta::ViewImportStats {
        nodes,
        blobs,
        kind,
        replaced: existed,
    })
}

/// SQLSTATE 42P01 (`undefined_table`) — the one "no ledger yet" case a recount
/// may create its way out of.
fn is_undefined_table(e: &postgres::Error) -> bool {
    e.as_db_error()
        .map(|d| d.code().code() == "42P01")
        .unwrap_or(false)
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
        PgEngine::create_node_with_attr(self, parent, name, is_dir, data_ref, mode, uid, gid)
    }
    fn create_symlink_with_attr(
        &self,
        parent: i64,
        name: &str,
        target: &str,
        uid: u32,
        gid: u32,
    ) -> Result<NodeInfo, MetaError> {
        PgEngine::create_symlink_with_attr(self, parent, name, target, uid, gid)
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

// ---- helpers shared by the tree-copy verbs --------------------------------

/// Does `schema` exist in `c`? The single existence question both copy verbs
/// ask, always of `information_schema` (never of a cached engine handle).
fn schema_exists(c: &mut Client, schema: &str) -> Result<bool, String> {
    Ok(!c
        .query(
            "SELECT 1 FROM information_schema.schemata WHERE schema_name = $1",
            &[&schema],
        )
        .map_err(|e| format!("postgres {e}"))?
        .is_empty())
}

/// Snapshot one tree's rows for a graft. One statement, so one consistent
/// point-in-time read even while the source view is mounted elsewhere.
fn read_graft_rows(c: &mut Client, schema: &str) -> Result<Vec<GraftPgRow>, String> {
    let rows = c
        .query(
            &format!("SELECT {COLS} FROM {schema}.node ORDER BY id"),
            &[],
        )
        .map_err(|e| format!("cannot read source tree '{schema}': {e}"))?
        .iter()
        .map(|r| GraftPgRow {
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
        })
        .collect();
    Ok(rows)
}

/// `parent id -> row indices`, exactly like the file arm's `graft_children`, so
/// a directory can be walked into its subtree without re-querying the source.
fn graft_children(rows: &[GraftPgRow]) -> HashMap<i64, Vec<usize>> {
    let mut m: HashMap<i64, Vec<usize>> = HashMap::new();
    for (i, r) in rows.iter().enumerate() {
        if let Some(p) = r.parent_id {
            m.entry(p).or_default().push(i);
        }
    }
    m
}

/// Resolve a plan path (`a/b/c`) against the source rows. `None` means the
/// path does not exist in that tree — the caller names the view in the error.
fn graft_resolve(
    rows: &[GraftPgRow],
    children: &HashMap<i64, Vec<usize>>,
    root: i64,
    path: &str,
) -> Option<usize> {
    let mut parent = root;
    let mut cur: Option<usize> = None;
    for comp in path.split('/') {
        let idx = *children.get(&parent)?.iter().find(|&&k| rows[k].name == comp)?;
        parent = rows[idx].id;
        cur = Some(idx);
    }
    cur
}

/// Render the internal write marker as the operator-facing text the file arm's
/// `rusqlite` errors already produce ("serialization failure" / the mapped
/// POSIX-visible message).
fn write_err_str(e: WriteErr) -> String {
    match e {
        WriteErr::Serialization => "postgres serialization failure (retry)".to_string(),
        WriteErr::Hard(me) => me.to_string(),
    }
}

#[cfg(test)]
mod shard_tests {
    use super::{control_store, route_multi, route_store, store_instances};

    /// Same spec + same view must always pick the same instance (that equality
    /// is what lets `view create`, every meta-server and cross-view ops agree
    /// without a registry), and a realistic view set must actually spread.
    #[test]
    fn route_store_is_stable_and_spreads_views() {
        let spec = "postgres://a@h/db1, postgres://a@h/db2,postgres://a@h/db3";
        for v in ["alice", "bob", "carol", "dave", "erin"] {
            assert_eq!(route_store(spec, v), route_store(spec, v));
            assert!(route_store(spec, v).starts_with("postgres://a@h/db"));
        }
        let mut happy = 0;
        for i in 0..64 {
            let v = format!("team{i}");
            if route_store(spec, &v) == "postgres://a@h/db1" {
                happy += 1;
            }
        }
        assert!(happy > 8 && happy < 40, "lopsided spread: {happy}/64 on db1");
    }

    /// A single URI must come back verbatim — the unimplemented (one-instance)
    /// deployment keeps byte-identical behaviour, whitespace included is
    /// trimmed so `--meta-store " uri "` still connects.
    #[test]
    fn route_store_passes_a_single_uri_through() {
        assert_eq!(route_store("postgres://a@h/db", "alice"), "postgres://a@h/db");
        assert_eq!(route_store(" postgres://a@h/db ", "alice"), "postgres://a@h/db");
        assert_eq!(route_store("", "alice"), "");
    }

    /// The control plane is pinned to the first instance of a sharded spec.
    #[test]
    fn control_store_pins_the_first_instance() {
        assert_eq!(control_store("postgres://a@h/db1,postgres://a@h/db2"), "postgres://a@h/db1");
        assert_eq!(control_store("postgres://a@h/db1"), "postgres://a@h/db1");
    }

    /// A fork/graft pair may only run when both names hash to the SAME
    /// instance (one connection cannot span two databases); the error names the
    /// split instead of connecting to the wrong server.
    #[test]
    fn route_multi_requires_one_instance_per_pair() {
        let spec = "postgres://a@h/db1,postgres://a@h/db2";
        // Same view twice is trivially one instance.
        assert_eq!(route_multi(spec, &["x", "x"]).unwrap(), route_store(spec, "x"));
        // Find a pair that the hash actually splits, then require a refusal.
        let mut split = None;
        for i in 0..64 {
            for j in 0..64 {
                let (a, b) = (format!("v{i}"), format!("v{j}"));
                if route_store(spec, &a) != route_store(spec, &b) {
                    split = Some((a, b));
                    break;
                }
            }
            if split.is_some() {
                break;
            }
        }
        let (a, b) = split.expect("64 names must not all hash to one instance");
        let err = route_multi(spec, &[&a, &b]).unwrap_err();
        assert!(err.contains("different PostgreSQL instances"), "{err}");
        // ... and a same-instance pair is accepted and yields that URI.
        let mut same = None;
        for i in 0..64 {
            if route_store(spec, &format!("v{i}")) == route_store(spec, &a) {
                same = Some(format!("v{i}"));
                break;
            }
        }
        let s = same.expect("some name must share an instance with itself");
        assert_eq!(route_multi(spec, &[&a, &s]).unwrap(), route_store(spec, &a));
    }

    /// Instances are split on commas and trimmed; blanks are never an instance.
    #[test]
    fn store_instances_skips_blanks() {
        assert_eq!(store_instances("postgres://a/d1, ,postgres://a/d2 "), vec!["postgres://a/d1", "postgres://a/d2"]);
        assert_eq!(store_instances(" postgres://a/d1 "), vec!["postgres://a/d1"]);
        assert!(store_instances("").is_empty());
    }

    /// A brand-new node's `nlink`/`size` are deterministic, and `fresh_info`
    /// must return exactly what `finalize` would after a create — without any
    /// store read. This is the correctness contract behind the single-round-trip
    /// `create_node_with_attr` override (a fresh inode has one name and no
    /// children, so a `COUNT` would yield the same). Scalar fields pass through
    /// straight from the row the INSERT wrote.
    #[test]
    fn fresh_info_matches_finalize_for_new_nodes() {
        use super::{PgEngine, RawNode};
        use crate::meta::NodeKind;
        let base = |is_dir: i64, data_ref: Option<&str>, link: Option<&str>| RawNode {
            id: 7,
            parent_id: Some(3),
            name: "n".into(),
            is_dir,
            data_ref: data_ref.map(str::to_string),
            link_target: link.map(str::to_string),
            mode: 0o640,
            uid: 12,
            gid: 34,
            mtime: 99,
            atime: 99,
            ctime: 99,
            ino: 7,
        };
        let reg = PgEngine::fresh_info(&base(0, None, None));
        assert_eq!(reg.kind, NodeKind::Regular);
        assert_eq!((reg.nlink, reg.size), (1, 0));
        // A fresh file already naming a shared blob still reports size 0, and
        // carries the data_ref through — parity with what getattr returns.
        let reg2 = PgEngine::fresh_info(&base(0, Some("k"), None));
        assert_eq!((reg2.nlink, reg2.size), (1, 0));
        assert_eq!(reg2.data_ref.as_deref(), Some("k"));
        let dir = PgEngine::fresh_info(&base(1, None, None));
        assert_eq!(dir.kind, NodeKind::Dir);
        assert_eq!((dir.nlink, dir.size), (2, 4096));
        let sym = PgEngine::fresh_info(&base(0, None, Some("target")));
        assert_eq!(sym.kind, NodeKind::Symlink);
        assert_eq!((sym.nlink, sym.size), (1, 6));
        // scalar fields straight from the written row
        assert_eq!(
            (reg.id, reg.parent_id, reg.name.as_str(), reg.mode, reg.uid, reg.gid, reg.mtime, reg.ctime),
            (7, 3, "n", 0o640, 12, 34, 99, 99),
        );
    }

    /// `finalize_counts` is the single kind -> nlink/size mapping that both the
    /// folded `getattr`/`lookup` (counts arrive as scalar subqueries on the row)
    /// and `finalize_batch` (counts from a grouped `COUNT ... GROUP BY`) feed.
    /// It must apply ONLY the count the kind needs — the other count is computed
    /// by the shared SQL shape but discarded — and reproduce the pre-fold scalar
    /// `finalize` byte for byte. Pure, so no store is needed.
    #[test]
    fn finalize_counts_applies_only_the_kinds_count() {
        use super::{PgEngine, RawNode};
        use crate::meta::NodeKind;
        let base = |is_dir: i64, link: Option<&str>| RawNode {
            id: 7,
            parent_id: Some(3),
            name: "n".into(),
            is_dir,
            data_ref: None,
            link_target: link.map(str::to_string),
            mode: 0o640,
            uid: 12,
            gid: 34,
            mtime: 99,
            atime: 99,
            ctime: 99,
            ino: 7,
        };
        // Directory: nlink is 2 + child-dir count; the holder count (garbage
        // here) must NOT leak in. size is the fixed 4096.
        let dir = PgEngine::finalize_counts(&base(1, None), 3, 999);
        assert_eq!(dir.kind, NodeKind::Dir);
        assert_eq!((dir.nlink, dir.size), (5, 4096));

        // Regular file: nlink is the hard-link (holder) count; the subdir count
        // (garbage here) must NOT leak in. size is 0.
        let file = PgEngine::finalize_counts(&base(0, None), 999, 4);
        assert_eq!(file.kind, NodeKind::Regular);
        assert_eq!((file.nlink, file.size), (4, 0));
        // The `.max(1)` guard: a holder count of 0 (never real, but the fold
        // could see it mid-unlink) still reports nlink 1, matching `finalize`.
        assert_eq!(PgEngine::finalize_counts(&base(0, None), 0, 0).nlink, 1);

        // Symlink: nlink 1, size = target length; BOTH counts are ignored.
        let sym = PgEngine::finalize_counts(&base(0, Some("/a/b")), 999, 999);
        assert_eq!(sym.kind, NodeKind::Symlink);
        assert_eq!((sym.nlink, sym.size), (1, 4));

        // A fresh node (child-dir count 0, holder count 1) must match what
        // `fresh_info` claims for the same row — the two entry points into the
        // same mapping agree, so create and a later getattr never disagree.
        let fresh_dir = PgEngine::finalize_counts(&base(1, None), 0, 1);
        assert_eq!((fresh_dir.nlink, fresh_dir.size), (2, 4096));
        assert_eq!(
            format!("{fresh_dir:?}"),
            format!("{:?}", PgEngine::fresh_info(&base(1, None)))
        );
        // non-derived columns pass straight through from the row
        assert_eq!((dir.id, dir.parent_id, dir.mode, dir.uid, dir.gid, dir.mtime), (7, 3, 0o640, 12, 34, 99));
    }
}

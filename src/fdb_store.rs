//! FoundationDB metadata backend — a third authoritative store beside the
//! embedded SQLite [`crate::meta::MetaEngine`] and [`crate::pg_store::PgEngine`].
//!
//! Compiled only with `--features fdb` (see `Cargo.toml`), because it links
//! `libfdb_c`. Without the feature the `MetaStore::Fdb` variant does not exist at
//! all, so no build that has not opted in can reach this code or need the client
//! library.
//!
//! **This module is the P2+P3 layer.** The encoding contract, the cluster
//! opening, the OCC retry driver, the epoch surface and the id/ino block
//! allocator are real and tested here, and so is the tree body: root seeding,
//! `lookup`/`getattr`/`readdir`/`readdir_page` with §3.3's stride-32 cursors,
//! `create_node`/`create_symlink`/`unlink` with their secondary-key and ledger
//! sets, the inode-targeting `set_mode`/`set_uid_gid`/`set_times`/
//! `touch_mtime`, and the two operations the design doc calls the hardest shapes:
//! `link_node` (§3.4/§3.5/§4) and `rename` (§3.9, one transaction end to end
//! including the cycle check), and the whole lock surface (P4): `lock_op`'s four
//! steps in one transaction, `get_lock` on a read that joins no conflict set,
//! and the `'lo`/`'ls`-riding implicit release and renewal paths (§3.7).
//! `docs/fdb-keyspace.md` — not this file — is the specification they were
//! written against, so each section number below points at the rule being
//! enforced rather than at work left to do.
//!
//! ## Where this file departs from the design doc
//!
//! Five departures from the first draft of the design doc, each recorded as a
//! version note in `docs/fdb-keyspace.md` rather than left implicit:
//!
//! * **C1** `ino` is not a second issued counter. `create_node` sets `ino = id`,
//!   because PG's `insert` does exactly that (`pg_store.rs:851-877`) and a
//!   differential harness that compared inode numbers would otherwise report
//!   every tree as different. `(v,'m','ino')` is therefore the migration
//!   high-water mark, raised from issued ids — §4's "two independent counters"
//!   becomes "one counter plus a watermark".
//! * **C2** the `'h` holder key carries `parent_id` before `name`. §3.4's
//!   existence-key property (empty value, nlink by prefix count) is preserved;
//!   what the extra component buys is §3.2's pointer repair in `unlink` as one
//!   decode instead of a second scan.
//! * §3.2's "two point reads issued in parallel, one round trip" for `getattr`
//!   is not achievable as written: the second read's KEY is the first read's
//!   VALUE, so no batching can overlap them. [`FdbStore::getattr`] issues the two
//!   reads inside one transaction and accepts the dependency; [`Tx::get_multi`]
//!   is kept as the primitive for reads that genuinely are independent, which is
//!   what a range-less multi-get needs.
//!
//! Two more landed with the later milestones, same rule — recorded as C4/C5 in
//! the design doc's version notes, not kept implicit here:
//!
//! * **C4** (P3) batched attribute finalisation (`info`, called per entry by
//!   [`FdbStore::readdir`] and [`FdbStore::readdir_page`] — the counterpart of
//!   `pg_store::finalize`) is M prefix probes inside one transaction, not one
//!   merged scan: FDB has no "seek several prefixes at once" primitive, so the
//!   READDIRPLUS cost grows with the number of distinct parents in a batch.
//! * **C5** (P4) the lock secondary keys carry the FULL primary key, not the
//!   `(owner, ino, l_start)` / `(sid, ino, l_start)` the first draft of §3.7
//!   sketched. A secondary key that cannot be decoded back to exactly one
//!   primary key is not an index — it is a scan with extra write amplification,
//!   and here a collapsed key would delete someone else's bookkeeping on an
//!   implicit release. Two shapes collapse on the draft form: compatible read
//!   locks from different owners at the same `l_start`, and one owner holding
//!   `[0,50)` next to `[0,100)`. [`Keys::lock_owner`], [`Keys::lock_sid`] and
//!   [`Keys::lock_expire`] are pinned against that by
//!   `tests::lock_keys_reconstruct_their_primary_exactly`.
//!
//! ## What is already real, and why
//!
//! * **Key encoding** ([`Keys`]): every form below is byte-frozen the moment it
//!   is written to a cluster, so it is settled here and now (decisions D1/D2),
//!   not deferred to P3. Integers go through the tuple layer's sign-preserving
//!   ordering — never a hand-rolled big-endian fixed width — because `l_start`
//!   and computed `ino` differences can be negative and a raw encoding would sort
//!   them on the wrong side of zero (§9 of the design doc). The exact bytes are
//!   pinned by `tests::key_encoding_is_byte_frozen`, so a refactor cannot quietly
//!   change the on-cluster format.
//! * **Isolation**: FDB's default is strict serialisability, so the "Why
//!   SERIALIZABLE" argument in [`crate::pg_store`] transfers unchanged — the
//!   governance invariants I1–I4 follow from the schedule being serialisable, and
//!   the mechanism we rely on is optimistic concurrency detection at commit
//!   instead of PostgreSQL's SSI predicate tracking. Consequence worth stating
//!   plainly: where PG pays for read predicates, FDB pays on *commit* for
//!   overlapping write sets, so a hot rename pair aborts one side.
//! * **Retry budget** ([`SERIALIZATION_MAX_ATTEMPTS`] etc.): deliberately the same
//!   numbers as the PG backend (§5 of the design doc), so a benchmark comparing
//!   backends does not also compare tuning histories. We do NOT use
//!   `Transaction::on_error`, which applies the C client's own unbounded retry and
//!   backoff policy: that would silently replace the frozen budget with a
//!   different one and make the two backends incomparable. For the same reason
//!   `RetryLimit`/`MaxRetryDelay` are never set — without `on_error` they do
//!   nothing. What every data-path transaction DOES carry is
//!   `TransactionOption::Timeout` (see [`DATA_TX_TIMEOUT_MS`], applied by
//!   [`data_trx`]): that is a liveness ceil, not a retry policy — `block_on` has
//!   no deadline of its own, so an unreachable cluster would otherwise park a
//!   fuser worker forever instead of surfacing an error.
//! * **No connection pools.** There is no checkout/requeue/wait stage here on
//!   purpose — porting [`crate::pg_store`]`'s pool logic would invent a resource
//!   the backend does not have. One `Database` handle per cluster file is shared
//!   by every view, which is exactly why `--engine-idle-secs` degrades to a plain
//!   in-memory handle cache under this backend.
//!
//! ## Synchronicity
//!
//! The crate's metadata surface is synchronous (`MetaProvider` is driven straight
//! from a fuser worker), and the `foundationdb` bindings are async-only. Rather
//! than put an async runtime in the library, every wait goes through
//! `futures::executor::block_on`: an FDB future is just a completion callback from
//! the client's own network thread, so parking the calling thread on a waker is
//! all that is needed and no reactor is involved. Where the design doc promises
//! several reads "issued in parallel, one round trip" (§3.2 getattr),
//! [`Tx::get_multi`] joins the futures and blocks once, preserving that property
//! instead of degrading it into N sequential waits.

use crate::meta::{LockConflict, LockType, MetaError, MetaProvider, NodeInfo, NodeKind};
use foundationdb::api::{FdbApiBuilder, NetworkAutoStop};
use foundationdb::tuple::{Element, Subspace, TuplePack, TupleUnpack};
use foundationdb::{Database, FdbError, KeySelector, RangeOption, Transaction};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;

/// Number of ids handed out per reservation. A single-key auto-increment per
/// `create_node` would serialise every writer in the view on one FDB key — the
/// exact hotspot the design doc's §4 allocator exists to avoid — so ids are
/// claimed in blocks and only the block boundary is contended.
pub const ID_BLOCK: i64 = 1_024;

/// The `(next, limit)` pair a reservation of `block` ids ending at `hi` yields.
///
/// The allocator key holds the **highest issued** id, so a `+block` bump claims the
/// half-open range `(hi - block, hi]` and the first usable id is `hi - block + 1`.
/// Stated as a function because the off-by-one in it is the difference between a
/// tree and a corrupted one: an id of 1 is the root's, and handing it out would
/// overwrite the root's `'d'`/`'i'` keys rather than collide with a new row.
fn block_bounds(hi: i64, block: i64) -> (i64, i64) {
    ((hi - block + 1).max(1), hi)
}

/// Retry bound per write op, mirroring `pg_store::SERIALIZATION_MAX_ATTEMPTS`.
const SERIALIZATION_MAX_ATTEMPTS: u32 = 12;
const SERIALIZATION_BACKOFF_MIN: std::time::Duration = std::time::Duration::from_micros(200);
const SERIALIZATION_BACKOFF_MAX: std::time::Duration = std::time::Duration::from_millis(25);

// ---- FDB error codes we classify (fdb_c.h, API 710) -------------------------
//
// Named per code on purpose: the mapping to POSIX differs, and lumping them into
// one "retry" bucket would hide a `transaction_timed_out` (our op is too slow)
// inside a `not_committed` (someone else wrote the same key), which are different
// operational problems. See §6 of the design doc.
const CODE_NOT_COMMITTED: i32 = 1020;
const CODE_TRANSACTION_TOO_OLD: i32 = 1021;
const CODE_TRANSACTION_TIMED_OUT: i32 = 1031;
const CODE_READY_WRITE_DISABLED: i32 = 1032;
const CODE_COMMIT_ON_FIRST_PROXY: i32 = 1034;
const CODE_PROXY_METADATA_VERSION_MISMATCH: i32 = 1037;

/// Codes that mean "re-run the body on a reset transaction". Everything else must
/// surface — an infinite retry loop on `key_outside_legal_range` (2000) would hang
/// the mount instead of returning EIO.
fn is_retriable(code: i32) -> bool {
    matches!(
        code,
        CODE_NOT_COMMITTED
            | CODE_TRANSACTION_TOO_OLD
            | CODE_TRANSACTION_TIMED_OUT
            | CODE_READY_WRITE_DISABLED
            | CODE_COMMIT_ON_FIRST_PROXY
            | CODE_PROXY_METADATA_VERSION_MISMATCH
    )
}

/// Map an FDB error onto the store-error vocabulary the trait surface uses. The PG
/// backend does the same job for SQLSTATE via `map_db_err`; keeping the two tables
/// side by side is what makes "semantically equivalent" checkable rather than
/// aspirational (§6: "冲突码绝不外泄给 POSIX 调用方" is enforced by the driver,
/// and only a *non*-retriable code can reach this function).
pub(crate) fn map_fdb_error(err: &FdbError) -> MetaError {
    MetaError::Db(format!("fdb error {}: {err}", err.code()))
}

// ---- client library / network lifecycle ------------------------------------

/// The FDB C API may be selected once per process and its network thread run once
/// per process; `FdbApiBuilder::build` panics on a second call. `OnceLock` is what
/// makes our use of it correct from arbitrary threads (FUSE workers, meta-server
/// dispatch) without exposing a global init order requirement to callers.
static NETWORK: OnceLock<Result<(), String>> = OnceLock::new();

/// Per-cluster-file `Database` handles, shared as `Arc` because `Database` is not
/// `Clone` (it owns a refcounted C handle and destroys it on drop, so the bindings
/// hand out one owner and everyone else must share through an `Arc`).
/// `fdb_create_database` is itself refcounted inside the C library, so two views on
/// the same cluster share one handle and a view pointing at a different cluster
/// gets its own — no need to invent a registry, and no need to force a single
/// cluster file on a process that legitimately serves several.
static DATABASES: OnceLock<DbMap> = OnceLock::new();

/// The one handle that can close the network thread, kept where
/// [`shutdown_network`] can reach it (see [`ensure_network`] for why it is a
/// static and why a daemon must not run it before it is really done).
static NETWORK_GUARD: OnceLock<std::sync::Mutex<Option<NetworkAutoStop>>> = OnceLock::new();

type DbHandle = std::sync::Arc<Database>;
type DbMap = std::sync::Mutex<std::collections::HashMap<String, DbHandle>>;

/// Start the FDB network exactly once, and hand the stop guard to
/// [`shutdown_network`] so a short-lived process can close the network the way
/// `libfdb_c` requires.
///
/// The guard is kept in a static rather than dropped at the end of this
/// function, because a `meta-server` has no shutdown point at which every view
/// is quiesced (the store is reachable from live FUSE workers until the process
/// is gone), and `NetworkAutoStop`'s `Drop` signals the event loop, joins its
/// thread and `process::abort()`s if that fails — running it there would convert
/// an ordinary daemon exit into an abort. What must NOT happen is leaving the
/// guard nowhere to be reached: the bindings document the boot return value as
/// "a `NetworkAutoStop` handle which must be dropped before the program exit",
/// and a CLI command that exits while the network thread is still tearing down
/// its own state races exactly that contract (observed as an intermittent
/// SIGSEGV, rc=139 instead of the refusal's rc=1, on the fastest error paths —
/// a refusal that returns before the first real round trip).
///
/// Returning a `Result` instead of panicking matters for the failure mode
/// operators actually hit: an `libfdb_c` older than the bindings' API level
/// fails with 2203 `api_version_not_supported`, and that must be an io error at
/// open time.
fn ensure_network() -> Result<(), std::io::Error> {
    let state = NETWORK.get_or_init(|| {
        let builder = FdbApiBuilder::default()
            .build()
            .map_err(|e| format!("fdb api version select: {e}"))?;
        // SAFETY: the contract is that `stop()` runs before exit; see above for
        // why this daemon leaks the guard instead, and why `OnceLock` keeps
        // `boot`'s "once per process" precondition.
        match unsafe { builder.boot() } {
            Ok(guard) => {
                // Keep the guard reachable: this is the one handle that can
                // close the network, and a process that is exiting needs it.
                *guard_slot().lock().unwrap_or_else(|p| p.into_inner()) = Some(guard);
                Ok(())
            }
            Err(e) => Err(format!("fdb network start: {e}")),
        }
    });
    state
        .as_ref()
        .map(|_| ())
        .map_err(|s| std::io::Error::other(s.clone()))
}

fn guard_slot() -> &'static std::sync::Mutex<Option<NetworkAutoStop>> {
    NETWORK_GUARD.get_or_init(|| std::sync::Mutex::new(None))
}

/// Close the FDB network before a short-lived process exits: signal the event
/// loop, then join its thread, so nothing of `libfdb_c`'s own teardown is still
/// in flight when `exit()` runs the process-wide destructors.
///
/// Idempotent, and a no-op when the network was never booted (every non-FDB
/// command, and an `fdb://` typo that failed before reaching
/// [`ensure_network`]). Called from `main`, which is the only place that knows a
/// command is over: a `meta-server` that is merely closing one connection must
/// not reach here, and does not, because its serve loop has not returned.
///
/// `NetworkAutoStop`'s `Drop` aborts if signalling the loop fails — the bindings
/// treat continuing after a refused `fdb_stop_network()` as undefined behavior,
/// and that is the right call for a process that has nowhere else to go.
pub fn shutdown_network() {
    // The lease is released BEFORE the network is closed: dropping the guard
    // first would make this the one call that cannot reach the cluster.
    release_held_writer_lease();
    let taken = guard_slot()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .take();
    if let Some(guard) = taken {
        drop(guard);
    }
}

fn database(cluster_file: &str) -> Result<DbHandle, std::io::Error> {
    ensure_network()?;
    let table = DATABASES.get_or_init(|| DbMap::new(std::collections::HashMap::new()));
    let mut map = table.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(db) = map.get(cluster_file) {
        return Ok(db.clone());
    }
    let db: DbHandle = std::sync::Arc::new(
        Database::new(Some(cluster_file))
            .map_err(|e| std::io::Error::other(format!("fdb open {cluster_file}: {e}")))?,
    );
    map.insert(cluster_file.to_string(), db.clone());
    Ok(db)
}

// ---- cluster gate and writer lease ------------------------------------------
//
// Two things a `fdb://` meta-server could not promise before this section:
//
// 1. It could start against an unreachable or UNCONFIGURED cluster and print its
//    banners as if all was well; the first mount then blocked for ~150s with no
//    output at all (measured). The gate below reads the cluster's own status at
//    startup, refuses when the database is not available, and prints the
//    topology it found — which is also how an operator can tell a single-node
//    test cluster from a replicated one in the server's own log.
// 2. "One meta-server per deployment" was a COMMENT, not an enforced rule: two
//    servers sharing a `--meta-dir` would both write the same subspaces with no
//    fencing between them (governance is cluster-wide too now, so the risk is
//    two write authorities over one tree, not divergent definitions). It is now
//    a lease row in the cluster: the second server waits up to the TTL for the
//    first one's row to expire and then refuses, a crashed holder's row expires
//    so a restart proceeds after at most one TTL, and `grovefs view lease` shows
//    and clears stale rows.
//
// The unit is the DEPLOYMENT (the canonical `--meta-dir`), not the cluster:
// the suite this backend is tested with runs several meta-servers against one
// cluster on purpose (one per `--home`, each serving its own views), so a
// cluster-wide lock would forbid a shape the backend already supports. What the
// lease forbids is the one shape that cannot be correct: two servers behind the
// SAME control plane.

/// Environment knob: writer-lease TTL in seconds (`0` disables the lease).
pub const WRITER_LEASE_ENV: &str = "GROVEFS_FDB_WRITER_LEASE_SECS";
/// Environment knob: how long a starting server waits for a held lease.
pub const WRITER_WAIT_ENV: &str = "GROVEFS_FDB_WRITER_WAIT_SECS";
/// Environment knob: timeout of the startup cluster-status probe, in seconds.
pub const OPEN_TIMEOUT_ENV: &str = "GROVEFS_FDB_OPEN_TIMEOUT_SECS";

const WRITER_LEASE_DEFAULT_SECS: u64 = 10;
const OPEN_TIMEOUT_DEFAULT_SECS: u64 = 10;
const LEASE_TX_TIMEOUT_MS: i32 = 5_000;

/// Timeout on every data-path transaction (namespace ops, governance probes,
/// scans). Without it a transaction whose commit cannot complete waits forever:
/// `block_on` parks a fuser worker thread and nothing above it has a deadline,
/// so an unreachable cluster turns into a hung mount instead of an error.
///
/// Deliberately generous: this is a liveness ceil, not a latency budget. An
/// `import` may commit close to the 10 MB transaction limit, and a loaded
/// three-replica cluster can take seconds to acknowledge that — 30s clears the
/// slow case while still bounding a wedged one. Retry accounting stays ours
/// (see the module docs on why `on_error`/`RetryLimit` are not used).
const DATA_TX_TIMEOUT_MS: i32 = 30_000;

/// One data-path transaction with the standard options applied. A failure to
/// carry the timeout is tolerated (the op still runs, just unbounded), because
/// turning a tuning knob into an outage would be worse than the missing ceil.
fn data_trx(db: &Database) -> Result<Transaction, FdbError> {
    let trx = db.create_trx()?;
    let _ = trx.set_option(foundationdb::options::TransactionOption::Timeout(
        DATA_TX_TIMEOUT_MS,
    ));
    Ok(trx)
}

fn env_secs(name: &str) -> Option<u64> {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
}

/// TTL of the writer lease, or `None` when it is disabled (`…=0`).
pub fn writer_lease_ttl() -> Option<std::time::Duration> {
    match env_secs(WRITER_LEASE_ENV) {
        Some(0) => None,
        Some(s) => Some(std::time::Duration::from_secs(s)),
        None => Some(std::time::Duration::from_secs(WRITER_LEASE_DEFAULT_SECS)),
    }
}

/// How long a starting server waits for a lease another (possibly dead) server
/// still holds. Default: one TTL plus two seconds — long enough for a crashed
/// holder's row to expire, short enough that a live holder is reported quickly.
pub fn writer_lease_wait(ttl: std::time::Duration) -> std::time::Duration {
    match env_secs(WRITER_WAIT_ENV) {
        Some(s) => std::time::Duration::from_secs(s),
        None => ttl + std::time::Duration::from_secs(2),
    }
}

/// Timeout of the startup cluster-status probe.
pub fn cluster_open_timeout() -> std::time::Duration {
    std::time::Duration::from_secs(
        env_secs(OPEN_TIMEOUT_ENV).unwrap_or(OPEN_TIMEOUT_DEFAULT_SECS),
    )
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// One writer-lease row: who holds a deployment's write authority, and until when.
///
/// The value is `(owner, expires_at_ms)` and the owner string carries
/// `host:pid:boot-nonce`, so an operator reading `view lease` can tell WHICH
/// process to stop without asking the cluster for more.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriterLease {
    /// Canonical `--meta-dir` this row governs.
    pub deployment: String,
    /// `host:pid:nonce` of the holding process.
    pub owner: String,
    /// Wall-clock expiry (epoch ms). Renewed every TTL/3 while the holder lives.
    pub expires_at_ms: i64,
}

impl WriterLease {
    fn pack(&self) -> Vec<u8> {
        (&self.owner, self.expires_at_ms).pack_to_vec()
    }

    fn unpack(deployment: &str, bytes: &[u8]) -> Option<Self> {
        let items = Vec::<Element>::unpack_root(bytes).ok()?;
        Some(Self {
            deployment: deployment.to_string(),
            owner: items.first()?.as_str()?.to_string(),
            expires_at_ms: items.get(1)?.as_i64()?,
        })
    }

    /// Milliseconds left before the row stops counting; negative once expired.
    pub fn remaining_ms(&self) -> i64 {
        self.expires_at_ms - now_ms()
    }

    pub fn is_live(&self) -> bool {
        self.expires_at_ms > now_ms()
    }
}

/// Identity of a DEPLOYMENT in a lease row: the canonical `--meta-dir`. The
/// server and `view lease` must compute the same string or `--release` would
/// address a row that does not exist, so both go through this function (a path
/// that cannot be canonicalized — already deleted — is used verbatim).
pub fn deployment_id(meta_dir: &Path) -> String {
    std::fs::canonicalize(meta_dir)
        .unwrap_or_else(|_| meta_dir.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// Identity of THIS process inside a lease row: `host:pid:nonce`. The nonce
/// separates two lifetimes of the same pid (a restart on the same port), which is
/// what makes "is the row mine" answerable after a crash.
pub fn writer_owner_id() -> String {
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "unknown".to_string());
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{host}:{}:{nonce:x}", std::process::id())
}

fn lease_keys() -> Keys {
    // The lease key space is a ROOT sibling, so any `Keys` value addresses it;
    // this name is never written (same trick `rebuild_ledger` uses).
    root_keys()
}

/// Run one small control-plane transaction with the same bounded retry policy the
/// view writes use: a conflict re-runs the BODY (which re-reads the row), so a
/// race is resolved by the read, never by a blind overwrite. Used by the
/// writer-lease rows and by the credential/audit tables ([`CtlKeyTable`]) — all
/// of them are single-row (or single-scan) control-plane operations, not tree
/// mutations.
fn with_kv_tx<T, F>(
    db: &DbHandle,
    mut body: F,
) -> Result<T, std::io::Error>
where
    F: FnMut(&Transaction) -> Result<T, TxErr>,
{
    let mut delay = SERIALIZATION_BACKOFF_MIN;
    for _ in 0..SERIALIZATION_MAX_ATTEMPTS {
        let trx = db
            .create_trx()
            .map_err(|e| std::io::Error::other(TxErr::from(e).hard_or_exhausted().to_string()))?;
        trx.set_option(foundationdb::options::TransactionOption::Timeout(
            LEASE_TX_TIMEOUT_MS,
        ))
        .map_err(|e| std::io::Error::other(TxErr::from(e).hard_or_exhausted().to_string()))?;
        let out: Result<T, TxErr> = (|| {
            let v = body(&trx)?;
            match futures::executor::block_on(trx.commit()) {
                Ok(_) => Ok(v),
                Err(e) => Err(TxErr::from(FdbError::from(e))),
            }
        })();
        match out {
            Ok(v) => return Ok(v),
            Err(TxErr::Hard(me)) => return Err(std::io::Error::other(me.to_string())),
            Err(TxErr::Retry(_)) => {
                std::thread::sleep(delay);
                delay = (delay * 2).min(SERIALIZATION_BACKOFF_MAX);
            }
        }
    }
    Err(std::io::Error::other(
        "writer lease: serialization retry exhausted",
    ))
}

enum LeaseStep {
    Acquired,
    Held(WriterLease),
}

fn lease_attempt(
    db: &DbHandle,
    key: &[u8],
    deployment: &str,
    owner: &str,
    ttl: std::time::Duration,
) -> Result<LeaseStep, std::io::Error> {
    let now = now_ms();
    let expires = now + ttl.as_millis() as i64;
    with_kv_tx(db, |trx| {
        let cur = futures::executor::block_on(trx.get(key, false)).map_err(TxErr::from)?;
        if let Some(row) = cur.as_deref().and_then(|b| WriterLease::unpack(deployment, b)) {
            if row.expires_at_ms > now {
                return Ok(LeaseStep::Held(row));
            }
        }
        trx.set(
            key,
            &WriterLease {
                deployment: deployment.to_string(),
                owner: owner.to_string(),
                expires_at_ms: expires,
            }
            .pack(),
        );
        Ok(LeaseStep::Acquired)
    })
}

/// Take (or wait for) the deployment's writer lease.
///
/// `wait` bounds the wait, and it exists for the crash case: a SIGKILLed holder
/// leaves its row behind until the TTL passes, and a restart should take over
/// then rather than requiring an operator to clear a key by hand. When the holder
/// is ALIVE, renewal keeps the row live and this returns the error below once
/// `wait` elapses.
pub fn acquire_writer_lease(
    uri: &str,
    deployment: &str,
    owner: &str,
    ttl: std::time::Duration,
    wait: std::time::Duration,
) -> Result<WriterLease, std::io::Error> {
    let cluster_file = FdbConfig::parse_cluster(uri)?;
    let db = database(&cluster_file)?;
    let key = lease_keys().writer_lease(deployment);
    let deadline = std::time::Instant::now() + wait;
    loop {
        match lease_attempt(&db, &key, deployment, owner, ttl)? {
            LeaseStep::Acquired => {
                return Ok(WriterLease {
                    deployment: deployment.to_string(),
                    owner: owner.to_string(),
                    expires_at_ms: now_ms() + ttl.as_millis() as i64,
                })
            }
            LeaseStep::Held(row) => {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                if left.is_zero() {
                    return Err(std::io::Error::other(format!(
                        "deployment '{deployment}' already has a live meta-server writer \
                         (holder '{}', lease expires in {}s): one meta-server per --meta-dir \
                         may write this cluster — stop that server, or wait for its lease to \
                         expire ({}s, `{WRITER_LEASE_ENV}`), or clear a stale row with \
                         `grovefs view --meta-store {uri} lease --release --deployment {deployment} --force`",
                        row.owner,
                        row.remaining_ms() / 1000,
                        ttl.as_secs(),
                    )));
                }
                std::thread::sleep(left.min(std::time::Duration::from_millis(200)));
            }
        }
    }
}

/// Refresh a lease this process still owns. `Ok(false)` means the row is gone or
/// belongs to someone else — the caller must STOP serving, not keep writing.
pub fn renew_writer_lease(
    uri: &str,
    deployment: &str,
    owner: &str,
    ttl: std::time::Duration,
) -> Result<bool, std::io::Error> {
    let cluster_file = FdbConfig::parse_cluster(uri)?;
    let db = database(&cluster_file)?;
    let key = lease_keys().writer_lease(deployment);
    let expires = now_ms() + ttl.as_millis() as i64;
    with_kv_tx(&db, |trx| {
        let cur = futures::executor::block_on(trx.get(&key, false)).map_err(TxErr::from)?;
        match cur.as_deref().and_then(|b| WriterLease::unpack(deployment, b)) {
            Some(row) if row.owner == owner => {
                trx.set(
                    &key,
                    &WriterLease {
                        owner: owner.to_string(),
                        expires_at_ms: expires,
                        ..row
                    }
                    .pack(),
                );
                Ok(true)
            }
            _ => Ok(false),
        }
    })
}

/// Clear a lease row. `owner = Some(_)` releases only this process's own row;
/// `None` is the operator override `--force` (an EXPIRED row is always safe to
/// clear, a live one is a deliberate fence). `Ok(true)` = a row was removed.
pub fn release_writer_lease(
    uri: &str,
    deployment: &str,
    owner: Option<&str>,
) -> Result<bool, std::io::Error> {
    let cluster_file = FdbConfig::parse_cluster(uri)?;
    let db = database(&cluster_file)?;
    let key = lease_keys().writer_lease(deployment);
    with_kv_tx(&db, |trx| {
        let cur = futures::executor::block_on(trx.get(&key, false)).map_err(TxErr::from)?;
        let Some(row) = cur.as_deref().and_then(|b| WriterLease::unpack(deployment, b)) else {
            return Ok(false);
        };
        if owner.is_some_and(|o| o != row.owner) {
            return Ok(false);
        }
        trx.clear(&key);
        Ok(true)
    })
}

/// Every lease row on the cluster, for `grovefs view lease`.
pub fn list_writer_leases(uri: &str) -> Result<Vec<WriterLease>, std::io::Error> {
    let cluster_file = FdbConfig::parse_cluster(uri)?;
    let db = database(&cluster_file)?;
    let keys = lease_keys();
    let trx = db
        .create_trx()
        .map_err(|e| std::io::Error::other(TxErr::from(e).hard_or_exhausted().to_string()))?;
    let _ = trx.set_option(foundationdb::options::TransactionOption::Timeout(
        LEASE_TX_TIMEOUT_MS,
    ));
    // `iteration` starts at 1, not 0: `fdb_transaction_get_range` rejects 0 with
    // error 2000 (`invalid_api_call`) — measured, not assumed, when this first
    // ran as `get_range(.., 0, ..)` and `view lease` failed on an empty cluster.
    let rows = futures::executor::block_on(trx.get_range(&keys.writer_leases(), 1, true))
        .map_err(|e| std::io::Error::other(TxErr::from(e).hard_or_exhausted().to_string()))?;
    let mut out = Vec::new();
    for kv in rows.as_ref().iter() {
        if let Ok((_, deployment)) = keys.writer.unpack::<(String, String)>(kv.key()) {
            if let Some(row) = WriterLease::unpack(&deployment, kv.value()) {
                out.push(row);
            }
        }
    }
    out.sort_by(|a, b| a.deployment.cmp(&b.deployment));
    Ok(out)
}

// ---- control plane: credentials and the call audit ---------------------------
//
// The same two tables [`crate::apikeys`] keeps in one SQLite file or in the
// `grovefs_control` schema, here under `(root,'ctl',…)` — a root sibling for the
// reason `refs` and `writer` are siblings: a credential governs EVERY tree, so
// there is no view prefix it could belong to, and `view rm --base` of any tree
// must be unable to reach it.
//
// Two shape decisions are what keep this table comparable to the SQL ones rather
// than merely working:
//   * the row's KEY is the `id` (the primary key both SQL backends use), so
//     authentication is one point read and never a scan;
//   * insertion ORDER is a key, not a column — `(ctl,'order',seq)` → id and
//     `(ctl,'calls',seq)` → row — mirroring SQLite's implicit `rowid` and PG's
//     `seq IDENTITY`. `apikey list` then reports newest-first from a range read,
//     and `apikey calls` takes its `--limit` off the END of the range, which is
//     the cost `ORDER BY … DESC LIMIT $1` has on the SQL backends.
//
// Values are JSON rather than a packed tuple: nothing indexes a value component
// here, so tuple packing would buy an ordering nobody reads, while JSON keeps a
// row legible to an operator looking at a cluster in trouble with `fdbcli
// getrange` — which is the one moment this table has to be readable by a human
// who is not running grovefs.

/// One credential row: exactly the `api_keys` columns, so `apikey list` prints
/// the same table on all three backends.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CtlKeyRow {
    pub id: String,
    pub label: String,
    /// SHA-256 of the secret half. The plaintext is never stored (see
    /// [`crate::apikeys`]), so a dump of this subspace hands out nothing usable.
    pub secret_hash: String,
    pub created_at: String,
    pub last_used_at: Option<String>,
    pub revoked_at: Option<String>,
    pub scope: String,
    pub view: Option<String>,
    pub ro: bool,
    pub owner: Option<String>,
}

impl CtlKeyRow {
    fn encode(&self) -> Vec<u8> {
        json!({
            "id": &self.id,
            "label": &self.label,
            "secret_hash": &self.secret_hash,
            "created_at": &self.created_at,
            "last_used_at": &self.last_used_at,
            "revoked_at": &self.revoked_at,
            "scope": &self.scope,
            "view": &self.view,
            "ro": self.ro,
            "owner": &self.owner,
        })
        .to_string()
        .into_bytes()
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        let v: Value = serde_json::from_slice(bytes).ok()?;
        let s = |f: &str| v.get(f).and_then(Value::as_str).map(str::to_string);
        Some(Self {
            id: s("id")?,
            label: s("label")?,
            secret_hash: s("secret_hash")?,
            created_at: s("created_at")?,
            last_used_at: s("last_used_at"),
            revoked_at: s("revoked_at"),
            scope: s("scope")?,
            view: s("view"),
            ro: v.get("ro").and_then(Value::as_bool).unwrap_or(false),
            owner: s("owner"),
        })
    }
}

/// One call-audit row (`mcp_calls`), by the same column names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CtlCallRow {
    pub ts: String,
    pub key_id: Option<String>,
    pub key_label: Option<String>,
    pub method: String,
    pub tool: String,
    pub ok: bool,
    pub detail: String,
}

impl CtlCallRow {
    fn encode(&self) -> Vec<u8> {
        json!({
            "ts": &self.ts,
            "key_id": &self.key_id,
            "key_label": &self.key_label,
            "method": &self.method,
            "tool": &self.tool,
            "ok": self.ok,
            "detail": &self.detail,
        })
        .to_string()
        .into_bytes()
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        let v: Value = serde_json::from_slice(bytes).ok()?;
        let s = |f: &str| v.get(f).and_then(Value::as_str).map(str::to_string);
        Some(Self {
            ts: s("ts")?,
            key_id: s("key_id"),
            key_label: s("key_label"),
            method: s("method")?,
            tool: s("tool")?,
            ok: v.get("ok").and_then(Value::as_bool).unwrap_or(false),
            detail: s("detail").unwrap_or_default(),
        })
    }
}

/// Any `Keys` value addresses the ROOT siblings (`refs`, `writer`, `ctl`): the
/// view name packed into it is never written, which is the trick
/// [`rebuild_ledger`] uses for the ledger.
fn root_keys() -> Keys {
    Keys::for_view(REBUILD_PROBE_VIEW)
}

/// Claim the next value of one insertion counter INSIDE the caller's transaction,
/// so a row and its ordering entry commit together — a torn write would otherwise
/// leave the `order`/`calls` index pointing at a row that never landed.
fn ctl_next_seq(trx: &Transaction, key: &[u8]) -> Result<i64, TxErr> {
    let cur = futures::executor::block_on(trx.get(key, false))
        .map_err(TxErr::from)?
        .as_deref()
        .and_then(unpack_i64)
        .unwrap_or(0);
    let next = cur + 1;
    trx.set(key, &next.pack_to_vec());
    Ok(next)
}

/// Every key in `opt`, pages followed until the range is exhausted — the same
/// continuation [`Tx::scan`] runs, without that method's view-boundary assertion
/// (these keys live under the `ctl` sibling, outside every view by design).
fn ctl_scan_all(
    trx: &Transaction,
    opt: RangeOption<'static>,
) -> Result<Vec<(Vec<u8>, Vec<u8>)>, TxErr> {
    let mut out = Vec::new();
    let mut next = Some(opt);
    while let Some(mut page) = next {
        page.target_bytes = PAGE_BYTES;
        let kvs = futures::executor::block_on(trx.get_range(&page, 1, true)).map_err(TxErr::from)?;
        for kv in kvs.as_ref().iter() {
            out.push((kv.key().to_vec(), kv.value().to_vec()));
        }
        next = page.next_range(&kvs);
    }
    Ok(out)
}

/// The cluster-side credential/audit tables of one `fdb://` deployment.
///
/// Cheap to build and safe to share (`&self` everywhere): the `Database` handle
/// behind it is the per-cluster one every view already shares, so a meta-server
/// that opens a key store adds no connection.
pub struct CtlKeyTable {
    keys: Keys,
    db: DbHandle,
}

impl CtlKeyTable {
    /// Open the tables on the cluster named by `uri` (`fdb://<cluster-file>`).
    /// Nothing is created here: both tables are key spaces, and an empty range is
    /// a valid empty table — unlike PG, which needs its DDL run first.
    pub fn open(uri: &str) -> Result<Self, String> {
        let cluster_file = FdbConfig::parse_cluster(uri).map_err(|e| e.to_string())?;
        let db = database(&cluster_file).map_err(|e| e.to_string())?;
        Ok(Self {
            keys: root_keys(),
            db,
        })
    }

    /// One control-plane transaction, with the same bounded retry budget the view
    /// writes use (see [`with_kv_tx`]).
    fn run<T, F>(&self, body: F) -> Result<T, String>
    where
        F: FnMut(&Transaction) -> Result<T, TxErr>,
    {
        with_kv_tx(&self.db, body).map_err(|e| e.to_string())
    }

    /// Insert one credential row, claiming its position in the listing order in
    /// the same transaction.
    pub fn insert_key(&self, row: &CtlKeyRow) -> Result<(), String> {
        let val = row.encode();
        let row_key = self.keys.ctl_key(&row.id);
        let order_id = row.id.clone().into_bytes();
        self.run(move |trx| {
            let seq = ctl_next_seq(trx, &self.keys.ctl_seq(CTL_KEY_ROW))?;
            trx.set(&row_key, &val);
            trx.set(&self.keys.ctl_order(seq), &order_id);
            Ok(())
        })
    }

    /// One credential row by id — the whole authentication lookup.
    pub fn lookup(&self, id: &str) -> Result<Option<CtlKeyRow>, String> {
        let key = self.keys.ctl_key(id);
        self.run(|trx| {
            // A snapshot read: this transaction writes nothing, and an
            // authentication must not enter a conflict set a concurrent mint/revoke
            // would then have to settle.
            let cur = futures::executor::block_on(trx.get(&key, true)).map_err(TxErr::from)?;
            Ok(cur.as_deref().and_then(CtlKeyRow::decode))
        })
    }

    /// Read-modify-write one row. `f` returns false for "there is nothing this
    /// operation may change" (a revoked key, an unknown id), which is reported as
    /// `Ok(false)` rather than written back unchanged — the same verdict the SQL
    /// backends derive from `rows_affected == 0`.
    fn mutate<F>(&self, id: &str, mut f: F) -> Result<bool, String>
    where
        F: FnMut(&mut CtlKeyRow) -> bool,
    {
        let key = self.keys.ctl_key(id);
        self.run(move |trx| {
            let cur = futures::executor::block_on(trx.get(&key, false)).map_err(TxErr::from)?;
            let Some(mut row) = cur.as_deref().and_then(CtlKeyRow::decode) else {
                return Ok(false);
            };
            if !f(&mut row) {
                return Ok(false);
            }
            trx.set(&key, &row.encode());
            Ok(true)
        })
    }

    /// Stamp `last_used_at` after a successful authentication.
    pub fn stamp_used(&self, id: &str, ts: &str) -> Result<(), String> {
        let ts = ts.to_string();
        self.mutate(id, move |r| {
            r.last_used_at = Some(ts.clone());
            true
        })
        .map(|_| ())
    }

    /// Flip the read-only flag of an ACTIVE key (`apikey set-ro`), reporting
    /// whether an active row was there to flip.
    pub fn set_ro(&self, id: &str, ro: bool) -> Result<bool, String> {
        self.mutate(id, move |r| {
            if r.revoked_at.is_some() {
                return false;
            }
            r.ro = ro;
            true
        })
    }

    /// Revoke an ACTIVE key, reporting whether this call did it (a second revoke
    /// is `Ok(false)` and leaves the first timestamp alone).
    pub fn revoke(&self, id: &str, ts: &str) -> Result<bool, String> {
        let ts = ts.to_string();
        self.mutate(id, move |r| {
            if r.revoked_at.is_some() {
                return false;
            }
            r.revoked_at = Some(ts.clone());
            true
        })
    }

    /// Every key, in the order `apikey list` prints them: `created_at` descending
    /// with insertion order as the tiebreak — the ORDER BY both SQL backends
    /// spell out, kept identical rather than "close enough".
    pub fn list(&self) -> Result<Vec<CtlKeyRow>, String> {
        self.run(|trx| {
            let mut rows: Vec<(i64, CtlKeyRow)> = Vec::new();
            for (okey, oval) in ctl_scan_all(trx, self.keys.ctl_orders())? {
                let Ok((_, seq)) = self.keys.ctl.unpack::<(String, i64)>(&okey) else {
                    continue;
                };
                let id = String::from_utf8_lossy(&oval).into_owned();
                let cur = futures::executor::block_on(trx.get(&self.keys.ctl_key(&id), true))
                    .map_err(TxErr::from)?;
                if let Some(row) = cur.as_deref().and_then(CtlKeyRow::decode) {
                    rows.push((seq, row));
                }
            }
            rows.sort_by(|a, b| b.1.created_at.cmp(&a.1.created_at).then(b.0.cmp(&a.0)));
            Ok(rows.into_iter().map(|(_, r)| r).collect())
        })
    }

    /// Append one audit row.
    pub fn append_call(&self, row: &CtlCallRow) -> Result<(), String> {
        let val = row.encode();
        self.run(move |trx| {
            let seq = ctl_next_seq(trx, &self.keys.ctl_seq(CTL_CALL_ROW))?;
            trx.set(&self.keys.ctl_call(seq), &val);
            Ok(())
        })
    }

    /// The most recent `limit` audit rows, newest first: a REVERSE range read off
    /// the tail of the `calls` subrange, which is `ORDER BY seq DESC LIMIT n`
    /// without the sort.
    pub fn recent_calls(&self, limit: usize) -> Result<Vec<CtlCallRow>, String> {
        let mut opt = self.keys.ctl_calls();
        opt.reverse = true;
        opt.limit = Some(limit);
        self.run(move |trx| {
            let kvs = futures::executor::block_on(trx.get_range(&opt, 1, true))
                .map_err(TxErr::from)?;
            Ok(kvs
                .as_ref()
                .iter()
                .filter_map(|kv| CtlCallRow::decode(kv.value()))
                .take(limit)
                .collect())
        })
    }
}

/// The cluster-side view-definition table of one `fdb://` deployment — the
/// FoundationDB analogue of PG's `grovefs_control.view_defs` and of the file
/// store's `meta/views/<name>.layout` slots. It lives under the SAME `ctl` root
/// sibling as the credential/audit tables ([`CtlKeyTable`]), so a definition
/// belongs to the deployment: no view's teardown can reach it and no view can
/// shadow it. This is what makes an `fdb://` control plane a single system of
/// record shared by every meta-server / MCP / CLI on the cluster, exactly as PG
/// is — nothing to sync, nothing to co-locate.
///
/// Cheap to build and safe to share (`&self` everywhere): the `Database` handle
/// behind it is the per-cluster one every view already shares, so opening a
/// definition table adds no connection (same contract as [`CtlKeyTable`]).
pub struct CtlDefTable {
    keys: Keys,
    db: DbHandle,
}

impl CtlDefTable {
    /// Open the table on the cluster named by `uri` (`fdb://<cluster-file>`).
    /// Nothing is created here: the definitions are a key space, and an empty
    /// range is a valid empty table — unlike PG, which needs its DDL run first.
    pub fn open(uri: &str) -> Result<Self, String> {
        let cluster_file = FdbConfig::parse_cluster(uri).map_err(|e| e.to_string())?;
        let db = database(&cluster_file).map_err(|e| e.to_string())?;
        Ok(Self {
            keys: root_keys(),
            db,
        })
    }

    /// One control-plane transaction, with the same bounded retry budget the
    /// credential table and the view writes use (see [`with_kv_tx`]).
    fn run<T, F>(&self, body: F) -> Result<T, String>
    where
        F: FnMut(&Transaction) -> Result<T, TxErr>,
    {
        with_kv_tx(&self.db, body).map_err(|e| e.to_string())
    }

    /// Publish (or replace) one definition, claiming a fresh write version in
    /// the same transaction. The version is the hot-reload ticket the file store
    /// gets from mtime and PG from `xmin`: it changes on EVERY publish, even a
    /// byte-identical one (exactly PG's UPDATE semantics), so a cached lens is
    /// always evicted when a definition is re-written.
    pub fn put(&self, name: &str, text: &str) -> Result<(), String> {
        let def_key = self.keys.ctl_def(name);
        let ver_key = self.keys.ctl_def_ver(name);
        let val = text.as_bytes().to_vec();
        self.run(move |trx| {
            let ver = ctl_next_seq(trx, &self.keys.ctl_seq(CTL_DEF_ROW))?;
            trx.set(&def_key, &val);
            trx.set(&ver_key, &ver.pack_to_vec());
            Ok(())
        })
    }

    /// One definition's verbatim text, or `None` when the name is not governed.
    pub fn get(&self, name: &str) -> Result<Option<String>, String> {
        let key = self.keys.ctl_def(name);
        self.run(|trx| {
            // Snapshot read: this transaction writes nothing, and a definition
            // probe must not enter a conflict set a concurrent publish would
            // then have to settle (same reason [`CtlKeyTable::lookup`] does).
            let cur = futures::executor::block_on(trx.get(&key, true)).map_err(TxErr::from)?;
            Ok(cur.as_deref().map(|b| String::from_utf8_lossy(b).into_owned()))
        })
    }

    /// Every governed definition as (name, verbatim text), in name order — the
    /// `ORDER BY name` the PG store spells out, carried here by the tuple-layer
    /// ordering of the packed names.
    pub fn list(&self) -> Result<Vec<(String, String)>, String> {
        self.run(|trx| {
            let mut out = Vec::new();
            for (key, val) in ctl_scan_all(trx, self.keys.ctl_defs())? {
                let Ok((_, name)) = self.keys.ctl.unpack::<(String, String)>(&key) else {
                    continue;
                };
                out.push((name, String::from_utf8_lossy(&val).into_owned()));
            }
            Ok(out)
        })
    }

    /// Remove one definition and its version ticket, reporting whether a row was
    /// there to remove — the `rows_affected > 0` verdict the SQL backends derive
    /// from DELETE. The version key is cleared too, so a later re-publish claims
    /// a fresh ticket rather than resurrecting the old one.
    pub fn remove(&self, name: &str) -> Result<bool, String> {
        let def_key = self.keys.ctl_def(name);
        let ver_key = self.keys.ctl_def_ver(name);
        self.run(move |trx| {
            let cur = futures::executor::block_on(trx.get(&def_key, false)).map_err(TxErr::from)?;
            if cur.is_none() {
                return Ok(false);
            }
            trx.clear(&def_key);
            trx.clear(&ver_key);
            Ok(true)
        })
    }

    /// The hot-reload ticket for `name`: its write version, or `None` when the
    /// name is not governed right now. The version is claimed from the GLOBAL
    /// `view_defs` counter, so it is monotonic and never reused — a remove-then-
    /// re-publish cannot collide with a cached stamp the way a per-name counter
    /// reset to zero would.
    pub fn stamp(&self, name: &str) -> Result<Option<String>, String> {
        let key = self.keys.ctl_def_ver(name);
        self.run(|trx| {
            let cur = futures::executor::block_on(trx.get(&key, true)).map_err(TxErr::from)?;
            Ok(cur.as_deref().and_then(unpack_i64).map(|v| v.to_string()))
        })
    }

    /// One-time upload of a deployment's pre-existing local definition slots
    /// (an older build kept `fdb://` definitions as `meta/views/*.layout` files)
    /// into the cluster. Non-clobbering: a name already governed in the cluster
    /// is left untouched, so a stale local slot can never overwrite a newer
    /// cluster definition. Guarded by a marker written in the SAME transaction,
    /// so it runs once per deployment and is safe to call concurrently. Returns
    /// how many definitions were uploaded (0 once the marker is set).
    pub fn migrate_local(&self, defs: &[(String, String)]) -> Result<usize, String> {
        let marker = self.keys.ctl_defs_migrated();
        self.run(move |trx| {
            let done = futures::executor::block_on(trx.get(&marker, true)).map_err(TxErr::from)?;
            if done.is_some() {
                return Ok(0);
            }
            let mut n = 0;
            for (name, text) in defs {
                let def_key = self.keys.ctl_def(name);
                let cur =
                    futures::executor::block_on(trx.get(&def_key, false)).map_err(TxErr::from)?;
                if cur.is_some() {
                    continue;
                }
                let ver = ctl_next_seq(trx, &self.keys.ctl_seq(CTL_DEF_ROW))?;
                trx.set(&def_key, text.as_bytes());
                trx.set(&self.keys.ctl_def_ver(name), &ver.pack_to_vec());
                n += 1;
            }
            trx.set(&marker, b"1");
            Ok(n)
        })
    }
}

/// What the cluster says about itself: availability, replication mode, fault
/// tolerance and member counts — read from FDB's own `\xff\xff/status/json`
/// special key, so this needs no `fdbcli` on the host.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClusterStatus {
    pub available: bool,
    /// The cluster's own explanation when it is not available (empty when up).
    pub message: String,
    pub redundancy: String,
    pub storage_engine: String,
    pub max_zone_failures: i64,
    pub storage_servers: usize,
    pub coordinators: usize,
}

impl ClusterStatus {
    /// One line for the server banner, e.g.
    /// `redundancy triple (ssd-2), tolerates 1 zone failure(s), 4 storage server(s), 3 coordinator(s)`.
    pub fn topology(&self) -> String {
        format!(
            "redundancy {} ({}), tolerates {} zone failure(s), {} storage server(s), {} coordinator(s)",
            if self.redundancy.is_empty() { "?" } else { &self.redundancy },
            if self.storage_engine.is_empty() { "?" } else { &self.storage_engine },
            self.max_zone_failures,
            self.storage_servers,
            self.coordinators
        )
    }
}

/// Probe the cluster at startup. A read TIMEOUT is an error (that is the 150s
/// silent case this exists to replace); an available=false answer is returned so
/// the caller can print the cluster's own explanation.
pub fn cluster_status(
    uri: &str,
    timeout: std::time::Duration,
) -> Result<ClusterStatus, std::io::Error> {
    let cluster_file = FdbConfig::parse_cluster(uri)?;
    let db = database(&cluster_file)?;
    let trx = db.create_trx().map_err(|e| {
        std::io::Error::other(format!(
            "fdb://{cluster_file}: cannot create a transaction: {e}"
        ))
    })?;
    let ms = timeout.as_millis().max(1).min(i32::MAX as u128) as i32;
    trx.set_option(foundationdb::options::TransactionOption::Timeout(ms))
        .map_err(|e| std::io::Error::other(format!("fdb://{cluster_file}: set timeout: {e}")))?;
    // `\xff\xff/status/json` is FDB's own special key; reading it needs no
    // system-key option (only WRITES to the special key space do).
    let raw = futures::executor::block_on(trx.get(b"\xff\xff/status/json", true))
        .map_err(|e| {
            std::io::Error::other(format!(
                "fdb://{cluster_file}: cluster status unreadable within {}s ({e}) — is the \
                 cluster configured and running? (`fdbcli -C {cluster_file} --exec 'status minimal'`); \
                 the startup probe timeout is `{OPEN_TIMEOUT_ENV}`",
                timeout.as_secs(),
            ))
        })?;
    let bytes = raw
        .map(|b| b.to_vec())
        .unwrap_or_else(|| b"".to_vec());
    let doc: Value = serde_json::from_slice(&bytes).map_err(|e| {
        std::io::Error::other(format!(
            "fdb://{cluster_file}: cluster status is not the documented JSON ({e}); \
             this build speaks the 7.1 status shape"
        ))
    })?;
    let field = |ptr: &str| doc.pointer(ptr).and_then(|v| v.as_str()).unwrap_or_default();
    let count = |ptr: &str| {
        doc.pointer(ptr)
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0)
    };
    // FDB 7.1's status JSON does not spell these the way a reader would guess,
    // which is why the first version of this probe printed
    // "0 storage server(s), 0 coordinator(s)" for a healthy cluster:
    //   * there is no `cluster.storage_servers` — storage is a ROLE inside
    //     `cluster.processes`, and `processes` is an OBJECT keyed by process id;
    //   * the coordinator list lives on the CLIENT side (`client.coordinators`),
    //     with `cluster.configuration.coordinators_count` as the fallback;
    //   * an unavailable client explains itself in `client.messages[].description`
    //     (`cluster.messages` once the cluster itself is up but unhappy), not in
    //     `database_status.message`, which is usually absent.
    // Verified against 7.1.67 with `fdbcli … --exec 'get \xff\xff/status/json'`.
    let role_count = |role: &str| -> usize {
        doc.pointer("/cluster/processes")
            .and_then(|v| v.as_object())
            .map(|procs| {
                procs
                    .values()
                    .filter_map(|p| p.get("roles").and_then(|r| r.as_array()))
                    .flatten()
                    .filter(|r| r.get("role").and_then(|v| v.as_str()) == Some(role))
                    .count()
            })
            .unwrap_or(0)
    };
    let first_message = |ptr: &str| -> String {
        doc.pointer(ptr)
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .map(|m| {
                let name = m.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let desc = m.get("description").and_then(|v| v.as_str()).unwrap_or("");
                if name.is_empty() {
                    desc.to_string()
                } else {
                    format!("{name}: {desc}")
                }
            })
            .unwrap_or_default()
    };
    let db_message = field("/client/database_status/message");
    let message = if !db_message.is_empty() {
        db_message.to_string()
    } else {
        let client = first_message("/client/messages");
        if !client.is_empty() {
            client
        } else {
            first_message("/cluster/messages")
        }
    };
    let coordinators = {
        let listed = count("/client/coordinators/coordinators");
        if listed > 0 {
            listed
        } else {
            doc.pointer("/cluster/configuration/coordinators_count")
                .and_then(|v| v.as_i64())
                .unwrap_or(0)
                .max(0) as usize
        }
    };
    Ok(ClusterStatus {
        available: doc
            .pointer("/client/database_status/available")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        message,
        redundancy: field("/cluster/configuration/redundancy_mode").to_string(),
        storage_engine: field("/cluster/configuration/storage_engine").to_string(),
        max_zone_failures: doc
            .pointer("/cluster/fault_tolerance/max_zone_failures_without_losing_availability")
            .and_then(|v| v.as_i64())
            .unwrap_or(0),
        storage_servers: role_count("storage"),
        coordinators,
    })
}

// ---- lease keeper (the daemon side) ----------------------------------------
//
// `serve()` calls [`hold_writer_lease`] once; the row is renewed from a dedicated
// thread and released from [`shutdown_network`], which is the only point this
// process knows a command is over. A daemon killed by a signal never reaches that
// point, so the row simply expires after one TTL — that is the designed
// behaviour, not a leak: it is what lets a restart take over without an operator
// deleting a key, and `view lease` shows the row while it waits.

struct LeaseKeeper {
    uri: String,
    deployment: String,
    owner: String,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

fn keeper_slot() -> &'static std::sync::Mutex<Option<LeaseKeeper>> {
    static SLOT: OnceLock<std::sync::Mutex<Option<LeaseKeeper>>> = OnceLock::new();
    SLOT.get_or_init(|| std::sync::Mutex::new(None))
}

/// Acquire the lease for `deployment` and keep it alive until shutdown.
///
/// `Ok(None)` means the lease is disabled by `{WRITER_LEASE_ENV}=0`. The renewal
/// thread is the safety half of the contract: if the row is taken over (this
/// process stalled past its TTL, or an operator forced a release), continuing to
/// write would be exactly the divergence the lease exists to prevent, so the
/// process prints why and exits rather than racing the new holder.
pub fn hold_writer_lease(
    uri: &str,
    deployment: &str,
) -> Result<Option<WriterLease>, std::io::Error> {
    let Some(ttl) = writer_lease_ttl() else {
        return Ok(None);
    };
    let owner = writer_owner_id();
    let wait = writer_lease_wait(ttl);
    let lease = acquire_writer_lease(uri, deployment, &owner, ttl, wait)?;
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (u, d, o) = (uri.to_string(), deployment.to_string(), owner.clone());
    let flag = stop.clone();
    let join = std::thread::Builder::new()
        .name("fdb-writer-lease".into())
        .spawn(move || {
            let period = (ttl / 3).max(std::time::Duration::from_millis(200));
            while !flag.load(Ordering::SeqCst) {
                std::thread::sleep(period);
                if flag.load(Ordering::SeqCst) {
                    break;
                }
                match renew_writer_lease(&u, &d, &o, ttl) {
                    Ok(true) => {}
                    Ok(false) => {
                        eprintln!(
                            "meta-server: LOST the FDB writer lease for '{d}' (another server \
                             took it over); stopping instead of writing beside it"
                        );
                        std::process::exit(1);
                    }
                    Err(e) => eprintln!(
                        "meta-server: writer lease renewal failed ({e}); the row stays valid \
                         until its TTL and will be retried"
                    ),
                }
            }
        })
        .map_err(std::io::Error::other)?;
    *keeper_slot()
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = Some(LeaseKeeper {
        uri: uri.to_string(),
        deployment: deployment.to_string(),
        owner,
        stop,
        join: Some(join),
    });
    Ok(Some(lease))
}

fn release_held_writer_lease() {
    let taken = keeper_slot()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .take();
    let Some(mut k) = taken else { return };
    k.stop.store(true, Ordering::SeqCst);
    if let Some(j) = k.join.take() {
        let _ = j.join();
    }
    match release_writer_lease(&k.uri, &k.deployment, Some(&k.owner)) {
        Ok(true) => println!(
            "meta-server: writer lease released for '{}'",
            k.deployment
        ),
        Ok(false) => {}
        Err(e) => eprintln!(
            "meta-server: could not release the writer lease (it expires on its own): {e}"
        ),
    }
}

// ---- key space (design doc §1–§4, frozen by decision D1/D2) -----------------

/// Root prefix of every GroveFS key in the cluster. Packed as a byte string, so
/// the literal first bytes are `0x01 "grovefs" 0x00` — the `\x01` written in §1 of
/// the design doc is the tuple layer's BYTES typecode, not a hand-written
/// constant, and the terminator is `0x00` (a literal NUL inside the string would
/// be escaped as `0xff 0x00`, so `0xff` never terminates anything here). Pinned
/// byte-for-byte by `tests::key_encoding_is_byte_frozen`; a wrong guess here is
/// not a comment bug, it is an unreadable cluster.
const ROOT_PREFIX: &[u8] = b"grovefs";

/// Kind tags of the family subspaces inside a view (§10's frozen list). These are
/// tuple *strings*: the doc spells them `'n'`, and the tuple layer has no char
/// type, so `STRING` (0x02) is the encoding that interoperates unchanged with a
/// Python/Java client following the same document. The choice is pinned by test.
const TAG_NAME: &str = "n";
const TAG_INO: &str = "i";
const TAG_DIR: &str = "k";
const TAG_HOLDER: &str = "h";
const TAG_DATAREF: &str = "r";
const TAG_BY_ID: &str = "d";
const TAG_LOCK: &str = "l";
const TAG_LOCK_OWNER: &str = "lo";
const TAG_LOCK_SID: &str = "ls";
const TAG_LOCK_EXPIRE: &str = "lx";
const TAG_CTRL: &str = "c";
const TAG_ALLOC: &str = "m";
const TAG_REFS: &str = "refs";
/// `root + tuple("writer")` — the cluster-level writer-lease family, a SIBLING of
/// the view prefixes for the same reason `refs` is (it belongs to no view). One
/// row per DEPLOYMENT, where a deployment is the `--meta-dir` whose local
/// control plane governs those trees; see [`FdbStore::open`] for why that, and
/// not the cluster, is the unit that must have one writer.
const TAG_WRITER: &str = "writer";
/// `root + tuple("ctl")` — the deployment's API-key and call-audit tables, a
/// THIRD root sibling beside the view prefixes for the same reason the other two
/// are: a credential belongs to the deployment (for this backend, the cluster),
/// so no view's teardown may reach it, and no view may shadow it.
const TAG_CTL: &str = "ctl";
const CTL_KEY_ROW: &str = "keys";
const CTL_KEY_ORDER: &str = "order";
const CTL_CALL_ROW: &str = "calls";
const CTL_SEQ: &str = "seq";
/// `view_defs` / `view_def_ver` — the cluster-side definition table and its
/// per-name write-version tickets: the FDB analogue of PG's
/// `grovefs_control.view_defs` (and of the file store's `meta/views/*.layout`
/// slots). They share the `ctl` root sibling with the credential/audit tables
/// because a definition belongs to the DEPLOYMENT, so no view's teardown may
/// reach it and no view may shadow it.
const CTL_DEF_ROW: &str = "view_defs";
const CTL_DEF_VER: &str = "view_def_ver";
/// One-time marker: this deployment's pre-existing local definition slots have
/// been uploaded into the cluster (see [`CtlDefTable::migrate_local`]).
const CTL_DEFS_MIGRATED: &str = "defs_migrated";

const CTRL_EPOCH: &str = "epoch";
const ALLOC_ID: &str = "id";
const ALLOC_INO: &str = "ino";
const WRITER_LEASE: &str = "lease";

/// §2: the root's name key is `('n', 0, "")` — `parent_id = 0` stands for the SQL
/// `parent_id IS NULL` of the root row, and root takes part in no lookup or
/// readdir, so no collision with a real directory id is possible.
pub const ROOT_PARENT: i64 = 0;

/// Key-space layout, per `docs/fdb-keyspace.md` §1–§3. Every accessor returns the
/// packed key bytes; the ordering properties come from the tuple layer rather than
/// from string concatenation, which is §1's stated reason for using it at all
/// ("tuple 层按类型定界，天然无 `a|b` 与 `ab` 的拼接歧义").
#[derive(Clone, Debug)]
pub struct Keys {
    /// `root + tuple(view_name)` — one prefix per view, so a view's whole key
    /// space can be dropped by clearing one range, and so no query can leak
    /// across views by construction. That is the FDB analogue of PG's per-view
    /// schema and of SQLite's one-file-per-view.
    view: Subspace,
    /// `root + tuple("refs")` — the shared cross-view byte ledger, deliberately
    /// OUTSIDE any view prefix: every view books and unbooks the same counts
    /// (decision D5, mirroring the `grovefs_refs` schema in the PG backend).
    refs: Subspace,
    /// `root + tuple("writer")` — the cluster-level writer-lease table, a sibling
    /// of the view prefixes (no view owns it, so it must live outside any view
    /// prefix or `view rm --base` of one tree could delete the lease of another).
    writer: Subspace,
    /// `root + tuple("ctl")` — the API-key and call-audit tables the governance
    /// port authenticates against. A sibling for the writer table's reason: they
    /// describe who may open ANY tree, so they cannot live inside one.
    ctl: Subspace,
}

impl Keys {
    /// The cluster-wide root subspace.
    fn root() -> Subspace {
        Subspace::from(ROOT_PREFIX)
    }

    pub fn for_view(view: &str) -> Self {
        let root = Self::root();
        Self {
            view: root.subspace(&view),
            refs: root.subspace(&TAG_REFS),
            writer: root.subspace(&TAG_WRITER),
            ctl: root.subspace(&TAG_CTL),
        }
    }

    /// Prefix of this view's whole key space, for `--purge-view` and for the P6
    /// differential harness (drop everything and rebuild from a dump).
    pub fn view_prefix(&self) -> &Subspace {
        &self.view
    }

    /// One family subspace, e.g. `view + tuple("n")`.
    fn family(&self, tag: &str) -> Subspace {
        self.view.subspace(&tag)
    }

    /// `(v,'n',parent_id,name)` — the directory entry. The KEY IS the uniqueness
    /// constraint: `UNIQUE(parent_id,name)` needs no index here, existence of the
    /// key decides it (§3.1).
    pub fn name(&self, parent: i64, name: &str) -> Vec<u8> {
        self.family(TAG_NAME).pack(&(parent, name))
    }

    /// `(v,'i',ino)` → `(parent_id, name)` pointer. Hot-path getattr (§3.2).
    pub fn ino(&self, ino: i64) -> Vec<u8> {
        self.family(TAG_INO).pack(&(ino,))
    }

    /// `(v,'k',parent,name)` — directories only, so `nlink = 2 + count(prefix)`
    /// never rescans a 100k-entry directory (the 9.6ms → 0.02ms lesson, §3.4).
    pub fn dir_entry(&self, parent: i64, name: &str) -> Vec<u8> {
        self.family(TAG_DIR).pack(&(parent, name))
    }

    /// `(v,'h',ino,parent,name)` — hard-link names of one inode, for `nlink`
    /// (§3.4).
    ///
    /// `parent_id` is part of the KEY and the value stays empty, which is a
    /// deliberate widening of §3.4's `(ino, name)` shape: the pointer repair of
    /// §3.2 has to know the parent of the name it is re-pointing to, and with an
    /// empty value a `(ino, name)` key could not tell it. Keeping the key
    /// existence-only preserves §3.4's actual argument (nlink is a prefix count,
    /// never a rescan); the extra component only makes the repair in `unlink` a
    /// decode instead of a second scan. Recorded as change C2 in the doc's
    /// version notes.
    pub fn holder(&self, ino: i64, parent: i64, name: &str) -> Vec<u8> {
        self.family(TAG_HOLDER).pack(&(ino, parent, name))
    }

    /// `(v,'r',data_ref,parent,name)` — reverse index "who still holds this key",
    /// which is what makes GC decidable. Existence key, not a counter: §3.5
    /// rejects a counter because a rename+unlink interleave can open a window
    /// where it hits zero while a name still exists.
    pub fn dataref(&self, data_ref: &str, parent: i64, name: &str) -> Vec<u8> {
        self.family(TAG_DATAREF).pack(&(data_ref, parent, name))
    }

    /// Prefix of this view's holder keys for one `ino` — the nlink source for
    /// regular files (§3.4, counterpart of `SELECT ino, COUNT(*) WHERE ino=ANY`).
    pub fn holder_prefix(&self, ino: i64) -> RangeOption<'static> {
        self.family(TAG_HOLDER).subspace(&(ino,)).range().into()
    }

    /// `(v,'h',lo,·) .. (v,'h',hi,·)` — the holder rows of every inode in
    /// `[lo,hi]`, so one read can answer a whole page of nlink counts. Ids are
    /// handed out in creation order, so a directory's children normally sit in
    /// a compact run and the span holds little more than the page itself.
    pub fn holder_span(&self, lo: i64, hi: i64) -> RangeOption<'static> {
        let (begin, _) = self.family(TAG_HOLDER).subspace(&(lo,)).range();
        let (_, end) = self.family(TAG_HOLDER).subspace(&(hi,)).range();
        (begin, end).into()
    }

    /// Decode a `'h` key down to the `ino` whose nlink it counts, dropping the
    /// `(parent,name)` that makes the key unique.
    pub fn decode_holder_ino(&self, key: &[u8]) -> Option<i64> {
        self.family(TAG_HOLDER)
            .unpack::<(i64, i64, String)>(key)
            .ok()
            .map(|(ino, _, _)| ino)
    }

    /// Prefix of this view's directory-existence keys under one parent row id —
    /// the nlink source for directories (§3.4).
    pub fn dir_children_prefix(&self, parent_id: i64) -> RangeOption<'static> {
        self.family(TAG_DIR).subspace(&(parent_id,)).range().into()
    }

    /// Prefix of every name in this view whose data_ref is `data_ref`.
    pub fn dataref_prefix_of(&self, data_ref: &str) -> RangeOption<'static> {
        self.family(TAG_DATAREF).subspace(&(data_ref,)).range().into()
    }

    /// Prefix of the whole view, for `dump_tree` and view teardown.
    pub fn all_range(&self) -> RangeOption<'static> {
        self.view.range().into()
    }

    /// Prefix of every `(parent,name)` holding `data_ref` in this view.
    pub fn dataref_prefix(&self, data_ref: &str) -> RangeOption<'static> {
        self.dataref_prefix_of(data_ref)
    }

    /// Prefix of the whole `'r'` family of this view: every reverse-index row it
    /// holds. Only [`FdbStore::import_jsonl`] reads it, because a REPLACE has to
    /// give the old tree's ledger units back before tearing the tree down
    /// (§3.6: the shared ledger carries one unit per VIEW per content key, so a
    /// teardown that does not release leaves every blob the replaced tree alone
    /// held pinned forever).
    pub fn dataref_all_range(&self) -> RangeOption<'static> {
        self.family(TAG_DATAREF).range().into()
    }

    /// Decode a `'r` key down to its `data_ref`, dropping the `(parent,name)`
    /// that makes the key unique.
    pub fn decode_dataref(&self, key: &[u8]) -> Option<String> {
        self.family(TAG_DATAREF)
            .unpack::<(String, i64, String)>(key)
            .ok()
            .map(|(dr, _, _)| dr)
    }

    /// `(v,'d',id)` → `(parent_id, name)`. Needed because the code has six
    /// `WHERE id=$1` lookups and FDB has no rowid to fall back on (§3.9).
    pub fn by_id(&self, id: i64) -> Vec<u8> {
        self.family(TAG_BY_ID).pack(&(id,))
    }

    /// `(v,'l',ino,l_start,l_end,owner)` — lock primary key; `expires_at` is in
    /// the VALUE on purpose, so §3.7's `renew_locks` is a value update plus an
    /// `'lx'` key swap rather than a move of the primary key.
    pub fn lock(&self, ino: i64, start: i64, end: i64, owner: i64) -> Vec<u8> {
        self.family(TAG_LOCK).pack(&(ino, start, end, owner))
    }

    /// Prefix of one inode's lock rows. Overlap (`l_start<=e AND l_end>=s`) is a
    /// two-dimensional condition and no single bounded range expresses it, so —
    /// exactly as PG does — the arbitration is a prefix scan filtered inside the
    /// transaction. The bound that makes that acceptable is the same one PG
    /// states: the lock rows naming one inode are few (§3.7, and §10's per-ino
    /// lock-bucket escalation only if a hot file ever breaks it).
    pub fn lock_ino_prefix(&self, ino: i64) -> RangeOption<'static> {
        self.family(TAG_LOCK).subspace(&(ino,)).range().into()
    }

    /// `(v,'lx',expires_at,ino,l_start,l_end,owner)` — the expire-ordered existence key
    /// (§3.7, counterpart of `idx_locks_expire`).
    ///
    /// The trailing `l_end`/`owner` are not decoration: a secondary key exists so
    /// that the operation riding it never has to search for its primary. §3.7's
    /// first draft wrote this as `(expires_at,ino,l_start,owner)`, which cannot
    /// name the `'l` row — recovering `l_end` would then cost a `'l` family scan
    /// per swept lock, i.e. the index stops being an index. Encoding-level
    /// addition, the kind §10 allows without reopening the phase.
    pub fn lock_expire(&self, expires_at: i64, ino: i64, start: i64, end: i64, owner: i64) -> Vec<u8> {
        self.family(TAG_LOCK_EXPIRE).pack(&(expires_at, ino, start, end, owner))
    }

    /// Every `'lx` row whose `expires_at` is strictly below `now` — the lease
    /// sweep as ONE bounded range instead of a family scan with a filter, which
    /// is what the key ordering buys and what PG's `DELETE ... WHERE expires_at <
    /// $1` gets out of `idx_locks_expire`.
    pub fn lock_expired_upto(&self, now: i64) -> RangeOption<'static> {
        let fam = self.family(TAG_LOCK_EXPIRE);
        let (begin, _end) = fam.range();
        RangeOption {
            begin: KeySelector::first_greater_or_equal(begin),
            end: KeySelector::first_greater_or_equal(fam.pack(&(now,))),
            ..RangeOption::default()
        }
    }

    /// `(v,'lo',owner,ino,l_start,l_end)` — counterpart of `idx_locks_owner`, the basis
    /// for close/exit implicit release not leaking (§3.7).
    pub fn lock_owner(&self, owner: i64, ino: i64, start: i64, end: i64) -> Vec<u8> {
        self.family(TAG_LOCK_OWNER).pack(&(owner, ino, start, end))
    }

    /// Prefix of every lock held by one file description — `release` on the last
    /// close reaps exactly this range.
    pub fn lock_owner_prefix(&self, owner: i64) -> RangeOption<'static> {
        self.family(TAG_LOCK_OWNER).subspace(&(owner,)).range().into()
    }

    /// `(v,'ls',sid,ino,l_start,l_end,owner)` — counterpart of `idx_locks_sid`.
    ///
    /// `owner` has to be part of this key even though `sid` is not `owner`: two
    /// DIFFERENT owners in one session may hold the same `(ino,l_start)` (read
    /// locks are compatible across owners, so `read(5..9)` by two processes can
    /// coexist), which makes `(sid,ino,l_start)` an ambiguous pointer and would
    /// make one renewal either silently drop a lock or rewrite the wrong row.
    pub fn lock_sid(&self, sid: i64, ino: i64, start: i64, end: i64, owner: i64) -> Vec<u8> {
        self.family(TAG_LOCK_SID).pack(&(sid, ino, start, end, owner))
    }

    /// Prefix of every lock taken by one mount session — renewal and
    /// connection-loss reaping both ride it (§3.7).
    pub fn lock_sid_prefix(&self, sid: i64) -> RangeOption<'static> {
        self.family(TAG_LOCK_SID).subspace(&(sid,)).range().into()
    }

    /// Decode a `'l` key back to `(ino, l_start, l_end, owner)`.
    pub fn decode_lock(&self, key: &[u8]) -> Option<(i64, i64, i64, i64)> {
        self.family(TAG_LOCK)
            .unpack::<(i64, i64, i64, i64)>(key)
            .ok()
    }

    /// Decode a `'lx` key back to `(expires_at, ino, l_start, l_end, owner)`.
    pub fn decode_lock_expire(&self, key: &[u8]) -> Option<(i64, i64, i64, i64, i64)> {
        self.family(TAG_LOCK_EXPIRE)
            .unpack::<(i64, i64, i64, i64, i64)>(key)
            .ok()
    }

    /// Decode a `'lo` key back to `(owner, ino, l_start, l_end)`.
    pub fn decode_lock_owner(&self, key: &[u8]) -> Option<(i64, i64, i64, i64)> {
        self.family(TAG_LOCK_OWNER)
            .unpack::<(i64, i64, i64, i64)>(key)
            .ok()
    }

    /// Decode a `'ls` key back to `(sid, ino, l_start, l_end, owner)`.
    pub fn decode_lock_sid(&self, key: &[u8]) -> Option<(i64, i64, i64, i64, i64)> {
        self.family(TAG_LOCK_SID)
            .unpack::<(i64, i64, i64, i64, i64)>(key)
            .ok()
    }

    /// `(v,'c','epoch')` — the fencing epoch, one key (§3.8).
    pub fn epoch(&self) -> Vec<u8> {
        self.family(TAG_CTRL).pack(&(CTRL_EPOCH,))
    }

    /// `(v,'m','id')` / `(v,'m','ino')` — block allocator water marks (§4).
    pub fn alloc(&self, which: AllocKind) -> Vec<u8> {
        self.family(TAG_ALLOC).pack(&(which.tag(),))
    }

    /// Prefix range of every name under `parent`, in name order — a `readdir` is
    /// one range scan, no sort, no OFFSET (§3.3).
    pub fn children_range(&self, parent: i64) -> RangeOption<'static> {
        self.family(TAG_NAME).subspace(&(parent,)).range().into()
    }

    /// Same range starting strictly after `after`, which is how §3.3's
    /// `start_after` paging cursor is expressed without server-side cursor state.
    pub fn children_range_after(&self, parent: i64, after: &str) -> RangeOption<'static> {
        let family = self.family(TAG_NAME);
        let (_begin, end) = family.subspace(&(parent,)).range();
        let cursor = family.pack(&(parent, after));
        RangeOption {
            // The scan starts strictly after the cursor's own key, or the caller
            // re-reads the entry it already handed to the kernel.
            begin: KeySelector::first_greater_than(cursor),
            end: KeySelector::first_greater_or_equal(end),
            ..RangeOption::default()
        }
    }

    /// Does `key` belong to this view's own subspace or to the shared ledger?
    /// Every family this backend writes is one or the other (§1), so this single
    /// predicate is the whole isolation rule — and [`Tx`] debug-asserts it on
    /// every access, which turns "no query escapes the view's subspace" from
    /// discipline into a failure that fires at the call site that broke it rather
    /// than surfacing much later as another view's data in a listing.
    pub fn permits(&self, key: &[u8]) -> bool {
        self.view.is_start_of(key) || self.refs.is_start_of(key) || self.writer.is_start_of(key)
    }

    /// Prefix of the whole `'n'` family — every entry of the view, which is what
    /// `dump_tree` (and only `dump_tree`) is allowed to read.
    pub fn all_names_range(&self) -> RangeOption<'static> {
        self.family(TAG_NAME).range().into()
    }

    /// Ledger key `(refs, data_ref)` in the SHARED subspace (§3.6).
    pub fn ledger(&self, data_ref: &str) -> Vec<u8> {
        self.refs.pack(&(data_ref,))
    }

    /// Whole shared ledger, for [`FdbStore::ledger_rows`]'s read-only inventory.
    /// The `refs` subspace holds nothing but `(data_ref,)` rows, so its own range
    /// IS the ledger — no family tag to add.
    pub fn ledger_range(&self) -> RangeOption<'static> {
        self.refs.range().into()
    }

    /// `(root,'writer','lease',deployment)` — one writer-lease row per deployment.
    /// The deployment string is the canonical `--meta-dir`, so two servers that
    /// share a control plane queue behind each other while two independent
    /// deployments on one cluster keep separate rows.
    pub fn writer_lease(&self, deployment: &str) -> Vec<u8> {
        self.writer.pack(&(WRITER_LEASE, deployment))
    }

    /// Every writer-lease row on this cluster, for `grovefs view lease`.
    pub fn writer_leases(&self) -> RangeOption<'static> {
        self.writer.subspace(&WRITER_LEASE).range().into()
    }

    /// `(root,'ctl','keys',id)` → one credential row (JSON, see [`CtlKeyRow`]).
    /// `id` is the whole key: the table has exactly one lookup pattern, and an
    /// insert is therefore one `set` with no index to maintain.
    pub fn ctl_key(&self, id: &str) -> Vec<u8> {
        self.ctl.pack(&(CTL_KEY_ROW, id))
    }

    /// `(root,'ctl','order',seq)` → the id of the `seq`-th key minted. This is
    /// the ordering `apikey list` reports newest-first, carried by the KEY so the
    /// listing needs no sort of a table it cannot index.
    pub fn ctl_order(&self, seq: i64) -> Vec<u8> {
        self.ctl.pack(&(CTL_KEY_ORDER, seq))
    }

    /// Whole insertion-order index.
    pub fn ctl_orders(&self) -> RangeOption<'static> {
        self.ctl.subspace(&CTL_KEY_ORDER).range().into()
    }

    /// `(root,'ctl','calls',seq)` → one audit row (JSON, see [`CtlCallRow`]).
    pub fn ctl_call(&self, seq: i64) -> Vec<u8> {
        self.ctl.pack(&(CTL_CALL_ROW, seq))
    }

    /// Every audit row in insertion order; [`CtlKeyTable::recent_calls`] reads it
    /// backwards with a limit, so "newest first" is a range direction, not a
    /// sort.
    pub fn ctl_calls(&self) -> RangeOption<'static> {
        self.ctl.subspace(&CTL_CALL_ROW).range().into()
    }

    /// `(root,'ctl','seq',kind)` — the insertion counter for `"keys"`, `"calls"`
    /// or `"view_defs"`. Separate counters rather than one so revoking a key
    /// cannot leave a hole in the audit sequence (a gap is how an operator spots
    /// a rewritten log), and a definition re-publish never consumes an audit seq.
    pub fn ctl_seq(&self, kind: &str) -> Vec<u8> {
        self.ctl.pack(&(CTL_SEQ, kind))
    }

    /// `(root,'ctl','view_defs',name)` → one definition's verbatim text. `name`
    /// is the whole key (one lookup pattern), so a publish is one `set` plus its
    /// version ticket — the FDB analogue of a `grovefs_control.view_defs` row.
    pub fn ctl_def(&self, name: &str) -> Vec<u8> {
        self.ctl.pack(&(CTL_DEF_ROW, name))
    }

    /// `(root,'ctl','view_def_ver',name)` → the definition's write version (i64),
    /// the hot-reload ticket the file store reads from mtime and PG from `xmin`.
    pub fn ctl_def_ver(&self, name: &str) -> Vec<u8> {
        self.ctl.pack(&(CTL_DEF_VER, name))
    }

    /// Whole definition table; the tuple layer orders the packed names, so the
    /// scan is already in `ORDER BY name` order (no sort of an unindexable table).
    pub fn ctl_defs(&self) -> RangeOption<'static> {
        self.ctl.subspace(&CTL_DEF_ROW).range().into()
    }

    /// `(root,'ctl','defs_migrated')` → the one-time local-slot migration marker.
    pub fn ctl_defs_migrated(&self) -> Vec<u8> {
        self.ctl.pack(&(CTL_DEFS_MIGRATED,))
    }

    /// Decode one ledger key back to its `data_ref`.
    pub fn decode_ledger(&self, key: &[u8]) -> Option<String> {
        self.refs.unpack::<(String,)>(key).ok().map(|(dr,)| dr)
    }

    // ---- decoders -----------------------------------------------------------
    //
    // Unpacking lives on `Keys` rather than at each call site so that "a query
    // only ever decodes its own view's families" is enforced by the type: a
    // `Subspace::unpack` refuses any key outside its prefix (`BadPrefix`), so a
    // mis-ranged scan decodes to `None` instead of reading another view's data.

    /// Decode a `'n` key back to `(parent_id, name)`.
    pub fn decode_name(&self, key: &[u8]) -> Option<(i64, String)> {
        self.family(TAG_NAME).unpack::<(i64, String)>(key).ok()
    }

    /// Decode a `'h` key back to `(ino, parent_id, name)`.
    pub fn decode_holder(&self, key: &[u8]) -> Option<(i64, i64, String)> {
        self.family(TAG_HOLDER)
            .unpack::<(i64, i64, String)>(key)
            .ok()
    }

    /// Decode a `'k` directory-existence key back to `(parent_id, name)`.
    pub fn decode_dir_entry(&self, key: &[u8]) -> Option<(i64, String)> {
        self.family(TAG_DIR).unpack::<(i64, String)>(key).ok()
    }
}

/// The two counters are separate because `link_node` reuses the source `ino` while
/// allocating a new row id — one shared counter would make `id == ino` (which the
/// SQLite/PG backends rely on for fresh names) hold inconsistently (§4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AllocKind {
    Id,
    Ino,
}

impl AllocKind {
    pub fn tag(self) -> &'static str {
        match self {
            AllocKind::Id => ALLOC_ID,
            AllocKind::Ino => ALLOC_INO,
        }
    }
}

/// Parsed `--meta-store` target. The URI carries the cluster file; everything else
/// about the cluster — coordinators, TLS, machine_class — belongs in that file,
/// which is FDB's own distribution mechanism and must not be re-invented here.
#[derive(Clone, Debug)]
pub struct FdbConfig {
    pub cluster_file: String,
    pub view: String,
}

impl FdbConfig {
    /// Accept `fdb://<path-to-cluster-file>` or `fdb://?cluster=<path>`; with no
    /// path, fall back to `$GROVEFS_FDB_CLUSTER_FILE`. FDB's own convention is
    /// `/etc/foundationdb/fdb.cluster`, but a user-space test cluster is not there
    /// and installing to `/usr` is not something this build may assume.
    pub fn parse(uri: &str, view: &str) -> Result<Self, std::io::Error> {
        let cluster = Self::parse_cluster(uri)?;
        // Same input gate as PG's `schema_for` (pg_store.rs:71-80): an unusable
        // view name is refused at open, never silently mangled.
        if !crate::layout::valid_view_name(view) {
            return Err(std::io::Error::other(format!(
                "view name '{view}' is not usable as an FDB subspace key"
            )));
        }
        // `refs` is not a view: it is the SHARED cross-view byte ledger (§3.6),
        // deliberately a sibling of the view prefixes. Opening a view with that
        // name would put a tree's root row inside the ledger's subspace — the
        // two would then share keys, and `view rm --base refs` would delete the
        // booking table of every other view. Refused at the gate, before any key
        // is written, rather than left as a name an operator can stumble into.
        if view == TAG_REFS {
            return Err(std::io::Error::other(
                "view name 'refs' is reserved: it is the shared cross-view byte \
                 ledger, not a tree — choose another name",
            ));
        }
        // `writer` is the other sibling, and the same argument applies: a tree
        // named 'writer' would put its root row inside the lease table, so
        // `view lease` would report a tree as a holder and `view rm --base writer`
        // would be the only way to clear a lease.
        if view == TAG_WRITER {
            return Err(std::io::Error::other(
                "view name 'writer' is reserved: it is the cluster's writer-lease \
                 table (one row per deployment), not a tree — choose another name",
            ));
        }
        // `ctl` is the third sibling: the API-key/audit tables the governance
        // port authenticates against. A tree named 'ctl' would put its root row
        // inside them, and `view rm --base ctl` would then delete the only
        // credentials that can govern the cluster.
        if view == TAG_CTL {
            return Err(std::io::Error::other(
                "view name 'ctl' is reserved: it holds the cluster's API-key and \
                 call-audit tables, not a tree — choose another name",
            ));
        }
        // §1 also carries PG's 25-byte cap over, even though FDB needs no such
        // limit for itself: a view this backend can open but `schema_for` can
        // never name would be a view that cannot be migrated to or from
        // PostgreSQL (P8) — an operational trap set silently at open time.
        // Refusing is the same choice PG makes, and it keeps the set of view
        // names identical across the three backends.
        if view.len() > 25 {
            return Err(std::io::Error::other(format!(
                "view name is {} bytes; the backends all cap it at 25 so no view \
                 is openable on one engine and unrepresentable on another",
                view.len()
            )));
        }
        Ok(Self {
            cluster_file: cluster,
            view: view.to_string(),
        })
    }

    /// The cluster file named by an `fdb://` uri — everything a CLUSTER-WIDE
    /// operation needs, with no view in hand (`view scan`, which enumerates the
    /// subspaces that exist rather than opening one).
    pub fn parse_cluster(uri: &str) -> Result<String, std::io::Error> {
        let rest = uri
            .strip_prefix("fdb://")
            .ok_or_else(|| std::io::Error::other(format!("not an fdb uri: {uri}")))?;
        let mut cluster = rest.split('?').next().unwrap_or("").to_string();
        for kv in rest.split('?').skip(1).flat_map(|s| s.split('&')) {
            if let Some(v) = kv.strip_prefix("cluster=") {
                cluster = v.to_string();
            }
        }
        if cluster.is_empty() {
            cluster = std::env::var("GROVEFS_FDB_CLUSTER_FILE").unwrap_or_default();
        }
        if cluster.is_empty() {
            return Err(std::io::Error::other(
                "fdb:// uri names no cluster file: give `fdb://<path>` \
                 or set GROVEFS_FDB_CLUSTER_FILE",
            ));
        }
        Ok(cluster)
    }
}

// ---- transaction wrapper ----------------------------------------------------

/// The driver's view of a transaction body's failure. Mirrors
/// `pg_store::WriteErr` one-for-one: PG asks "was this SQLSTATE 40001/40P01?",
/// FDB asks "was this an OCC abort code?", and in both cases a `Hard` error is
/// propagated to the POSIX caller untouched so `EEXIST`/`ENOTEMPTY` stay visible.
///
/// `pub(crate)`, like [`Tx`]: a transaction is a backend-internal scope, and the
/// surface callers speak to is `MetaError`. Making both crate-visible is also
/// what keeps "no one outside this module can bypass [`FdbStore::with_serial_retry`]
/// and commit a raw transaction" checkable by eye.
pub(crate) enum TxErr {
    /// A retriable FDB conflict/timeout: re-run the whole body.
    Retry(FdbError),
    /// Anything else, already mapped onto the caller-facing vocabulary.
    Hard(MetaError),
}

impl From<MetaError> for TxErr {
    fn from(me: MetaError) -> Self {
        TxErr::Hard(me)
    }
}

impl From<FdbError> for TxErr {
    fn from(e: FdbError) -> Self {
        if is_retriable(e.code()) {
            TxErr::Retry(e)
        } else {
            TxErr::Hard(map_fdb_error(&e))
        }
    }
}

/// A transaction plus the key space it is allowed to touch. Passing these around
/// together is what keeps "no query escapes the view's subspace" a property of the
/// type rather than of discipline.
pub(crate) struct Tx {
    inner: Transaction,
    keys: Keys,
}

impl Tx {
    /// Point read. Blocks on the client's completion callback (see the module
    /// docs); `snapshot = false` so the read is added to the transaction's read
    /// conflict set — that is precisely what makes the uniqueness arbitration of
    /// §3.1 ("read the rival's committed row after retrying") work.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, TxErr> {
        let mut out = self.get_multi(&[key.to_vec()])?;
        Ok(out.pop().unwrap_or(None))
    }

    /// Several point reads issued together and awaited once, so that all of them
    /// are in flight before any thread parks — the shape §3.2 promises for
    /// independent reads. Results come back in input order.
    ///
    /// `getattr` cannot use it: its second key is its first value. Anything that
    /// batches reads behind this must therefore know the keys up front, which is
    /// exactly the situation a `readdir` page's records are in and the situation
    /// a dependent pointer lookup is not.
    pub fn get_multi(&self, keys: &[Vec<u8>]) -> Result<Vec<Option<Vec<u8>>>, TxErr> {
        for k in keys {
            debug_assert!(self.keys.permits(k), "read outside the view: {k:?}");
        }
        let futs = keys
            .iter()
            .map(|k| self.inner.get(k, false))
            .collect::<Vec<_>>();
        // `join_all` polls every future before blocking, so all reads are in
        // flight at once — the round-trip property the design doc promises.
        let fetched = futures::executor::block_on(futures::future::join_all(futs));
        let mut out = Vec::with_capacity(fetched.len());
        for f in fetched {
            out.push(f.map(|v| v.map(|b| b.to_vec())).map_err(TxErr::from)?);
        }
        Ok(out)
    }

    /// Read one value and decode it as a single `i64` (epoch, allocator mark,
    /// ledger count). Absent key reads as `0`, matching the SQL engines'
    /// "no row yet" handling rather than inventing a third state.
    pub fn get_i64(&self, key: &[u8]) -> Result<i64, TxErr> {
        Ok(self.get(key)?.as_deref().and_then(unpack_i64).unwrap_or(0))
    }

    pub fn set(&self, key: &[u8], value: &[u8]) {
        debug_assert!(self.keys.permits(key), "write outside the view: {key:?}");
        self.inner.set(key, value);
    }

    pub fn set_i64(&self, key: &[u8], v: i64) {
        debug_assert!(self.keys.permits(key), "write outside the view: {key:?}");
        self.inner.set(key, &v.pack_to_vec());
    }

    pub fn clear(&self, key: &[u8]) {
        debug_assert!(self.keys.permits(key), "delete outside the view: {key:?}");
        self.inner.clear(key);
    }

    /// Range read of one page. `limit == 0` means "everything in the range", which
    /// §9's key-size table bounds at a safe size for a directory listing.
    pub fn get_range(&self, opt: &RangeOption) -> Result<Vec<(Vec<u8>, Vec<u8>)>, TxErr> {
        self.assert_range(opt);
        let values = futures::executor::block_on(self.inner.get_range(opt, 1, false))
            .map_err(TxErr::from)?;
        Ok(values
            .as_ref()
            .iter()
            .map(|kv| (kv.key().to_vec(), kv.value().to_vec()))
            .collect())
    }

    /// Every key in `opt`, pages followed until the range is exhausted. FDB has no
    /// server-side "count" or "select all", and one `get_range` hands back at most
    /// one reply-sized page, so a directory bigger than a page would otherwise be
    /// *silently truncated* — which for `readdir` is a correctness bug, not a
    /// performance one. `RangeOption::next_range` carries the limit remainder and
    /// the `more()` continuation forward, which is why no key-ordering logic is
    /// repeated here.
    pub fn scan(&self, opt: &RangeOption) -> Result<Vec<(Vec<u8>, Vec<u8>)>, TxErr> {
        self.assert_range(opt);
        let mut out = Vec::new();
        let mut next = Some(opt.clone());
        while let Some(page_opt) = next {
            let values = futures::executor::block_on(self.inner.get_range(&page_opt, 1, false))
                .map_err(TxErr::from)?;
            for kv in values.as_ref().iter() {
                out.push((kv.key().to_vec(), kv.value().to_vec()));
            }
            next = page_opt.next_range(&values);
        }
        Ok(out)
    }

    /// Size of a prefix, read by scanning it. That is the honest counterpart of
    /// PG's `COUNT(*)`: the client counts keys because the server will not.
    /// Bounded in practice by the per-directory key counts §9's size table
    /// assumes, and every caller of this is an `nlink` computation over an
    /// existence-key prefix — never over the whole `'n'` family.
    pub fn count_range(&self, opt: &RangeOption) -> Result<u64, TxErr> {
        Ok(self.scan(opt)?.len() as u64)
    }

    /// The range form of [`Tx::get_multi`]: the row count of every option, with
    /// one wave of concurrent requests instead of N serial round-trips.
    ///
    /// [`Self::scan`] already knows how to follow `next_range` past one
    /// reply-sized page; this does the same per range, but keeps up to
    /// [`COUNT_MULTI_FANOUT`] ranges in flight, so a `readdir` page's per-entry
    /// nlink counts cost a handful of round-trips instead of one per entry.
    pub fn count_multi(&self, opts: &[RangeOption<'static>]) -> Result<Vec<u64>, TxErr> {
        for opt in opts {
            self.assert_range(opt);
        }
        let mut counts = vec![0u64; opts.len()];
        let mut pending: Vec<(usize, RangeOption<'static>)> =
            opts.iter().cloned().enumerate().collect();
        while !pending.is_empty() {
            let wave: Vec<(usize, RangeOption<'static>)> =
                pending.drain(..pending.len().min(COUNT_MULTI_FANOUT)).collect();
            let futs = wave
                .iter()
                .map(|(_, opt)| self.inner.get_range(opt, 1, false))
                .collect::<Vec<_>>();
            // `join_all` polls every future before blocking, so the whole wave
            // is in flight at once — the same property `get_multi` relies on.
            let pages = futures::executor::block_on(futures::future::join_all(futs));
            for ((idx, opt), page) in wave.into_iter().zip(pages) {
                let page = page.map_err(TxErr::from)?;
                counts[idx] += page.len() as u64;
                if let Some(next) = opt.next_range(&page) {
                    pending.push((idx, next));
                }
            }
        }
        Ok(counts)
    }

    /// The nlink counts of a page's regular files in ONE read, when their inode
    /// ids form a compact run — the usual case, since ids are handed out in
    /// creation order. Each entry's count alone would be a request of its own:
    /// 128 serial counts measured ~12 ms, against ~1 ms for the whole page's
    /// span.
    ///
    /// `None` when the ids are too scattered (> `SPAN_SLACK` ids per entry) or
    /// the span already holds far more rows than the page can account for
    /// (> `ROW_SLACK` per entry): the caller then counts per prefix, which is
    /// always correct.
    pub fn count_holder_span(&self, inos: &[i64]) -> Result<Option<Vec<u64>>, TxErr> {
        const SPAN_SLACK: i128 = 8;
        const ROW_SLACK: usize = 4;
        let (Some(lo), Some(hi)) = (inos.iter().min(), inos.iter().max()) else {
            return Ok(None);
        };
        let span = *hi as i128 - *lo as i128 + 1;
        if span > inos.len() as i128 * SPAN_SLACK {
            return Ok(None);
        }
        let cap = inos.len() * ROW_SLACK + 64;
        let mut opt = self.keys.holder_span(*lo, *hi);
        opt.limit = Some(cap + 1);
        let rows = self.scan(&opt)?;
        if rows.len() > cap {
            return Ok(None);
        }
        let mut per_ino: std::collections::HashMap<i64, u64> = std::collections::HashMap::new();
        for (key, _) in &rows {
            if let Some(ino) = self.keys.decode_holder_ino(key) {
                *per_ino.entry(ino).or_insert(0) += 1;
            }
        }
        Ok(Some(
            inos.iter()
                .map(|ino| per_ino.get(ino).copied().unwrap_or(0))
                .collect(),
        ))
    }

    /// The range form of [`Keys::permits`]: both bounds must sit inside the
    /// view (or the ledger), so a range built from the wrong `Keys` cannot read
    /// — or be cleared — across a view boundary.
    fn assert_range(&self, opt: &RangeOption) {
        debug_assert!(
            self.keys.permits(&opt.begin.key().to_vec())
                && self.keys.permits(&opt.end.key().to_vec()),
            "range outside the view: {:?}..{:?}",
            opt.begin.key(),
            opt.end.key()
        );
    }

    /// Clear everything under `prefix` with ONE range mutation.
    ///
    /// This is the subspace-teardown form (§1): FoundationDB logs a range clear as
    /// a single mutation, so its cost does not grow with the number of keys inside
    /// it — unlike PG's `DROP SCHEMA ... CASCADE`, which must find every row.
    ///
    /// The exclusive upper bound is deliberately OUTSIDE the prefix (the prefix
    /// with its last byte raised), so unlike [`Tx::assert_range`] the end key is
    /// not and must not be tested with `Keys::permits`; only the start bound is
    /// inside the view. A prefix whose last byte is `0xff` has no such bound within
    /// reach, and this refuses it rather than carrying into the next byte: an
    /// overflow here would clear a neighbour's keyspace. No GroveFS prefix is ever
    /// generated that way, because the tuple layer terminates a string element with
    /// `0x00` (the byte form §1 pins).
    fn clear_prefix(&self, prefix: &[u8]) -> Result<(), TxErr> {
        debug_assert!(
            self.keys.permits(prefix),
            "range clear outside the view: {prefix:?}"
        );
        let Some(end) = prefix_upper_bound(prefix) else {
            return Err(TxErr::Hard(MetaError::Db(
                "fdb_store: cannot clear a prefix ending in 0xff (no in-range upper bound)".into(),
            )));
        };
        self.inner.clear_range(prefix, &end);
        Ok(())
    }

    /// Read-modify-write of one counter inside the caller's transaction — used by
    /// the ledger (§3.6) and the allocator (§4), where a separate transaction would
    /// open a window in which two views both see "first holder". §3.6 picks this
    /// shape over `atomic_op` precisely because the post-value must be visible in
    /// the same snapshot, which is what PG's `RETURNING holders` gives.
    fn bump(&self, key: &[u8], delta: i64) -> Result<i64, TxErr> {
        let cur = self.get_i64(key)?;
        let new = cur + delta;
        self.set_i64(key, new);
        Ok(new)
    }

    /// Monotonic raise: record `v` only if it is above what is stored. The
    /// allocator watermark (§4's migration continuation) must never go backwards,
    /// and a plain `set` by a lagging process could do exactly that.
    fn raise_to_max(&self, key: &[u8], v: i64) -> Result<(), TxErr> {
        if v > self.get_i64(key)? {
            self.set_i64(key, v);
        }
        Ok(())
    }
}

/// Decode a single-`i64` tuple value.
fn unpack_i64(bytes: &[u8]) -> Option<i64> {
    Element::unpack_root(bytes).ok()?.as_i64()
}

/// The exclusive upper bound of `prefix`: the prefix with its last byte raised by
/// one. `None` when that would overflow, which is the case [`Tx::clear_prefix`]
/// refuses instead of silently carrying.
fn prefix_upper_bound(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut end = prefix.to_vec();
    let last = end.last_mut()?;
    if *last == u8::MAX {
        return None;
    }
    *last += 1;
    Some(end)
}

/// Decode a `(parent_id, name)` pointer value (§3.2 / §3.9).
///
/// The value is a depth-0 two-element tuple, which the tuple layer writes inline
/// with no `NESTED` wrapper, so it must be read back as a sequence of elements
/// (`Vec<Element>` stops when the input is exhausted) rather than as one
/// `Element` — the latter would decode `parent_id` and then fail on trailing bytes.
fn unpack_ptr(bytes: &[u8]) -> Option<(i64, String)> {
    let items = Vec::<Element>::unpack_root(bytes).ok()?;
    Some((items.first()?.as_i64()?, items.get(1)?.as_str()?.to_string()))
}

/// A key that exists but decodes to nothing is not a POSIX condition — it is a
/// broken writer, so it surfaces as a db error naming the key rather than as one
/// of the five errno-mapped variants [`MetaError`] carries for ordinary races.
fn keys_corrupt(key: &[u8]) -> MetaError {
    MetaError::Db(format!(
        "fdb_store: value at key {key:?} does not decode as a record (§2)"
    ))
}

/// Every entry of one directory, as `(parent_id, name, record)` triples in the
/// order the range returns them — which IS name order, so no sort runs here
/// (§3.3's "键序即 name 序"). Entries whose value fails to decode are skipped:
/// one corrupt row must not turn a `readdir` into an error the way a SQL driver
/// error would, because the kernel's getdents contract is "give me what you can".
fn scan_children(tx: &Tx, keys: &Keys, opt: RangeOption<'static>) -> Vec<(i64, String, Rec)> {
    let rows = match tx.scan(&opt) {
        Ok(rows) => rows,
        Err(_) => return Vec::new(),
    };
    rows.into_iter()
        .filter_map(|(k, v)| {
            let (parent, name) = keys.decode_name(&k)?;
            let rec = Rec::unpack(&v)?;
            Some((parent, name, rec))
        })
        .collect()
}

fn entries_in(tx: &Tx, keys: &Keys, parent: i64) -> Vec<(i64, String, Rec)> {
    scan_children(tx, keys, keys.children_range(parent))
}

/// The whole view's namespace in one pass, for `dump_tree` (§3.3's full-prefix
/// scan). Deliberately a separate function so that the only unbounded scan in the
/// backend is named, countable and reviewable.
fn scan_all_entries(tx: &Tx, keys: &Keys) -> Vec<(i64, String, Rec)> {
    scan_children(tx, keys, keys.all_names_range())
}

/// §3.2's authoritative-name repair: the remaining holder whose key sorts first.
///
/// The doc phrases the rule as "name 序最小者"; because the `'h` key carries
/// `parent_id` before `name` (C2), the choice here is `(parent_id, name)`-minimal.
/// Both are deterministic and both are refinements of PG's `LIMIT 1`, which has no
/// ORDER BY at all and so no defined answer — §12 already lists "getattr may
/// report a different name" as an attributable difference, and this narrows it.
fn first_holder(tx: &Tx, keys: &Keys, ino: i64) -> Result<Option<(i64, String)>, TxErr> {
    let holders = tx.scan(&keys.holder_prefix(ino))?;
    Ok(holders
        .first()
        .and_then(|(k, _)| keys.decode_holder(k))
        .map(|(_, parent, name)| (parent, name)))
}

/// The portable spelling of an entry's name, mapped to the name its KEY lives at.
///
/// The root row is exported the way BOTH SQL dumps export it: name "/" with a
/// NULL parent. "/" is precisely the one spelling that is not a legal directory
/// entry — it names the frozen root KEY `(ROOT_PARENT, "")` (§1) and nothing else
/// can reach that key — so every place a row is turned into a stored name goes
/// through this one rule rather than re-deciding it.
fn root_entry_name<'a>(parent: i64, name: &'a str) -> &'a str {
    if parent == ROOT_PARENT && name == "/" {
        ""
    } else {
        name
    }
}

// ---- writing a tree's rows ---------------------------------------------------
//
// Three callers write whole trees into a view subspace: `import_jsonl` (a
// migration dump), `fork_tree` (a copy of another view) and `graft_tree` (a
// subtree placed into an existing one). They must agree on every key family a
// row owns, on when the shared ledger is booked, on which name the `('i',ino)`
// pointer names and on how the allocator water marks move — so all four
// decisions live here once, and the callers differ only in which rows they hand
// over.

/// Mutable state one transaction's worth of rows leaves behind for the finish
/// pass: the inodes whose `('i',ino)` pointer must be (re)written, the
/// authoritative name of every directory written, and the id/ino high-water
/// marks §4's allocator continues from.
struct RowWriter {
    inos: std::collections::BTreeSet<i64>,
    /// ino -> the `(parent, stored_name)` its name key lives at, for directories:
    /// they have no holder rows, so their own name is their authoritative one.
    dir_entry_of: std::collections::HashMap<i64, (i64, String)>,
    max_id: i64,
    max_ino: i64,
}

impl RowWriter {
    fn new() -> Self {
        Self {
            inos: Default::default(),
            dir_entry_of: Default::default(),
            max_id: crate::meta::ROOT_INO,
            max_ino: crate::meta::ROOT_INO,
        }
    }
}

/// Write ONE entry — every key family the entry owns — inside the caller's
/// transaction.
///
/// The ledger rule is stated once here because it is the one that is easy to get
/// wrong: the shared counter carries one unit per content key per VIEW (§3.6),
/// never per name, so a view books a blob at the moment it gains its FIRST name
/// for it. Read in the same transaction, that test is `count_range` over this
/// view's own `'r` family: a second name in the same batch, a re-graft of the
/// same subtree and a hard link all see the key the first name wrote and book
/// nothing. The alternative (collect the batch's blobs and book them after the
/// rows land) cannot tell "already named before this call" from "named by this
/// call" and leaves a holder nobody ever unbooks.
fn put_row_tx(
    tx: &Tx,
    keys: &Keys,
    w: &mut RowWriter,
    parent: i64,
    name: &str,
    rec: &Rec,
) -> Result<(), TxErr> {
    let stored_name = root_entry_name(parent, name);
    tx.set(&keys.name(parent, stored_name), &rec.pack());
    tx.set(&keys.by_id(rec.id), &(parent, stored_name).pack_to_vec());
    if rec.is_dir {
        tx.set(&keys.dir_entry(parent, stored_name), b"");
        w.dir_entry_of.insert(rec.ino, (parent, stored_name.to_string()));
    } else {
        tx.set(&keys.holder(rec.ino, parent, stored_name), b"");
    }
    if let Some(dr) = &rec.data_ref {
        let first_name = tx.count_range(&keys.dataref_prefix_of(dr))? == 0;
        tx.set(&keys.dataref(dr, parent, stored_name), b"");
        if first_name {
            tx.bump(&keys.ledger(dr), 1)?;
        }
    }
    w.inos.insert(rec.ino);
    w.max_id = w.max_id.max(rec.id);
    w.max_ino = w.max_ino.max(rec.ino);
    Ok(())
}

/// The pass that must run AFTER the last [`put_row_tx`] of a transaction: the
/// `('i',ino)` pointers and §4's allocator continuation.
///
/// Directories are their own one-row inode, so their authoritative pointer comes
/// straight out of the rows in hand — through the SAME `(parent, stored_name)`
/// pair the name key was written with, because the root's portable name "/" is
/// not the key it lives at: pointing `('i',1)` at `(ROOT_PARENT, "/")` would make
/// `getattr` of the root read a key that does not exist, and the mount would
/// answer ENOENT for the whole tree while every other key is in place. Files take
/// `first_holder`, the smallest-`(parent,name)` rule `link_node` and `unlink`
/// apply.
fn finish_rows_tx(tx: &Tx, keys: &Keys, w: &RowWriter) -> Result<(), TxErr> {
    for ino in &w.inos {
        if let Some((p, nm)) = first_holder(tx, keys, *ino)? {
            tx.set(&keys.ino(*ino), &(p, nm.as_str()).pack_to_vec());
        } else if let Some((p, nm)) = w.dir_entry_of.get(ino) {
            tx.set(&keys.ino(*ino), &(*p, nm.as_str()).pack_to_vec());
        } else {
            return Err(TxErr::Hard(MetaError::Db(format!(
                "tree write rejected: inode {ino} has no directory entry to point at"
            ))));
        }
    }
    // §4's continuation: the water marks must not go backwards, and a create
    // after this write must not re-issue a written id. This is the
    // `setval(pg_get_serial_sequence(...), MAX(id))` of `pg_store.rs`.
    tx.raise_to_max(&keys.alloc(AllocKind::Id), w.max_id)?;
    tx.raise_to_max(&keys.alloc(AllocKind::Ino), w.max_ino)?;
    Ok(())
}

/// Write a whole tree's rows into an view subspace that is (or was just made)
/// empty. Rows are `(parent_id, name, record)` with the root's parent as `None`
/// and its portable name spelled "/".
fn write_rows_tx(
    tx: &Tx,
    keys: &Keys,
    rows: &[(Option<i64>, String, Rec)],
) -> Result<(), TxErr> {
    let mut w = RowWriter::new();
    for (parent, name, rec) in rows {
        put_row_tx(tx, keys, &mut w, parent.unwrap_or(ROOT_PARENT), name, rec)?;
    }
    finish_rows_tx(tx, keys, &w)
}

/// FDB bounds ONE transaction at 10 MB of *transaction state*, and its own
/// accounting is far heavier than the bytes a row occupies at rest — and it
/// charges what a transaction only READS as well as what it writes. Measured on
/// this encoding (FDB 7.1 at the default limit), each cell the last one to commit
/// / the first refused:
///
/// | one transaction                                   | commits     | `2101` from |
/// |---------------------------------------------------|-------------|-------------|
/// | `import`, one blob ref per row                     | 7,700 rows  | 7,900 rows  |
/// | `import`, no blob refs                             | 15,000 rows | 17,000 rows |
/// | `graft` 6,000 rows into a 9,500-row tree            | fits        | 10,000 rows |
///
/// One model fits all three: ≈625 B per row written, ≈660 B per blob booked and
/// ≈240 B per row the transaction merely scans. That is two to three times the
/// 194 B (no blob) / 292 B (one blob) the same rows occupy on disk once written,
/// which is why a row-count ceiling was the wrong shape — at 292 B/row it would
/// have promised 34,000 rows and aborted at 7,800.
const TX_LIMIT_BYTES: usize = 10_000_000;
const TX_ROW_BYTES: usize = 625;
const TX_BLOB_BYTES: usize = 660;
const TX_READ_ROW_BYTES: usize = 240;

/// Budget ONE written transaction may use when a whole-tree write is split
/// across several: 70% of the hard limit, not 100%, because the cost model above
/// is measured rather than documented (FDB does not publish its per-key
/// accounting) — a chunk that lands 20% above its estimate must still commit.
const TX_CHUNK_BUDGET: usize = TX_LIMIT_BYTES * 7 / 10;

/// Lengths of the transactions one whole-tree write becomes: the first may be
/// smaller because its transaction also READS the destination (`read_rows`),
/// every later one only writes. `blobs` is the number of ledger units the plan
/// books in total, so the tail answers "how many rows at this density".
///
/// Never returns a zero length, and always sums to `rows` — the callers slice by
/// it, so an off-by-one would either drop a row or panic.
fn chunk_lengths(rows: usize, blobs: usize, read_rows: usize) -> Vec<usize> {
    if rows == 0 {
        return Vec::new();
    }
    let blob_per_row = blobs.saturating_mul(TX_BLOB_BYTES).div_ceil(rows);
    let per_row = TX_ROW_BYTES.saturating_add(blob_per_row).max(1);
    let cap_rest = (TX_CHUNK_BUDGET / per_row).max(1);
    let cap_first = (TX_CHUNK_BUDGET
        .saturating_sub(read_rows.saturating_mul(TX_READ_ROW_BYTES))
        / per_row)
        .max(1);
    let mut out = Vec::new();
    let mut left = rows;
    let mut first = true;
    while left > 0 {
        let n = left.min(if first { cap_first } else { cap_rest });
        out.push(n);
        left -= n;
        first = false;
    }
    out
}

/// Split a write list into "everything but the root" plus the root row, so the
/// caller can put the root LAST. The root entry is `(None, "/", _)` in the
/// portable spelling and `(None, "", _)` when it came from a scan; both are
/// recognised by the null parent, which is the only row that has one.
fn root_last(
    rows: Vec<(Option<i64>, String, Rec)>,
) -> (Vec<(Option<i64>, String, Rec)>, Vec<(Option<i64>, String, Rec)>) {
    let mut data = Vec::with_capacity(rows.len());
    let mut root = Vec::new();
    for r in rows {
        if r.0.is_none() {
            root.push(r);
        } else {
            data.push(r);
        }
    }
    (data, root)
}

/// Structural validation of an import dump, on the rows alone: exactly one root
/// row, a root that is `ROOT_INO` and a directory, no dangling parent, and — when
/// the header claims counts — the counts it claims. Pure on purpose: a malformed
/// dump must write nothing whether the write is one transaction or several, so
/// every check that needs no cluster read happens before the first transaction.
fn validate_import_rows(
    rows: &[(Option<i64>, String, Rec)],
    claimed_nodes: Option<i64>,
) -> Result<(), String> {
    let roots: Vec<&(Option<i64>, String, Rec)> =
        rows.iter().filter(|(p, _, _)| p.is_none()).collect();
    if roots.len() != 1 {
        return Err(format!(
            "import rejected: expected exactly one root row (parent_id null), found {}",
            roots.len()
        ));
    }
    if roots[0].2.id != crate::meta::ROOT_INO || !roots[0].2.is_dir {
        return Err(format!(
            "import rejected: root row must be id {} and a directory (got id {}, is_dir {})",
            crate::meta::ROOT_INO,
            roots[0].2.id,
            roots[0].2.is_dir
        ));
    }
    let mut dirs: std::collections::HashMap<i64, bool> = Default::default();
    for (_, _, r) in rows {
        dirs.entry(r.id).and_modify(|d| *d |= r.is_dir).or_insert(r.is_dir);
    }
    let mut dangling = 0usize;
    for (p, _, _) in rows {
        let Some(p) = p else { continue };
        if dirs.get(p).copied() != Some(true) {
            dangling += 1;
        }
    }
    if dangling != 0 {
        return Err(format!(
            "import rejected: {dangling} node(s) point at a missing or non-directory parent"
        ));
    }
    if let Some(c) = claimed_nodes {
        if c != rows.len() as i64 {
            return Err(format!(
                "import rejected: header declares {c} node(s), file carries {}",
                rows.len()
            ));
        }
    }
    Ok(())
}

/// The cluster-side import gate: corpse refusal, "already holds a tree" refusal,
/// and — when `teardown` — the old tree's ledger unwind plus the range clear.
///
/// `teardown` is false for a SPLIT import, which unwinds through
/// [`FdbStore::unbook_refs`] in 256-ref transactions instead: one transaction
/// touching a million ledger keys is the same 10 MB wall this split exists to
/// avoid, so a replace that does not fit one transaction cannot use the inline
/// unwind that the single-transaction path gets for free.
///
/// The one shape `--replace` adopts but `teardown` cannot serve is a CORPSE: its
/// leftovers can be many transactions' worth of rows, so the gate answers `true`
/// and lets the caller's chunked unwind do the work — see the branch below. The
/// caller forces the split branch for this shape, so a corpse never reaches the
/// refusal below when `replace` is set.
fn prep_import_target(
    tx: &Tx,
    keys: &Keys,
    view_name: &str,
    replace: bool,
    teardown: bool,
) -> Result<bool, TxErr> {
    // A subspace that holds keys and NO root row is a CORPSE (`view scan`
    // names that kind): it counts 0 entries, so the "already holds a tree"
    // test below would not fire and the import would seed a fresh root on
    // the leftovers. Measured, not hypothesised: importing onto a corpse
    // succeeded, the corpse's 9 keys stayed put, the import merely added
    // its own rows (13 keys / 2 entries) and the shared ledger still
    // carried BOTH the old and the new ref — so the imported tree claims
    // bytes it does not own, and the inherited rows keep the old ref
    // pinned to this view as a held claim no later pass can tell apart.
    // `view create` refuses this same shape for the same
    // reason (seeding a root over leftovers resurrects a corpse). `import`
    // refuses it too, unless `--replace` can adopt it through the CHUNKED
    // unwind: replacing presupposes rows whose ledger bookings can be given
    // back, and a corpse's can — it just takes 256 refs per transaction.
    let mut probe = keys.all_range();
    probe.limit = Some(1);
    let has_keys = !tx.get_range(&probe)?.is_empty();
    let has_root = tx.get(&keys.name(ROOT_PARENT, ""))?.is_some();
    if has_keys && !has_root {
        if replace && !teardown {
            // `--replace` DOES adopt a corpse — that is how a split import
            // interrupted before its last transaction gets continued — but only
            // through the chunked unwind (`teardown == false`), which is why the
            // caller forces the split branch for this shape. Answering `true` is
            // what tells that caller to run `booked_refs` + `unbook_refs` +
            // `clear_view_prefix` before the first row.
            return Ok(true);
        }
        // Reaching here means the caller did not ask to replace, so the leftovers
        // have to go first — and they may be several transactions' worth.
        return Err(TxErr::Hard(MetaError::Db(format!(
            "target subspace '{view_name}' holds keys but no root row (a corpse — \
             nothing can mount it); importing would seed a new root on the old \
             tree's leftovers and inherit its booked byte references. clear it \
             first with `grovefs view --meta-store fdb://… rm --name {view_name} \
             --base`, or pass --replace to have the import clear it"
        ))));
    }
    // Refuse-not-downgrade on the target, same test the SQLite import
    // makes with `dst.exists()`: here a healthy subspace always holds the
    // root row, so "a tree is already there" means "more than the root" —
    // NOT "holds keys", which is also true of the root row alone.
    let existing = tx.count_range(&keys.all_names_range())?;
    let replaced = existing > 1;
    if replaced && !replace {
        return Err(TxErr::Hard(MetaError::Db(
            "target view already holds a tree — pass --replace to overwrite it \
             (stop every mount/meta-server using this backend first)"
                .to_string(),
        )));
    }
    if replaced && teardown {
        // Release the old tree's ledger units first, in the SAME
        // transaction that tears it down: one unit per content key per
        // view (§3.6), so the distinct refs of the `'r` family are what
        // this view has booked. The re-add for the imported rows happens
        // below, and a retry of the whole body sees the same old tree
        // again because none of this commits until it succeeds.
        let mut released: std::collections::HashSet<String> = Default::default();
        for (k, _) in tx.scan(&keys.dataref_all_range())? {
            if let Some(dr) = keys.decode_dataref(&k) {
                if released.insert(dr.clone()) {
                    // Zero holders is not a count, it is the absence of a
                    // booking — the same "delete the row at zero" the
                    // unlink path and `remove_view` do. Leaving it was
                    // reachable: a replace-import over a tree that was a
                    // ref's last holder left the row at 0, and a second
                    // replace over that same view bumped it to -1. The
                    // row then outlived every tree in the cluster —
                    // measured as one `refs` key whose value decoded to
                    // -1 with no view beside it, i.e. a booking no tree
                    // could ever release again.
                    let left = tx.bump(&keys.ledger(&dr), -1)?;
                    if left <= 0 {
                        tx.clear(&keys.ledger(&dr));
                    }
                }
            }
        }
        tx.clear_prefix(keys.view_prefix().bytes())?;
    }
    Ok(replaced)
}

/// §3.7's lock value tuple `(ltype, pid, sid, expires_at)`.
///
/// `expires_at` is a VALUE and deliberately never part of the primary key: that
/// is what makes a lease renewal a value update plus one `'lx` key swap instead
/// of moving the lock's identity every `lease/3` seconds. The `ltype` code is the
/// wire/SQLite code (`LockType::to_u8`) so a lock row means the same thing in all
/// three backends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LockVal {
    ltype: i64,
    pid: i64,
    sid: i64,
    expires: i64,
}

impl LockVal {
    fn pack(&self) -> Vec<u8> {
        (self.ltype, self.pid, self.sid, self.expires).pack_to_vec()
    }

    fn unpack(bytes: &[u8]) -> Option<LockVal> {
        let items = Vec::<Element>::unpack_root(bytes).ok()?;
        let i = |k: usize| -> Option<i64> { items.get(k)?.as_i64() };
        Some(LockVal {
            ltype: i(0)?,
            pid: i(1)?,
            sid: i(2)?,
            expires: i(3)?,
        })
    }
}

/// One lock: its primary key's four columns and its value, so that a single
/// decoded row carries everything an overlap decision reads and everything a
/// four-key delete needs.
#[derive(Clone, Copy, Debug)]
struct LockRow {
    ino: i64,
    start: i64,
    end: i64,
    owner: i64,
    val: LockVal,
}

/// The overlap test PG spells `l_start<=$3 AND l_end>=$4` while passing the
/// caller's `(s, e)` in the OPPOSITE order — the parameters are crossed so the
/// condition reads "the stored range reaches into the requested one". Named once,
/// because three of the four lock paths use it and a copy that drifts is a
/// silently wrong `fcntl` compatibility ruling, not a visible error.
fn locks_overlap(row_start: i64, row_end: i64, s: i64, e: i64) -> bool {
    row_start <= e && row_end >= s
}

/// Write a lock and its three existence mirrors. FDB has no automatic index
/// maintenance, so "keep the mirrors" is explicit work here — the price paid once
/// per grant for the sweep, release and renewal paths each being a prefix scan.
fn put_lock_tx(tx: &Tx, keys: &Keys, r: &LockRow) {
    tx.set(&keys.lock(r.ino, r.start, r.end, r.owner), &r.val.pack());
    tx.set(
        &keys.lock_expire(r.val.expires, r.ino, r.start, r.end, r.owner),
        b"",
    );
    tx.set(&keys.lock_owner(r.owner, r.ino, r.start, r.end), b"");
    tx.set(
        &keys.lock_sid(r.val.sid, r.ino, r.start, r.end, r.owner),
        b"",
    );
}

/// The same set, removed. Every path that ends a lock goes through here so that
/// "a lock exists in exactly one family" is impossible by construction — which is
/// the pitfall §3.7 names: an owner whose locks are not booked on BOTH the owner
/// and the session axis leaks them past the close and past the lease.
fn delete_lock_tx(tx: &Tx, keys: &Keys, r: &LockRow) {
    tx.clear(&keys.lock(r.ino, r.start, r.end, r.owner));
    tx.clear(&keys.lock_expire(r.val.expires, r.ino, r.start, r.end, r.owner));
    tx.clear(&keys.lock_owner(r.owner, r.ino, r.start, r.end));
    tx.clear(&keys.lock_sid(r.val.sid, r.ino, r.start, r.end, r.owner));
}

/// §3.7's step ①: every lock whose lease ran out, as ONE bounded `'lx` range,
/// each hit taking all four of its keys with it. The primary's value is read
/// because `sid` lives only there; a mirror pointing at a row that is already
/// gone is dropped on the spot so the next sweep does not revisit it.
///
/// Counted into `LockExpireReclaim` the way the SQLite arm counts it, so the
/// stats surface does not go quiet just because the backend changed.
fn sweep_expired_tx(tx: &Tx, keys: &Keys, now: i64) -> Result<u64, TxErr> {
    let mut n = 0;
    for (k, _) in tx.scan(&keys.lock_expired_upto(now))? {
        let Some((expires, ino, start, end, owner)) = keys.decode_lock_expire(&k) else {
            return Err(TxErr::Hard(keys_corrupt(&k)));
        };
        let pk = keys.lock(ino, start, end, owner);
        if let Some(v) = tx.get(&pk)? {
            if let Some(mut val) = LockVal::unpack(&v) {
                // The delete carries the expiry FROM THE KEY, not from the value:
                // the key scanned is the key that must disappear, so a healthy
                // view clears the same bytes twice and an inconsistent one cannot
                // strand a `'lx` row that would be swept again forever.
                val.expires = expires;
                delete_lock_tx(
                    tx,
                    keys,
                    &LockRow {
                        ino,
                        start,
                        end,
                        owner,
                        val,
                    },
                );
                n += 1;
                continue;
            }
        }
        tx.clear(&k);
    }
    if n > 0 {
        crate::stats::count(crate::stats::Counter::LockExpireReclaim, n);
    }
    Ok(n)
}

/// This inode's lock rows, decoded, expired ones already skipped by the caller's
/// sweep. The filter is applied here as well rather than trusted: an expired
/// lease is not a blocker, and treating it as one would make a caller wait on a
/// lock nobody holds.
fn lock_rows_of_ino(tx: &Tx, keys: &Keys, ino: i64, now: i64) -> Result<Vec<LockRow>, TxErr> {
    let mut out = Vec::new();
    for (k, v) in tx.scan(&keys.lock_ino_prefix(ino))? {
        let Some((row_ino, start, end, owner)) = keys.decode_lock(&k) else {
            return Err(TxErr::Hard(keys_corrupt(&k)));
        };
        debug_assert_eq!(row_ino, ino, "lock scan escaped its inode prefix");
        let Some(val) = LockVal::unpack(&v) else {
            return Err(TxErr::Hard(keys_corrupt(&k)));
        };
        if val.expires < now {
            continue;
        }
        out.push(LockRow {
            ino,
            start,
            end,
            owner,
            val,
        });
    }
    Ok(out)
}

/// Delete one NAME — the row it presents plus every secondary key that embeds it
/// — inside the caller's transaction, and report whether its content key became
/// unreferenced. Shared by `unlink` and `rename`'s overwrite branch on purpose:
/// POSIX gives both the same delete semantics (a non-empty directory is
/// `ENOTEMPTY`, the last name of a key releases bytes, the penultimate name must
/// not), and a second copy of that rule is how backends drift apart.
///
/// Returns `Some(data_ref)` when the object may be collected. The last ledger
/// read happens in the SAME snapshot as the clears above it, which is §3.5's whole
/// argument for existence keys over counters, and mirrors
/// `pg_store.rs:1082-1084`.
fn drop_name_tx(
    tx: &Tx,
    keys: &Keys,
    parent: i64,
    name: &str,
    rec: &Rec,
) -> Result<Option<String>, TxErr> {
    if rec.is_dir && !tx.get_range(&keys.children_range(rec.id))?.is_empty() {
        return Err(MetaError::NotEmpty.into());
    }
    tx.clear(&keys.name(parent, name));
    tx.clear(&keys.by_id(rec.id));
    if rec.is_dir {
        tx.clear(&keys.dir_entry(parent, name));
    } else {
        tx.clear(&keys.holder(rec.ino, parent, name));
    }
    if let Some(dr) = &rec.data_ref {
        tx.clear(&keys.dataref(dr, parent, name));
        // Still held by another name in THIS view -> the shared ledger is
        // untouched; the last name drops one booking.
        if tx.count_range(&keys.dataref_prefix_of(dr))? == 0 {
            let left = tx.bump(&keys.ledger(dr), -1)?;
            if left <= 0 {
                tx.clear(&keys.ledger(dr));
            }
        }
    }
    // §3.2: the `ino` key holds ONE authoritative name, so deleting it must hand
    // the pointer to the next remaining name (the deterministic choice §3.2
    // documents), or every later getattr on that inode 404s while the file still
    // exists. A directory has no `'h` keys and one name, so its pointer is simply
    // dropped with the row.
    if !rec.is_dir {
        match first_holder(tx, keys, rec.ino)? {
            Some((p, nm)) => {
                tx.set(&keys.ino(rec.ino), &(p, nm.as_str()).pack_to_vec());
            }
            None => tx.clear(&keys.ino(rec.ino)),
        }
    }
    // GC decision mirrors pg_store's `ledger_remove_tx`: no ledger row at all
    // means the object was never booked against a shared key, so it is released
    // exactly as the pre-ledger engine released every private one.
    let freed = match &rec.data_ref {
        Some(dr) if tx.get(&keys.ledger(dr))?.is_none() => Some(dr.clone()),
        _ => None,
    };
    Ok(freed)
}

/// §3.9's cycle check, the counterpart of `pg_store.rs:1414`'s `is_descendant_tx`.
///
/// PG asks the row for its `parent_id` column; FDB has no row-by-id value, so each
/// hop reads `'d'`(id), whose value is that row's `(parent_id, name)` — the parent
/// id falls straight out, no second read. The walk starts at `new_parent` and
/// climbs to the root, whose by-id key names `(ROOT_PARENT, "")`; `by_id(0)` does
/// not exist, and the miss is the terminating condition — exactly PG's
/// `parent_id IS NULL` expressed as a key that is not there.
///
/// 4096 hops is PG's cap (`pg_store.rs:1421`) kept verbatim so a pathological tree
/// fails the same way on both backends. The cap returning `false` is deliberate
/// (PG does the same): depth past 4096 is not a legal tree, and guessing "cycle"
/// would refuse a rename that POSIX allows.
fn is_descendant_tx(tx: &Tx, keys: &Keys, mut cur: i64, src_id: i64) -> Result<bool, TxErr> {
    for _ in 0..4096 {
        if cur == src_id {
            return Ok(true);
        }
        let Some(raw) = tx.get(&keys.by_id(cur))? else {
            return Ok(false);
        };
        let Some((parent, _)) = unpack_ptr(&raw) else {
            return Ok(false);
        };
        // A by-id key that names itself as its parent is corruption, not a chain;
        // stopping turns that into a refused rename rather than a loop.
        if parent == cur {
            return Ok(false);
        }
        cur = parent;
    }
    Ok(false)
}

// ---- the store -------------------------------------------------------------

/// `READDIR_CURSOR_STRIDE` / `READDIR_CURSOR_CAP` in `pg_store.rs:165-170`: the
/// same checkpoints mean the two backends page a large directory with the same
/// cost profile, so a benchmark difference stays attributable (§5 of the doc).
const READDIR_CURSOR_STRIDE: usize = 32;
const READDIR_CURSOR_CAP: usize = 65_536;

/// How many range counts one [`Tx::count_multi`] wave keeps in flight. Each unit
/// is a separate client request; 256 hides the round-trip without swamping the
/// network thread of a single-node dev cluster.
const COUNT_MULTI_FANOUT: usize = 256;

/// A view's handle on the FDB-backed tree.
pub struct FdbStore {
    view: String,
    keys: Keys,
    db: DbHandle,
    /// Live fencing epoch, mirrored in-memory the way the PG engine reads it at
    /// open: the data plane stamps writes with it, so it must be readable without
    /// a round trip.
    fence: ArcFence,
    /// Ids reserved from `(v,'m','id')` but not yet handed out (§4).
    blk: std::sync::Mutex<Block>,
    /// §3.3's readdir paging hints: `(parent, stride index) -> name to resume
    /// after`. Wrong hints cost time, never correctness, which is what makes
    /// clearing the whole map on a name change (`drop_cursors`) safe.
    cursors: std::sync::Mutex<std::collections::HashMap<(i64, usize), String>>,
    last_touch: AtomicU64,
}

/// Process-local view of one allocator block: `[next, limit)`.
struct Block {
    next: i64,
    limit: i64,
}

/// `Arc<AtomicU64>` under a name, because the data plane holds a clone and must be
/// able to read the epoch without touching the store.
type ArcFence = std::sync::Arc<AtomicU64>;

// ---- cluster inventory (operator surface) -----------------------------------
//
// Every key this backend writes lives under ONE root, `(\x01,"grovefs",\x00)`,
// and the component after it is the view name — except `refs`, which is the
// shared ledger (§3.6). Nothing else is a sibling, so walking the root's
// immediate children is the whole inventory: the FDB counterpart of listing
// `meta/*.db` (SQLite) or `grovefs_v_*` schemas (PG). Before this existed, a
// subspace could be CREATED (`view create`) but neither listed nor deleted, so a
// scratch view (a test run, an abandoned migration) was invisible and permanent.

/// Bytes requested per page while walking a prefix.
///
/// Rows per page are NOT a knob that can be set here: FoundationDB caps a range
/// reply by BYTES, so `limit = 1000` does not mean "a thousand rows or the end"
/// — a 4 KiB batch of these keys is about sixty of them. Measured the hard way:
/// the first version of this walk stopped when a page came back short of its
/// `limit` and reported a 2000-key cluster as 58 keys. The walk therefore asks
/// for more bytes per round trip and stops when the driver says `more() == false`
/// (`RangeOption::next_range`, the same continuation [`Tx::scan`] uses).
const PAGE_BYTES: usize = 64 * 1024;

/// One child of the `grovefs` root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubspaceInfo {
    /// The tuple component after the root: a view name, or `refs`.
    pub name: String,
    /// Every key under the prefix.
    pub keys: u64,
    /// `'n'` rows — the entries the tree actually holds (the root row included).
    pub entries: u64,
    /// True for the shared cross-view ledger, which is not a mountable tree.
    pub ledger: bool,
    /// True for the cluster's writer-lease table, which is a sibling of the views
    /// (one row per deployment) and not a tree either. Without this flag a lease
    /// row has no root row and would be reported as a `corpse` — an invitation to
    /// run `view rm --base` on the lease table.
    pub lease: bool,
    /// True for the API-key/call-audit tables, the third root sibling. Same
    /// argument as `lease`: they hold no root row, so without their own flag they
    /// would read as corpses and invite a `view rm --base` that would delete the
    /// only credentials able to govern the cluster.
    pub ctl: bool,
    /// Did the subspace still hold its root row, `('n', ROOT_PARENT, '')`?
    ///
    /// A mount OPENS the root row, so a subspace that has keys but has lost it can
    /// never be mounted again — and yet every content key it names is still
    /// booked in the shared ledger. Measured, not hypothesised: the local cluster
    /// held `spike` with 7065 keys and 2001 entries but no root row (a partial
    /// `clearrange` of the `'n` family leaves exactly that shape), which is the
    /// case that made a removal predicate of "has a root row" wrong.
    pub root_row: bool,
    /// The prefix in hex, so an operator can hand it to `fdbcli clearrange`
    /// without re-deriving the tuple encoding.
    pub prefix_hex: String,
}

impl SubspaceInfo {
    /// `ledger` for the shared `refs` subspace, `lease`/`control` for the two
    /// other root siblings, `corpse` for a tree that can no longer be mounted,
    /// `view` for a live tree, `?` for a name this build cannot reproduce
    /// (reported by hex alone rather than hidden).
    pub fn kind(&self) -> &'static str {
        if self.ledger {
            "ledger"
        } else if self.lease {
            "lease"
        } else if self.ctl {
            "control"
        } else if !crate::layout::valid_view_name(&self.name) {
            "?"
        } else if !self.root_row {
            "corpse"
        } else {
            "view"
        }
    }
}

/// What [`FdbStore::remove_view`] did. Reported rather than logged because the
/// numbers are the operator's only evidence that the ledger was unwound rather
/// than merely the keys deleted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PurgeReport {
    pub view: String,
    /// Keys removed by the range clear.
    pub keys: u64,
    /// Entries the tree held when the removal started.
    pub entries: u64,
    /// Data refs whose ledger count reached 0: no surviving view names those
    /// bytes any more, so the data servers may reclaim them.
    pub unbooked: usize,
    /// The same refs by name, so a caller that owns a data root can free them
    /// (the CLI's `view rm --base`) instead of only reporting a number.
    pub unbooked_refs: Vec<String>,
    /// Data refs another view still names — booked down by one, kept.
    pub still_held: usize,
}

/// Hex spelling of a key prefix, for copy-paste `fdbcli` commands.
fn hex_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// The immediate child of `root` that `key` lives under, plus how many bytes that
/// child's name took: `Some(("alice", 8))` for the key
/// `root \x02alice\x00 \x02n\x00 \x14\x02 \x00`.
///
/// `Subspace::unpack` cannot answer this, and the reason is worth writing down
/// because the first attempt at this function used it and was wrong on EVERY key:
/// `unpack` insists on consuming the whole key (`TupleUnpack::unpack_root` returns
/// `TrailingBytes` otherwise, `foundationdb-tuple-0.11`'s `pack.rs:110-116`), and
/// a real key carries a family tag and a tuple tail after the name. The crate
/// has no public partial decode either (`TupleDepth::new` is private), so the
/// first component is read here and then **checked against the encoder**:
/// `root.subspace(&name)` must reproduce exactly the bytes consumed. That check
/// is what makes the hand-decoding safe — a different typecode, a mis-handled
/// escape or a name this backend could not have written all fail it and are
/// reported as undecodable instead of being silently filed under a wrong name.
///
/// The escape rule is the tuple layer's own (`pack.rs:169-200`, `is_end_of_tuple`):
/// a literal NUL inside a string is written `\x00\xff`, so a `\x00` followed by
/// `\xff` is data and anything else after `\x00` means the string ended.
fn child_name(root: &Subspace, key: &[u8]) -> Option<(String, usize)> {
    let base = root.bytes();
    let rest = key.strip_prefix(base)?;
    // STRING (0x02) is the typecode this backend writes for a view name (§1).
    if rest.first() != Some(&0x02) {
        return None;
    }
    let mut raw: Vec<u8> = Vec::new();
    let mut i = 1usize;
    loop {
        let b = *rest.get(i)?;
        i += 1;
        if b == 0x00 {
            if rest.get(i) == Some(&0xff) {
                i += 1;
                raw.push(0x00);
                continue;
            }
            break;
        }
        raw.push(b);
    }
    let name = String::from_utf8(raw).ok()?;
    // The verification that turns a decoder into a fact.
    if root.subspace(&name).bytes() != &key[..base.len() + i] {
        return None;
    }
    Some((name, i))
}

/// Both bounds of the whole `grovefs` root: inclusive start, exclusive end.
fn root_bounds() -> (Vec<u8>, Vec<u8>) {
    let prefix = Keys::root().bytes().to_vec();
    let end = prefix_upper_bound(&prefix)
        .expect("the root prefix is 0x01 \"grovefs\" 0x00 — it never ends in 0xff");
    (prefix, end)
}

/// Visit every key in `[begin, end)` once, paging with a fresh read-only
/// (snapshot) transaction per page. Returns how many keys were visited.
///
/// A single transaction would be the tidier code and the wrong one: FDB caps a
/// transaction's lifetime at five seconds, and "count the keys in a view" is
/// exactly the query that grows without bound (the whole point of §9's per-entry
/// size table). Paging by key also means the walk sees a stable order, so a
/// concurrent writer can add keys but cannot make the walk revisit one.
fn for_each_key<F>(
    db: &DbHandle,
    begin: Vec<u8>,
    end: Vec<u8>,
    f: &mut F,
) -> Result<u64, MetaError>
where
    F: FnMut(&[u8]) -> Result<(), MetaError>,
{
    let mut visited = 0u64;
    let mut next: Option<RangeOption> = Some(
        (
            KeySelector::first_greater_or_equal(begin),
            KeySelector::first_greater_or_equal(end),
        )
            .into(),
    );
    while let Some(mut opt) = next {
        opt.target_bytes = PAGE_BYTES;
        let txn = data_trx(&db).map_err(|e| MetaError::Db(format!("fdb scan: {e}")))?;
        let kvs = futures::executor::block_on(txn.get_range(&opt, 1, true))
            .map_err(|e| MetaError::Db(format!("fdb scan: {e}")))?;
        for kv in kvs.as_ref().iter() {
            f(kv.key())?;
            visited += 1;
        }
        next = opt.next_range(&kvs);
    }
    Ok(visited)
}

/// Every subspace under the `grovefs` root in this cluster, by name.
///
/// This is the answer to "what is actually in there", which `view list` does not
/// give: `view list` is the governance catalog (the definitions in the cluster's
/// `ctl` subspace and the base names they resolve to), while this walks the
/// cluster's physical subspaces with key/entry counts. The two are different
/// questions on purpose — reading the physical inventory is what makes an orphan
/// (a subspace no definition names) visible at all.
///
/// Entries are counted from the `'n'` family, so they mean the same thing the
/// export header means by `nodes`, and `refs` is reported as a ledger rather than
/// a tree (`entries` there is meaningless and left at 0).
pub fn list_subspaces(uri: &str) -> Result<Vec<SubspaceInfo>, MetaError> {
    let cluster_file = FdbConfig::parse_cluster(uri)
        .map_err(|e| MetaError::Db(format!("fdb:// inventory: {e}")))?;
    let db = database(&cluster_file)
        .map_err(|e| MetaError::Db(format!("fdb:// inventory: {e}")))?;
    let root = Keys::root();
    let (begin, end) = root_bounds();
    let mut found: std::collections::BTreeMap<String, SubspaceInfo> = Default::default();
    let mut visit = |key: &[u8]| -> Result<(), MetaError> {
        let base = root.bytes().len();
        let (name, cut) = match child_name(&root, key) {
            Some((n, used)) => (n, base + used),
            // A key this build cannot name is not silently dropped: it is
            // reported by its own bytes, because "there are keys here whose name
            // this build cannot reproduce" is exactly what an inventory is for.
            None => (format!("?{}", hex_bytes(key)), key.len()),
        };
        let rest = &key[cut..];
        let e = found.entry(name.clone()).or_insert_with(|| SubspaceInfo {
            ledger: name == TAG_REFS,
            lease: name == TAG_WRITER,
            ctl: name == TAG_CTL,
            name,
            keys: 0,
            entries: 0,
            root_row: false,
            prefix_hex: hex_bytes(&key[..cut]),
        });
        e.keys += 1;
        // `\x02n\x00` is the packed family tag (`'n'`), so this counts the same
        // rows `dump_tree`/`export` call nodes.
        if rest.starts_with(b"\x02n\x00") {
            e.entries += 1;
            // Both facts an operator needs come from the same walk: WHICH trees
            // are there, and which of them can never be mounted again. The root
            // row is an `'n'` row like any other, so the check costs one packed
            // key per entry and nothing extra per byte of the tree.
            if e.name != TAG_REFS
                && !e.name.starts_with('?')
                && Keys::for_view(&e.name).name(ROOT_PARENT, "") == key
            {
                e.root_row = true;
            }
        }
        Ok(())
    };
    for_each_key(&db, begin, end, &mut visit)?;
    Ok(found.into_values().collect())
}

/// The NAME of every tree under the `grovefs` root that can still be mounted,
/// without counting anybody's keys.
///
/// [`list_subspaces`] answers "what is in this cluster, and how big is each part",
/// and pays for it by visiting every key. The question `view list` asks is much
/// narrower — "which trees exist" — so this walk takes ONE key per subspace: read
/// a single-key page, decode the component after the root, then JUMP the range
/// past that whole subtree. Each candidate is then probed for its root row, which
/// is exactly the test [`FdbStore::subspace_exists`] applies, so a name reported
/// here is a name that mounts.
///
/// The root's three non-tree siblings (`refs`, `writer`, `ctl`) need no exclusion
/// list: they hold no root row, so the probe drops them by the same rule that
/// keeps a `corpse` out of the answer.
pub fn list_tree_names(uri: &str) -> Result<Vec<String>, MetaError> {
    let cluster_file = FdbConfig::parse_cluster(uri)
        .map_err(|e| MetaError::Db(format!("fdb:// inventory: {e}")))?;
    let db = database(&cluster_file)
        .map_err(|e| MetaError::Db(format!("fdb:// inventory: {e}")))?;
    let root = Keys::root();
    let (mut begin, end) = root_bounds();
    let mut names: Vec<String> = Vec::new();
    while begin < end {
        let opt = RangeOption {
            begin: KeySelector::first_greater_or_equal(begin.clone()),
            end: KeySelector::first_greater_or_equal(end.clone()),
            limit: Some(1),
            ..RangeOption::default()
        };
        let txn = data_trx(&db).map_err(|e| MetaError::Db(format!("fdb scan: {e}")))?;
        let kvs = futures::executor::block_on(txn.get_range(&opt, 1, true))
            .map_err(|e| MetaError::Db(format!("fdb scan: {e}")))?;
        let Some(kv) = kvs.as_ref().iter().next() else {
            break;
        };
        let key = kv.key().to_vec();
        match child_name(&root, &key) {
            Some((name, used)) => {
                names.push(name);
                // Jump past this child's whole subtree. Without the jump this
                // would be a one-key-per-key walk spelled as a loop.
                begin = prefix_upper_bound(&key[..root.bytes().len() + used])
                    .unwrap_or_else(|| key.clone());
            }
            // A subspace this build cannot name cannot be a mountable view
            // either; advance by one key so the walk still terminates.
            None => begin = key,
        }
    }
    // One batched root-row probe: `has_physical` asks each name the same
    // question, so `list` and `has` cannot disagree about what a tree is.
    let probes: Vec<Vec<u8>> = names
        .iter()
        .map(|n| Keys::for_view(n).name(ROOT_PARENT, ""))
        .collect();
    let txn = data_trx(&db).map_err(|e| MetaError::Db(format!("fdb scan: {e}")))?;
    let futs = probes.iter().map(|k| txn.get(k, true)).collect::<Vec<_>>();
    let found = futures::executor::block_on(futures::future::join_all(futs));
    let mut out = Vec::new();
    for (name, row) in names.into_iter().zip(found) {
        let row = row.map_err(|e| MetaError::Db(format!("fdb scan: {e}")))?;
        if row.is_some() && crate::layout::valid_view_name(&name) {
            out.push(name);
        }
    }
    out.sort();
    Ok(out)
}

/// What [`rebuild_ledger`] changed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LedgerRebuild {
    /// Subspaces walked (the shared ledger excluded).
    pub trees: usize,
    /// DISTINCT content keys the surviving trees name — the ledger's true size.
    pub refs: usize,
    /// Rows that disagreed with the recount, rewritten.
    pub rows_fixed: usize,
    /// Rows no surviving tree names any more, deleted.
    pub rows_deleted: usize,
    /// Total holder units after the rewrite (the sum of all counts).
    pub holders: i64,
}

/// A view name used only to obtain a transaction handle: [`Tx::permits`] allows the
/// shared ledger for ANY view's `Keys`, and this name is never written to.
const REBUILD_PROBE_VIEW: &str = "refs-rebuild";
/// Rows per transaction. A cluster's ledger can be far larger than one
/// transaction may touch, and each chunk is independently committed, so an
/// interrupted rebuild is re-runnable: the next run recounts from the trees.
const REBUILD_CHUNK: usize = 256;

/// The two READ halves of a ledger recount, with nothing written: what the
/// SURVIVING TREES still name (one holder unit per view, the same rule both
/// SQL rebuilds apply) and what the shared ledger currently books.
///
/// Split out because two callers need exactly this view of the cluster —
/// `refs rebuild`, which then rewrites the difference, and the control-plane
/// `gc` audit, which must never rewrite anything. Both inherit the same
/// refusal semantics: an undecodable key or an unwalkable tree ABORTS instead
/// of being skipped, because skipping lowers a count and lowering is the unsafe
/// direction (the next unlink would free bytes a live name still reads).
#[derive(Clone, Debug, Default)]
pub struct LedgerPicture {
    /// Subspaces that still exist (the shared ledger excluded).
    pub trees: std::collections::BTreeSet<String>,
    /// Content key → number of views naming it.
    pub refs: std::collections::BTreeMap<String, i64>,
    /// Content key → holder count currently booked (zeros included).
    pub booked: std::collections::BTreeMap<String, i64>,
}

/// Read a [`LedgerPicture`] from the cluster.
///
/// A pure READ: no write transaction, no writer lease, so an audit can run
/// beside a live deployment without changing a byte. `refs rebuild` calls this
/// and then writes the difference; the quiesced-store requirement belongs to
/// that write pass, not to this read.
pub fn ledger_picture(uri: &str) -> Result<LedgerPicture, MetaError> {
    let cluster_file = FdbConfig::parse_cluster(uri)
        .map_err(|e| MetaError::Db(format!("fdb:// ledger recount: {e}")))?;
    let db = database(&cluster_file)
        .map_err(|e| MetaError::Db(format!("fdb:// ledger recount: {e}")))?;
    let root = Keys::root();
    let ledger_root = root.subspace(&TAG_REFS);
    let (begin, end) = root_bounds();

    // ---- pass 1: what the SURVIVING TREES say, one walk over the root --------
    let mut computed: std::collections::BTreeMap<String, i64> = Default::default();
    let mut trees: std::collections::BTreeSet<String> = Default::default();
    let mut ledger_rows: Vec<String> = Vec::new();
    {
        // Keys are ordered by subspace, so a view's `'r'` rows are contiguous:
        // one distinct-set per subspace is enough to turn "rows" into "views".
        let mut cur = String::new();
        let mut seen: std::collections::HashSet<String> = Default::default();
        let mut visit = |key: &[u8]| -> Result<(), MetaError> {
            let base = root.bytes().len();
            let (name, used) = child_name(&root, key).ok_or_else(|| {
                MetaError::Db(format!(
                    "fdb:// ledger recount: a key under the 'grovefs' root names a subspace this build cannot decode ({}); refusing to recount from a tree that cannot be walked",
                    hex_bytes(&key[..key.len().min(48)])
                ))
            })?;
            let rest = &key[base + used..];
            if name == TAG_REFS {
                let (dr, _) = child_name(&ledger_root, key).ok_or_else(|| {
                    MetaError::Db(format!(
                        "fdb:// ledger recount: a ledger row's content key does not decode ({})",
                        hex_bytes(&key[..key.len().min(48)])
                    ))
                })?;
                ledger_rows.push(dr);
                return Ok(());
            }
            if cur != name {
                for dr in seen.drain() {
                    *computed.entry(dr).or_insert(0) += 1;
                }
                trees.insert(name.clone());
                cur = name.clone();
            }
            if rest.starts_with(b"\x02r\x00") {
                let dr = Keys::for_view(&name).decode_dataref(key).ok_or_else(|| {
                    MetaError::Db(format!(
                        "fdb:// ledger recount: view '{}' holds a 'r' row with no decodable content key ({}); refusing to recount",
                        name,
                        hex_bytes(&key[..key.len().min(48)])
                    ))
                })?;
                seen.insert(dr);
            }
            Ok(())
        };
        for_each_key(&db, begin, end, &mut visit)?;
        for dr in seen.drain() {
            *computed.entry(dr).or_insert(0) += 1;
        }
    }

    // ---- pass 2: what the ledger currently claims ---------------------------
    let probe = FdbStore::open(uri, REBUILD_PROBE_VIEW, true)
        .map_err(|e| MetaError::Db(format!("fdb:// ledger recount: {e}")))?;
    let mut current: std::collections::BTreeMap<String, i64> = Default::default();
    if !ledger_rows.is_empty() {
        let tx = probe.read_tx()?;
        let keys: Vec<Vec<u8>> = ledger_rows.iter().map(|dr| probe.keys.ledger(dr)).collect();
        let vals = tx
            .get_multi(&keys)
            .map_err(TxErr::hard_or_exhausted)?;
        for (dr, v) in ledger_rows.iter().zip(vals) {
            current.insert(dr.clone(), v.as_deref().and_then(unpack_i64).unwrap_or(0));
        }
    }
    Ok(LedgerPicture {
        trees,
        refs: computed,
        booked: current,
    })
}

/// Recompute the SHARED ledger from the surviving trees — the fdb:// answer to
/// `refs rebuild`, which used to be SQLite-only and therefore left a drifted
/// ledger in this backend with no repair path at all.
///
/// The rule is the one the SQLite rebuild states: one holder unit per view per
/// content key, and a row at zero is not a row ("absent and zero are the same
/// state"). Two consequences make this worth having beyond tidiness:
///   * an OVER-count (a subtree cleared out of band, or a view removed by
///     `fdbcli clearrange`) keeps bytes alive forever, because nothing ever reads
///     the row again to lower it;
///   * a LOST row (under-count) is the dangerous direction — the next unlink of
///     that key sees zero holders and frees bytes a live name still reads. It is
///     also invisible to every counter, which is why the recount reads the TREES
///     and never trusts the ledger it is rewriting.
///
/// Safety, and the one thing it cannot do for you: the recount is a read of the
/// whole root followed by a write pass, so it must run QUIESCED — every mount of
/// every view stopped, exactly as the SQLite arm requires (it refuses a mounted
/// store outright). A writer that books a key between the read and the write pass
/// would have that booking overwritten with the pre-booking count, i.e. precisely
/// the under-count this exists to repair.
///
/// A key this build cannot name, or an `'r'` row whose content key does not
/// decode, ABORTS the rebuild instead of being skipped: skipping one tree would
/// recompute every ref it names to a lower number, and lowering is the unsafe
/// direction. Refusing is the only honest answer.
pub fn rebuild_ledger(uri: &str) -> Result<LedgerRebuild, MetaError> {
    let pic = ledger_picture(uri)?;
    // The write half needs a store handle for its transactions; the picture
    // above deliberately did not keep one, so a read-only caller (the `gc`
    // audit) never holds a writer-capable object at all.
    let probe = FdbStore::open(uri, REBUILD_PROBE_VIEW, true)
        .map_err(|e| MetaError::Db(format!("fdb:// rebuild: {e}")))?;
    let computed = &pic.refs;
    let current = &pic.booked;

    // ---- pass 3: rewrite the difference ------------------------------------
    let mut set_rows: Vec<(String, i64)> = Vec::new();
    for (dr, want) in computed {
        if current.get(dr) != Some(want) {
            set_rows.push((dr.clone(), *want));
        }
    }
    let del_rows: Vec<String> = current
        .keys()
        .filter(|dr| !computed.contains_key(*dr))
        .cloned()
        .collect();
    let k = probe.keys.clone();
    for chunk in set_rows.chunks(REBUILD_CHUNK) {
        let chunk: Vec<(String, i64)> = chunk.to_vec();
        let k = k.clone();
        probe.with_serial_retry(move |tx| {
            for (dr, want) in &chunk {
                tx.set_i64(&k.ledger(dr), *want);
            }
            Ok(())
        })?;
    }
    for chunk in del_rows.chunks(REBUILD_CHUNK) {
        let chunk: Vec<String> = chunk.to_vec();
        let k = k.clone();
        probe.with_serial_retry(move |tx| {
            for dr in &chunk {
                tx.clear(&k.ledger(dr));
            }
            Ok(())
        })?;
    }
    Ok(LedgerRebuild {
        trees: pic.trees.len(),
        refs: computed.len(),
        rows_fixed: set_rows.len(),
        rows_deleted: del_rows.len(),
        holders: computed.values().sum(),
    })
}

impl FdbStore {
    /// Connect, ensure the root row (id/ino 1, so [`crate::meta::ROOT_INO`]
    /// stays valid), and return the handle. Idempotent: a second meta-server (or a
    /// restart) against the same cluster sees the same root.
    ///
    /// `readonly` suppresses that ensure step, which is what makes it mean
    /// something: `pg_store` and [`crate::mcp`] both open a view read-only to
    /// PROBE it, and a probe that seeds a root would create a subspace for a view
    /// that never existed (`du` on a typo would leave state behind). The FDB arm
    /// has to honour the same contract, or `readonly` is a decorative parameter —
    /// and unlike PG there is no `--role standby` path here (that combination is
    /// refused at `serve()`), so this flag's only user is the probe.
    pub fn open(uri: &str, view: &str, readonly: bool) -> std::io::Result<Self> {
        let cfg = FdbConfig::parse(uri, view)?;
        let db = database(&cfg.cluster_file)?;
        let this = Self {
            view: cfg.view.clone(),
            keys: Keys::for_view(&cfg.view),
            db,
            fence: std::sync::Arc::new(AtomicU64::new(0)),
            blk: std::sync::Mutex::new(Block { next: 0, limit: 0 }),
            cursors: std::sync::Mutex::new(std::collections::HashMap::new()),
            last_touch: AtomicU64::new(0),
        };
        if !readonly {
            this.ensure_root()?;
        }
        let e = this.get_epoch();
        this.fence.store(e.max(0) as u64, Ordering::SeqCst);
        Ok(this)
    }

    /// Read-only existence probe for the control plane's `has_physical` question:
    /// does `view`'s subspace in `uri` hold its root row? One snapshot read of
    /// `('n', ROOT_PARENT, "")` on a transaction that is never committed, so the
    /// probe neither creates anything nor adds a read-conflict to a live writer —
    /// the same contract [`FdbStore::open`]'s `readonly` flag exists to keep.
    ///
    /// The question has to be asked of the cluster rather than of a directory:
    /// under `--meta-store fdb://…` a base tree IS a subspace, so a mount gate
    /// that consulted the local filesystem would refuse every provisioned view and
    /// accept any stale `<view>.db` file — wrong in both directions, and the same
    /// class of bug as `view create`/`gc` falling through to the SQLite arm.
    ///
    /// Errors are plain text because the only caller is
    /// [`crate::mcp::ControlStore`], whose whole error surface is a `String`.
    pub fn subspace_exists(uri: &str, view: &str) -> Result<bool, String> {
        let this = Self::open(uri, view, true).map_err(|e| e.to_string())?;
        let tx = data_trx(&this.db)
            .map_err(|e| TxErr::from(e).hard_or_exhausted().to_string())?;
        let key = this.keys.name(ROOT_PARENT, "");
        let root = futures::executor::block_on(tx.get(&key, true))
            .map_err(|e| TxErr::from(e).hard_or_exhausted().to_string())?;
        Ok(root.is_some())
    }

    /// Does the cluster hold ANY key under `view`'s prefix?
    ///
    /// Deliberately a second question, not a replacement for
    /// [`Self::subspace_exists`]. That one asks "is there a tree a mount can
    /// open", which is the ROOT ROW; the two answers differ exactly where
    /// teardown needs them to. A subspace whose root row was cleared out of band
    /// (measured: `spike`, 7065 keys / 2001 entries / no root row) can never be
    /// mounted again, so "provisioned" is the wrong word for it — but it still
    /// books every content key it names, so it is the thing most in need of
    /// removal. `view rm --base` therefore asks THIS, and a name with nothing
    /// under it at all is the only "nothing to remove".
    ///
    /// One range read with `limit = 1`, on a snapshot transaction that is never
    /// committed: it neither creates the subspace nor leaves a read conflict.
    pub fn subspace_has_keys(uri: &str, view: &str) -> Result<bool, String> {
        let this = Self::open(uri, view, true).map_err(|e| e.to_string())?;
        let tx = data_trx(&this.db)
            .map_err(|e| TxErr::from(e).hard_or_exhausted().to_string())?;
        let prefix = this.keys.view_prefix().bytes().to_vec();
        let end = prefix_upper_bound(&prefix)
            .ok_or_else(|| "fdb_store: cannot bound a view prefix ending in 0xff".to_string())?;
        let opt: RangeOption = (
            KeySelector::first_greater_or_equal(prefix),
            KeySelector::first_greater_or_equal(end),
        )
            .into();
        let kvs = futures::executor::block_on(tx.get_range(&opt, 1, true))
            .map_err(|e| MetaError::Db(format!("fdb scan: {e}")).to_string())?;
        Ok(!kvs.as_ref().is_empty())
    }

    /// Holders booked against `data_ref` in the SHARED ledger, or 0 when no row
    /// exists (absent and zero are the same state: no view names those bytes).
    ///
    /// The diagnostic counterpart of [`PurgeReport`] and the only way to ask
    /// "how many views still name this content key?" without walking the whole
    /// `refs` subspace — which is also why it exists: a test that measured the
    /// ledger by counting keys could not tell a correct unwind from another
    /// suite's concurrent teardown (measured: 538 expected, 528 observed, purely
    /// because two sibling tests released their own bookings at the same time).
    pub fn ledger_holders(&self, data_ref: &str) -> Result<i64, MetaError> {
        let tx = self.read_tx()?;
        tx.get_i64(&self.keys.ledger(data_ref))
            .map_err(TxErr::hard_or_exhausted)
    }

    /// Every row of the shared ledger as `(data_ref, holders)`, read-only.
    ///
    /// [`Self::ledger_holders`] cannot answer "is there a row at all": an absent
    /// key reads as 0 by design (the SQL engines' "no row yet"), so a leaked
    /// zero/negative row and a clean absence look identical through it. This is
    /// the view that tells them apart, and it is what a `view scan`-style
    /// diagnosis needs when the ledger is the only thing left in a cluster (a
    /// bare `refs` subspace with no tree beside it is already a finding).
    pub fn ledger_rows(&self) -> Result<Vec<(String, i64)>, MetaError> {
        let tx = self.read_tx()?;
        let mut out = Vec::new();
        for (k, v) in tx
            .scan(&self.keys.ledger_range())
            .map_err(TxErr::hard_or_exhausted)?
        {
            if let Some(dr) = self.keys.decode_ledger(&k) {
                out.push((dr, unpack_i64(&v).unwrap_or(0)));
            }
        }
        Ok(out)
    }

    pub fn view(&self) -> &str {
        &self.view
    }

    pub fn keys(&self) -> &Keys {
        &self.keys
    }

    pub fn db(&self) -> &DbHandle {
        &self.db
    }

    /// Touch stamp for `--engine-idle-secs`. Under this backend eviction drops a
    /// handle and nothing else: no connection, no fd, no page cache is held per
    /// view (§8's table). Kept so the eviction policy can treat all three backends
    /// with one code path.
    pub fn touch(&self) {
        self.last_touch.store(secs_now() as u64, Ordering::Relaxed);
    }

    pub fn idle_secs(&self) -> i64 {
        let t = self.last_touch.load(Ordering::Relaxed);
        if t == 0 {
            0
        } else {
            secs_now().saturating_sub(t as i64)
        }
    }

    /// The root record: `id = ino = 1`, directory, mode 0755, owned by the process
    /// user — the same seed the SQLite and PG engines write. Keyed at
    /// `('n', ROOT_PARENT, "")` per §2 so it can never collide with a real entry.
    /// Written through the retry loop, because a concurrent `meta-server` starting
    /// the same view is a real scenario (HA handoff, or two operators) and losing
    /// that race is normal: the loser sees the committed root and does nothing.
    fn ensure_root(&self) -> std::io::Result<()> {
        let keys = self.keys.clone();
        self.with_serial_retry(|tx| {
            let name = keys.name(ROOT_PARENT, "");
            if tx.get(&name)?.is_some() {
                return Ok(false);
            }
            let rec = Rec {
                id: crate::meta::ROOT_INO,
                is_dir: true,
                data_ref: None,
                link_target: None,
                mode: 0o755,
                uid: crate::platform::geteuid(),
                gid: crate::platform::getgid(),
                mtime: secs_now(),
                atime: secs_now(),
                ctime: secs_now(),
                ino: crate::meta::ROOT_INO,
            };
            tx.set(&name, &rec.pack());
            tx.set(&keys.ino(crate::meta::ROOT_INO), &(ROOT_PARENT, "").pack_to_vec());
            tx.set(
                &keys.by_id(crate::meta::ROOT_INO),
                &(ROOT_PARENT, "").pack_to_vec(),
            );
            // The root's id is PRE-ISSUED, not allocated (§4's counter semantics).
            // Raising both allocator keys to it in this same transaction is what
            // keeps the first `create_node` from claiming id 1 and quietly
            // overwriting the root's `'d`/`'i` keys; a view seeded before this
            // line existed would need the same raise, which is P8's migration job.
            tx.raise_to_max(&keys.alloc(AllocKind::Id), crate::meta::ROOT_INO)?;
            tx.raise_to_max(&keys.alloc(AllocKind::Ino), crate::meta::ROOT_INO)?;
            Ok(true)
        })
        .map(|_| ())
        .map_err(|e| std::io::Error::other(e.to_string()))
    }

    /// The one piece of governance state that is already live: the epoch, read as
    /// a single point get on a fresh transaction with no write set.
    pub fn get_epoch(&self) -> i64 {
        let Ok(tx) = data_trx(&self.db) else {
            return 0;
        };
        let Ok(Some(v)) = futures::executor::block_on(tx.get(&self.keys.epoch(), false)) else {
            return 0;
        };
        unpack_i64(&v).unwrap_or(0)
    }

    /// Monotonic epoch update (§3.8): read, compare, write, so a stale replay
    /// cannot lower it. FDB's conflict detection aborts the racing writer, which is
    /// what makes the fencing decision "陈旧节点的重放被 epoch 拒绝" hold without a
    /// lock.
    pub fn set_epoch(&self, e: i64) -> std::io::Result<()> {
        let keys = self.keys.clone();
        self.with_serial_retry(move |tx| {
            let key = keys.epoch();
            let cur = tx.get_i64(&key)?;
            if e > cur {
                tx.set_i64(&key, e);
            }
            Ok(())
        })
        .map_err(|err| std::io::Error::other(err.to_string()))?;
        self.fence.store(e.max(0) as u64, Ordering::SeqCst);
        Ok(())
    }

    pub fn fence_handle(&self) -> ArcFence {
        self.fence.clone()
    }

    /// `VACUUM INTO` has no FDB analogue and must not get one: the cluster is
    /// already replicated, so a grovefs-side snapshot would be a second, weaker
    /// copy mechanism competing with `fdbbackup`. Reported as unsupported, exactly
    /// as the PG arm does, and `serve()` refuses `--role/--peer/--witness` here.
    pub fn backup_to_bytes(&self) -> std::io::Result<Vec<u8>> {
        Err(std::io::Error::other(
            "backup_to_bytes (VACUUM INTO snapshot shipping) is SQLite-only: a \
             FoundationDB cluster is already replicated, use fdbbackup for it",
        ))
    }

    // ---- transaction driver ------------------------------------------------

    /// Run `body` as one strict-serialisable transaction, retrying an OCC abort on
    /// a reset transaction, with the bounded exponential backoff the PG backend
    /// uses. The structural twin of `PgEngine::with_serial_retry`; the difference
    /// is only the detector (SQLSTATE 40001/40P01 vs abort codes, §6).
    ///
    /// `body` receives a borrowed [`Tx`] and returns:
    /// - `Ok(v)`   — the driver commits and returns `v`; a commit-time abort
    ///   re-runs the whole body, exactly as PG retries a 40001 at commit;
    /// - `Err(TxErr::Retry)` — body re-run;
    /// - `Err(TxErr::Hard)`  — propagated unchanged.
    ///
    /// Contract inherited from PG and worth stating: **a conflict must never
    /// escape to the POSIX caller.** `lock_op` etc. translate the final
    /// [`MetaError`] to errno; a `not_committed` that leaked out would look like
    /// EIO on a contention that is entirely normal.
    fn with_serial_retry<T, F>(&self, mut body: F) -> Result<T, MetaError>
    where
        F: FnMut(&Tx) -> Result<T, TxErr>,
    {
        let mut delay = SERIALIZATION_BACKOFF_MIN;
        for attempt in 0..SERIALIZATION_MAX_ATTEMPTS {
            let raw = match data_trx(&self.db) {
                Ok(t) => t,
                Err(e) => {
                    return Err(TxErr::from(e).hard_or_exhausted());
                }
            };
            let tx = Tx {
                inner: raw,
                keys: self.keys.clone(),
            };
            let out: Result<T, TxErr> = (|| {
                let v = body(&tx)?;
                // Commit consumes the transaction; hand it back for a possible
                // retry through the error's `Deref`, which is how the bindings
                // model "a tx is consumed by committing".
                match futures::executor::block_on(tx.inner.commit()) {
                    Ok(_committed) => Ok(v),
                    Err(e) => Err(TxErr::from(FdbError::from(e))),
                }
            })();
            match out {
                Ok(v) => return Ok(v),
                Err(TxErr::Hard(me)) => return Err(me),
                Err(TxErr::Retry(_)) => {
                    std::thread::sleep(delay);
                    delay = (delay * 2).min(SERIALIZATION_BACKOFF_MAX);
                    let _ = attempt; // exhausted -> terminal error below
                }
            }
        }
        Err(MetaError::Db(format!(
            "serialization retry exhausted after {SERIALIZATION_MAX_ATTEMPTS} attempts \
             (view '{}')",
            self.view
        )))
    }

    /// Reserve the next id for this process, refilling the block from the cluster
    /// key when the current one runs out (§4). The refill is its own
    /// read-modify-write transaction and is NOT part of the calling op's write set:
    /// 1024 creates cost 1 allocator key, and a crash between a refill and its use
    /// leaves only a hole, never a duplicate (§4's "号段在未落盘前不外泄").
    ///
    /// `ino` is derived from `id` here rather than from a second counter, which is
    /// exactly what PG does (`insert` sets `ino = id`; `pg_store.rs:851-877`).
    /// Keeping that parity is what makes the P6 differential harness compare trees
    /// instead of inode numbering. The `(v,'m','ino')` key is still written — as
    /// the high-water mark of every id this process may issue — because §4 requires
    /// migration import to be able to continue the sequence from `max(existing)`.
    ///
    /// The per-store mutex is held across the refill transaction. That is
    /// deliberate: the default SQLite backend holds a single-writer mutex across
    /// every write, so a lock taken 1/1024 of the time is strictly cheaper than the
    /// status quo, and no other lock is ever taken while it is held.
    fn alloc_id(&self) -> Result<i64, MetaError> {
        let mut blk = self.blk.lock().unwrap_or_else(|p| p.into_inner());
        if blk.next >= blk.limit {
            let block = ID_BLOCK;
            let id_key = self.keys.alloc(AllocKind::Id);
            let ino_key = self.keys.alloc(AllocKind::Ino);
            let hi = self.with_serial_retry(move |tx| {
                let hi = tx.bump(&id_key, block)?;
                // §4 migration watermark, not a second issuing counter.
                tx.raise_to_max(&ino_key, hi)?;
                Ok(hi)
            })?;
            // The counter means "HIGHEST ISSUED id", so a reservation claims
            // `(hi - block, hi]` and the first usable id is `hi - block + 1`. The
            // off-by-one here would reissue the root's own id (1): `ensure_root`
            // raises the counter to 1 precisely so the first `create_node` gets 2,
            // and a `next` of 1 would silently overwrite the root's `'d'`/`'i'`
            // keys — corruption, not a race. Pinned by
            // `tests::id_block_bounds_are_root_safe` and by
            // `tests/fdb_tree.rs` against a live cluster.
            let (next, limit) = block_bounds(hi, block);
            blk.next = next;
            blk.limit = limit;
        }
        let v = blk.next;
        blk.next += 1;
        Ok(v)
    }

    // ---- read paths ---------------------------------------------------------

    /// A read-only transaction. No commit is needed for reads and none is sent,
    /// which is what replaces PG's `with_read` pooled read path: there is no
    /// connection to check out (§8's table).
    fn read_tx(&self) -> Result<Tx, MetaError> {
        let inner = self
            .db
            .create_trx()
            .map_err(|e| MetaError::Db(format!("create_trx: {e}")))?;
        Ok(Tx {
            inner,
            keys: self.keys.clone(),
        })
    }

    /// `NodeInfo` for one located entry, with nlink/size computed exactly as
    /// `pg_store::finalize` does: directory = 2 + count of directory children
    /// (§3.4's `'k'` prefix), regular file = count of names sharing the inode
    /// (§3.4's `'h'` prefix), symlink = 1 with size = target length.
    ///
    /// `NodeInfo.id` carries the POSIX `ino`, not the internal row id — the same
    /// mapping `pg_store::build_info` applies, and the reason row ids never reach
    /// the wire protocol.
    fn info(&self, tx: &Tx, parent: i64, name: &str, rec: &Rec) -> Result<NodeInfo, TxErr> {
        let count = if rec.is_dir {
            tx.count_range(&self.keys.dir_children_prefix(rec.id))?
        } else {
            tx.count_range(&self.keys.holder_prefix(rec.ino))?
        };
        Ok(Self::entry_info(parent, name, rec, count))
    }

    /// One entry's `NodeInfo` from its record plus a count the caller already
    /// paid for: directories count their subdirectories, everything else counts
    /// its holders (hard links; 1 for a file named once). Split out of
    /// [`Self::info`] so a `readdir` page can hand over N counts fetched in one
    /// concurrent wave ([`Tx::count_multi`]) instead of doing N serial scans.
    fn entry_info(parent: i64, name: &str, rec: &Rec, count: u64) -> NodeInfo {
        let (kind, nlink, size) = if rec.is_dir {
            (NodeKind::Dir, 2 + count, 4096u64)
        } else if rec.link_target.is_some() {
            (
                NodeKind::Symlink,
                count.max(1),
                rec.link_target.as_deref().map(|s| s.len() as u64).unwrap_or(0),
            )
        } else {
            (NodeKind::Regular, count.max(1), 0u64)
        };
        NodeInfo {
            id: rec.ino,
            parent_id: parent,
            name: name.to_string(),
            kind,
            data_ref: rec.data_ref.clone(),
            link_target: rec.link_target.clone(),
            mode: rec.mode,
            uid: rec.uid,
            gid: rec.gid,
            mtime: rec.mtime,
            atime: rec.atime,
            ctime: rec.ctime,
            nlink,
            size,
        }
    }

    pub fn lookup(&self, parent: i64, name: &str) -> Option<NodeInfo> {
        self.touch();
        // One point read for the record; nlink then costs its own range count.
        let tx = self.read_tx().ok()?;
        let raw = tx.get(&self.keys.name(parent, name)).ok()??;
        let rec = Rec::unpack(&raw)?;
        self.info(&tx, parent, name, &rec).ok()
    }

    pub fn getattr(&self, ino: i64) -> Option<NodeInfo> {
        self.touch();
        let tx = self.read_tx().ok()?;
        let ptr = tx.get(&self.keys.ino(ino)).ok()??;
        let (parent, name) = unpack_ptr(&ptr)?;
        let raw = tx.get(&self.keys.name(parent, &name)).ok()??;
        let rec = Rec::unpack(&raw)?;
        self.info(&tx, parent, &name, &rec).ok()
    }

    pub fn readdir(&self, parent: i64) -> Vec<NodeInfo> {
        self.touch();
        let Ok(tx) = self.read_tx() else {
            return Vec::new();
        };
        entries_in(&tx, &self.keys, parent)
            .into_iter()
            .filter_map(|(p, n, rec)| self.info(&tx, p, &n, &rec).ok())
            .collect()
    }

    /// §3.3's paged listing: the same `offset` contract the kernel's `seekdir`
    /// cookie depends on, reached with a `start_after` cursor plus the same
    /// stride-32 checkpoints PG keeps, so a big directory costs O(limit + stride)
    /// keys per kernel buffer instead of O(offset + limit).
    pub fn readdir_page(&self, parent: i64, offset: usize, limit: usize) -> Vec<NodeInfo> {
        self.touch();
        let Ok(tx) = self.read_tx() else {
            return Vec::new();
        };
        // The checkpoint contract is the SQLite/PG one, and it has to be: the
        // mount derives resume offsets from what the KERNEL accepted (a 4 KiB
        // reply boundary), not from our stride, so both engines must agree on
        // what a checkpoint means. Key = the row count the page resumes at (a
        // multiple of STRIDE), value = the name of row `key - 1`. Keying by
        // BLOCK index instead lands the resume `offset % STRIDE` rows late —
        // measured on a 6000-name directory: 851 names listed twice and 851
        // dropped, while the total count stayed right.
        let base = offset - offset % READDIR_CURSOR_STRIDE;
        let start_after = if offset == 0 {
            None
        } else {
            self.cursors
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(&(parent, base))
                .cloned()
        };
        // A checkpoint we no longer need is never wrong, only slower: an expired
        // hint means "re-scan from the start", which is the pre-cursor behaviour.
        let (skip, mut opt) = match &start_after {
            // The range starts at row `base` (strictly after the checkpoint's
            // own name), so only the rows between `base` and `offset` are skipped.
            Some(nm) => (offset - base, self.keys.children_range_after(parent, nm)),
            None => (offset, self.keys.children_range(parent)),
        };
        // The read is bounded the way PG bounds its `LIMIT`: nothing past
        // `skip + limit` entries can be reached by this call, so fetching further
        // would turn one 4KiB getdents buffer into a whole-directory reply
        // (§3.3's cost claim rests on this bound, not on the cursor alone).
        opt.limit = Some(skip + limit + 1);
        let rows: Vec<(i64, String, Rec)> = scan_children(&tx, &self.keys, opt)
            .into_iter()
            .skip(skip)
            .take(limit)
            .collect();
        // READDIRPLUS wants an nlink per entry, and one serial count per entry is
        // N round-trips: on a 2,000-name directory, a 128-name page measured
        // 95.7 us/entry counted one by one against 8.1 us/entry this way (same
        // data, same process, 11.8x) — at 100k names that is ~9.6 s of counting.
        // The counts are independent, so the page's files are read as one
        // inode-span (`count_holder_span`) and its directories as one wave of
        // prefix counts (`count_multi`); the serial loop stays as the fallback.
        let mut counts: Vec<Option<u64>> = vec![None; rows.len()];
        let (files, dirs): (Vec<usize>, Vec<usize>) =
            (0..rows.len()).partition(|&i| !rows[i].2.is_dir);
        let file_inos: Vec<i64> = files.iter().map(|&i| rows[i].2.ino).collect();
        if !files.is_empty() {
            let counted = match tx.count_holder_span(&file_inos) {
                Ok(Some(found)) => Ok(found),
                _ => tx.count_multi(
                    &file_inos
                        .iter()
                        .map(|&ino| self.keys.holder_prefix(ino))
                        .collect::<Vec<_>>(),
                ),
            };
            if let Ok(found) = counted {
                for (k, &i) in files.iter().enumerate() {
                    counts[i] = Some(found[k]);
                }
            }
        }
        if !dirs.is_empty() {
            let ranges: Vec<RangeOption<'static>> = dirs
                .iter()
                .map(|&i| self.keys.dir_children_prefix(rows[i].2.id))
                .collect();
            if let Ok(found) = tx.count_multi(&ranges) {
                for (k, &i) in dirs.iter().enumerate() {
                    counts[i] = Some(found[k]);
                }
            }
        }
        let mut out = Vec::with_capacity(rows.len());
        if counts.iter().all(Option::is_some) {
            for ((p, n, rec), c) in rows.into_iter().zip(counts) {
                out.push(Self::entry_info(p, &n, &rec, c.unwrap_or(0)));
            }
        } else {
            for (p, n, rec) in rows {
                match self.info(&tx, p, &n, &rec) {
                    Ok(info) => out.push(info),
                    Err(_) => break,
                }
            }
        }
        // Checkpoint every stride boundary this page actually crossed, so a
        // resume at ANY later offset inside the covered window is a keyset seek
        // (the SQLite/PG loops do the same, one row at a time).
        if !out.is_empty() {
            let mut cur = self.cursors.lock().unwrap_or_else(|p| p.into_inner());
            for (i, info) in out.iter().enumerate() {
                let next = offset + i + 1;
                if next.is_multiple_of(READDIR_CURSOR_STRIDE) {
                    if cur.len() >= READDIR_CURSOR_CAP {
                        cur.clear(); // cap reached: the whole map is a hint, dropping it is safe
                    }
                    cur.insert((parent, next), info.name.clone());
                }
            }
        }
        out
    }

    /// Any name change invalidates every paging hint (§3.3's `drop_cursors`).
    /// Cheap and infrequent enough to clear wholesale; a partial invalidation
    /// would have to know which directories the change touched.
    fn drop_cursors(&self) {
        self.cursors.lock().unwrap_or_else(|p| p.into_inner()).clear();
    }

    pub fn supports_paged_readdir(&self) -> bool {
        true // §3.3: range + start_after + stride checkpoints
    }

    // ---- write paths --------------------------------------------------------

    /// One create = one transaction (§5). Uniqueness is decided by key existence
    /// after reading it (§3.1), so a concurrent insert of the same name aborts one
    /// side and the retry sees the rival row and reports `Exists` — the same
    /// observable `EEXIST` PG reaches through `UNIQUE_VIOLATION`/40001.
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
        self.touch();
        if name.is_empty() || name.contains('/') {
            return Err(MetaError::Db("create_node: empty or slashed name".to_string()));
        }
        let id = self.alloc_id()?;
        let keys = self.keys.clone();
        let name_owned = name.to_string();
        let ref_owned = data_ref.map(|s| s.to_string());
        let out = self.with_serial_retry(move |tx| {
            let nk = keys.name(parent, &name_owned);
            if tx.get(&nk)?.is_some() {
                return Err(MetaError::Exists.into());
            }
            // Parent must exist and be a directory; `by_id` is exactly the
            // `WHERE id=$1` lookup §3.9 says FDB has to make explicit.
            let pkey = keys.by_id(parent);
            let praw = tx.get(&pkey)?;
            let Some(ptr) = praw.as_deref().and_then(unpack_ptr) else {
                return Err(MetaError::NotFound.into());
            };
            let p_raw = tx.get(&keys.name(ptr.0, &ptr.1))?;
            if p_raw.is_none() {
                return Err(TxErr::Hard(MetaError::NotFound));
            }
            // `is_dir` of the parent is deliberately NOT checked here: the SQL
            // backends' INSERT only carries an FK on `parent_id`, because ENOTDIR
            // is decided during path resolution above the store. Adding a check
            // would give FDB an error surface the two backends it must match do not
            // have.

            let mut wrote = 0usize; // §5 write-set accounting, asserted in tests
            tx.set(
                &nk,
                &Rec {
                    id,
                    is_dir,
                    data_ref: ref_owned.clone(),
                    link_target: None,
                    mode,
                    uid,
                    gid,
                    mtime: secs_now(),
                    atime: secs_now(),
                    ctime: secs_now(),
                    ino: id,
                }
                .pack(),
            );
            wrote += 1;
            // Secondary keys, one per index the SQL backends carry.
            tx.set(&keys.by_id(id), &(parent, name_owned.as_str()).pack_to_vec());
            tx.set(&keys.ino(id), &(parent, name_owned.as_str()).pack_to_vec());
            wrote += 2;
            if is_dir {
                tx.set(&keys.dir_entry(parent, &name_owned), b"");
                wrote += 1;
            } else {
                tx.set(&keys.holder(id, parent, &name_owned), b"");
                wrote += 1;
            }
            if let Some(dr) = &ref_owned {
                tx.set(&keys.dataref(dr, parent, &name_owned), b"");
                wrote += 1;
                // 0 -> 1 first holder in this view books the shared ledger, in the
                // SAME transaction (pg_store.rs:847-850 / §3.6).
                if tx.count_range(&keys.dataref_prefix_of(dr))? == 1 {
                    tx.bump(&keys.ledger(dr), 1)?;
                    wrote += 1;
                }
            }
            Ok(wrote)
        });
        match out {
            Ok(_) => {
                self.drop_cursors();
                Ok(id)
            }
            Err(e) => Err(e),
        }
    }

    pub fn create_symlink(
        &self,
        parent: i64,
        name: &str,
        target: &str,
        uid: u32,
        gid: u32,
    ) -> Result<i64, MetaError> {
        self.touch();
        if name.is_empty() || name.contains('/') || target.is_empty() {
            return Err(MetaError::Db("create_symlink: bad name or target".to_string()));
        }
        let id = self.alloc_id()?;
        let keys = self.keys.clone();
        let n = name.to_string();
        let t = target.to_string();
        self.with_serial_retry(move |tx| {
            let nk = keys.name(parent, &n);
            if tx.get(&nk)?.is_some() {
                return Err(MetaError::Exists.into());
            }
            let pkey = keys.by_id(parent);
            let ptr = tx
                .get(&pkey)?
                .as_deref()
                .and_then(unpack_ptr)
                .ok_or_else(|| TxErr::Hard(MetaError::NotFound))?;
            let praw = tx.get(&keys.name(ptr.0, &ptr.1))?;
            if praw.is_none() {
                return Err(TxErr::Hard(MetaError::NotFound));
            }
            let now = secs_now();
            tx.set(
                &nk,
                &Rec {
                    id,
                    is_dir: false,
                    data_ref: None,
                    link_target: Some(t.clone()),
                    // Symlinks carry mode 0777 in the other backends; the caller
                    // never chmods through a symlink (POSIX follows it).
                    mode: 0o777,
                    uid,
                    gid,
                    mtime: now,
                    atime: now,
                    ctime: now,
                    ino: id,
                }
                .pack(),
            );
            tx.set(&keys.by_id(id), &(parent, n.as_str()).pack_to_vec());
            tx.set(&keys.ino(id), &(parent, n.as_str()).pack_to_vec());
            tx.set(&keys.holder(id, parent, &n), b"");
            Ok(())
        })?;
        self.drop_cursors();
        Ok(id)
    }

    /// One unlink = one transaction, ending in "is this content key now unheld".
    /// The final ledger read stays inside the same snapshot as the deletes — that
    /// is §3.5's reason the reverse index is an existence key rather than a
    /// counter, and the same argument `pg_store.rs:1082-1084` makes for PG.
    /// Everything the delete itself does now lives in [`drop_name_tx`], because
    /// `rename`'s overwrite path must tear a name down with byte-for-byte the same
    /// ledger and pointer rules; two copies of that logic would drift.
    pub fn unlink(&self, parent: i64, name: &str) -> Result<Option<String>, MetaError> {
        self.touch();
        let keys = self.keys.clone();
        let n = name.to_string();
        let out = self.with_serial_retry(move |tx| {
            let nk = keys.name(parent, &n);
            let raw = tx.get(&nk)?;
            let Some(raw) = raw else {
                return Err(MetaError::NotFound.into());
            };
            let rec = Rec::unpack(&raw).ok_or_else(|| TxErr::Hard(keys_corrupt(&nk)))?;
            drop_name_tx(tx, &keys, parent, &n, &rec)
        })?;
        self.drop_cursors();
        Ok(out)
    }

    /// chmod / chown / utimensall target an INODE, not a name, so the update must
    /// land on every row sharing it — PG's `UPDATE ... WHERE ino=$3`. A directory
    /// has no `'h'` keys (its ino is its id and it has one row), so the pointer
    /// key alone is the target set.
    fn patch_inode<F>(&self, ino: i64, f: F) -> Result<(), MetaError>
    where
        F: Fn(&mut Rec) + Send + Sync,
    {
        self.touch();
        let keys = self.keys.clone();
        self.with_serial_retry(move |tx| {
            let mut targets = vec![keys.ino(ino)];
            // Non-dir inodes can have several names; each is a separate row.
            targets.extend(
                tx.get_range(&keys.holder_prefix(ino))?
                    .into_iter()
                    .filter_map(|(k, _)| keys.decode_holder(&k))
                    .map(|(_, p, nm)| keys.name(p, &nm)),
            );
            // Deduplicate: for a plain file the pointer key names the same entry as
            // its only holder key, and patching twice would double the write set.
            targets.sort();
            targets.dedup();
            let mut done = false;
            for tk in targets {
                let raw = match &tk[..] {
                    b if b == keys.ino(ino).as_slice() => {
                        unpack_ptr(&tx.get(&tk)?.ok_or_else(|| {
                            TxErr::Hard(MetaError::NotFound)
                        })?)
                        .map(|(p, nm)| keys.name(p, &nm))
                    }
                    _ => Some(tk.clone()),
                };
                let Some(nk) = raw else { continue };
                let Some(mut rec) = tx.get(&nk)?.as_deref().and_then(Rec::unpack) else {
                    continue;
                };
                f(&mut rec);
                tx.set(&nk, &rec.pack());
                done = true;
            }
            if !done {
                return Err(MetaError::NotFound.into());
            }
            Ok(())
        })
    }

    pub fn set_mode(&self, ino: i64, mode: u32) {
        let _ = self.patch_inode(ino, move |r| {
            r.mode = mode;
            r.ctime = secs_now();
        });
    }

    pub fn set_uid_gid(&self, ino: i64, uid: u32, gid: u32) {
        let _ = self.patch_inode(ino, move |r| {
            r.uid = uid;
            r.gid = gid;
            r.ctime = secs_now();
        });
    }

    /// `atime`/`mtime` are the two columns a write path may leave alone, so `None`
    /// means "keep" — the same convention `pg_store::set_times` implements.
    pub fn set_times(&self, ino: i64, atime: Option<i64>, mtime: Option<i64>) {
        let _ = self.patch_inode(ino, move |r| {
            if let Some(a) = atime {
                r.atime = a;
            }
            if let Some(m) = mtime {
                r.mtime = m;
            }
        });
    }

    /// Data write: bump only `mtime`, which is why this is its own method rather
    /// than a `set_times` call with a `None` atime (§2's "独立写" note).
    pub fn touch_mtime(&self, ino: i64) {
        let _ = self.patch_inode(ino, move |r| r.mtime = secs_now());
    }

    // ---- the two hardest tree shapes (§3.9, §4) ----------------------------

    /// `link_node` reuses the source `ino` and allocates a new row `id` (§4).
    /// Its ledger booking is the 0->1 case in reverse: a second name for the same
    /// key must NOT re-book, so the count is taken BEFORE the reverse key is
    /// written — `pg_store.rs:949`'s `view_name_count(...) == 0` test, ported
    /// literally rather than re-derived, because "the source already holds this
    /// key in this view" is exactly the case that must not double-book.
    pub fn link_node(
        &self,
        src_ino: i64,
        new_parent: i64,
        new_name: &str,
    ) -> Result<i64, MetaError> {
        self.touch();
        if new_name.is_empty() || new_name.contains('/') {
            return Err(MetaError::Db("link_node: empty or slashed name".to_string()));
        }
        let id = self.alloc_id()?;
        let keys = self.keys.clone();
        let n = new_name.to_string();
        self.with_serial_retry(move |tx| {
            // The source row: §3.2's pointer locates ONE authoritative name, and
            // that record carries the state every sibling must present identically
            // (mode/uid/gid/data_ref) — sharing the inode IS the hard link.
            let ipk = keys.ino(src_ino);
            let ptr = unpack_ptr(
                &tx.get(&ipk)?
                    .ok_or_else(|| TxErr::Hard(MetaError::NotFound))?,
            )
            .ok_or_else(|| TxErr::Hard(keys_corrupt(&ipk)))?;
            let src_nk = keys.name(ptr.0, &ptr.1);
            let src = Rec::unpack(
                &tx.get(&src_nk)?
                    .ok_or_else(|| TxErr::Hard(MetaError::NotFound))?,
            )
            .ok_or_else(|| TxErr::Hard(keys_corrupt(&src_nk)))?;
            // Directories and symlinks are not linkable (PG refuses `is_dir ||
            // data_ref.is_none()` with the same EPERM, `pg_store.rs:945-947`).
            if src.is_dir || src.data_ref.is_none() {
                return Err(MetaError::OpNotPermitted.into());
            }
            let nk = keys.name(new_parent, &n);
            if tx.get(&nk)?.is_some() {
                return Err(MetaError::Exists.into());
            }
            let now = secs_now();
            let dr = src.data_ref.clone();
            tx.set(
                &nk,
                &Rec {
                    id,
                    is_dir: false,
                    data_ref: dr.clone(),
                    link_target: None,
                    mode: src.mode,
                    uid: src.uid,
                    gid: src.gid,
                    mtime: now,
                    atime: now,
                    ctime: now,
                    ino: src_ino,
                }
                .pack(),
            );
            tx.set(&keys.by_id(id), &(new_parent, n.as_str()).pack_to_vec());
            tx.set(&keys.holder(src_ino, new_parent, &n), b"");
            if let Some(dr) = &dr {
                let first_name = tx.count_range(&keys.dataref_prefix_of(dr))? == 0;
                tx.set(&keys.dataref(dr, new_parent, &n), b"");
                if first_name {
                    tx.bump(&keys.ledger(dr), 1)?;
                }
            }
            // §3.2 keeps one authoritative name per inode, chosen deterministically
            // as the smallest remaining (parent,name). A new name can displace it, so
            // the pointer is RECOMPUTED rather than left alone: special-casing
            // "only if the old one sorts first" is the kind of rule that drifts.
            if let Some((p, nm)) = first_holder(tx, &keys, src_ino)? {
                tx.set(&ipk, &(p, nm.as_str()).pack_to_vec());
            }
            Ok(src_ino)
        })
    }

    /// The hardest single transaction (§3.9): cycle check up the by-id pointer
    /// chain, same-inode no-op test, target teardown with its ledger effect, then
    /// the move of one name across the secondary keys that embed it. One POSIX
    /// rename = one FDB transaction, exactly as `pg_store.rs:994`'s single SSI
    /// transaction: I1 (one identity) and I2 (visible atomicity) are then
    /// properties of the schedule being serialisable, and a concurrent rename
    /// whose write sets intersect aborts one side, whose retry observes the
    /// committed shape and returns the correct POSIX outcome
    /// (ENOENT / ENOTEMPTY / EEXIST / success). No global writer, no lock ordering
    /// — OCC detection at commit does the job PostgreSQL's predicate tracking does.
    pub fn rename(
        &self,
        parent: i64,
        name: &str,
        new_parent: i64,
        new_name: &str,
        noreplace: bool,
    ) -> Result<Option<String>, MetaError> {
        self.touch();
        if name.is_empty() || new_name.is_empty() || name.contains('/') || new_name.contains('/') {
            return Err(MetaError::Db("rename: empty or slashed name".to_string()));
        }
        let keys = self.keys.clone();
        let n = name.to_string();
        let nn = new_name.to_string();
        let out = self.with_serial_retry(move |tx| {
            let old_nk = keys.name(parent, &n);
            let src = Rec::unpack(
                &tx.get(&old_nk)?
                    .ok_or_else(|| TxErr::Hard(MetaError::NotFound))?,
            )
            .ok_or_else(|| TxErr::Hard(keys_corrupt(&old_nk)))?;
            if is_descendant_tx(tx, &keys, new_parent, src.id)? {
                return Err(MetaError::BadParentCycle.into());
            }
            let tgt_nk = keys.name(new_parent, &nn);
            let tgt = match tx.get(&tgt_nk)? {
                Some(b) => Some(Rec::unpack(&b).ok_or_else(|| TxErr::Hard(keys_corrupt(&tgt_nk)))?),
                None => None,
            };
            // I1: renaming a name onto itself, or onto a sibling hard link of the
            // SAME inode, is a POSIX no-op. Without the ino test the teardown below
            // would drop one link and shrink nlink (pg_store.rs:1015-1024, which
            // mirrors the SQLite engine's guard).
            if let Some(t) = &tgt {
                if t.id == src.id || (t.ino != 0 && t.ino == src.ino) {
                    return Ok(None);
                }
            }
            if tgt.is_some() && noreplace {
                return Err(MetaError::Exists.into());
            }
            let mut gc = None;
            if let Some(t) = &tgt {
                // POSIX: overwrite only same-kind, and a directory target must be
                // empty — the emptiness test is the same `drop_name_tx` guard an
                // unlink applies, because it is the same delete.
                if t.is_dir != src.is_dir {
                    return Err(MetaError::TypeMismatch.into());
                }
                gc = drop_name_tx(tx, &keys, new_parent, &nn, t)?;
            }
            // The move: one value write, then every key that embeds (parent,name)
            // follows it. §5's write-set ceiling counts exactly these.
            let new_nk = keys.name(new_parent, &nn);
            tx.clear(&old_nk);
            tx.set(
                &new_nk,
                &Rec {
                    ctime: secs_now(),
                    ..src.clone()
                }
                .pack(),
            );
            tx.set(&keys.by_id(src.id), &(new_parent, nn.as_str()).pack_to_vec());
            if src.is_dir {
                tx.clear(&keys.dir_entry(parent, &n));
                tx.set(&keys.dir_entry(new_parent, &nn), b"");
            } else {
                tx.clear(&keys.holder(src.ino, parent, &n));
                tx.set(&keys.holder(src.ino, new_parent, &nn), b"");
            }
            if let Some(dr) = &src.data_ref {
                // Clear-then-set of the SAME (data_ref, parent, name)-shaped key in
                // one transaction: the view's name count for the content is
                // unchanged, so the shared ledger is not touched at all — rename is
                // not a delete plus a create as far as bytes are concerned.
                tx.clear(&keys.dataref(dr, parent, &n));
                tx.set(&keys.dataref(dr, new_parent, &nn), b"");
            }
            // §3.2 pointer after the move. A directory has no `'h` keys (its ino is
            // its id and it has exactly one name), so `first_holder` finds nothing
            // and the fallback IS the answer; a file gets its recomputed
            // smallest-remaining name, which the move may have changed either way.
            let (p, nm) = first_holder(tx, &keys, src.ino)?
                .unwrap_or((new_parent, nn.clone()));
            tx.set(&keys.ino(src.ino), &(p, nm.as_str()).pack_to_vec());
            Ok(gc)
        })?;
        self.drop_cursors();
        Ok(out)
    }

    /// §3.7's four steps in ONE transaction: lease sweep, conflict scan, the
    /// requesting owner's own conversion delete, and the grant. They belong in a
    /// single transaction because the answer to "does anyone else hold this
    /// range" is only meaningful together with the write that follows it — the
    /// same argument PG makes when it wraps the same four statements in one SSI
    /// tx (`pg_store.rs:1257-1318`). Two clients racing for one range: the loser
    /// aborts on commit, re-runs against a fresh snapshot, and then sees the
    /// winner's row — either as the `LockConflict` it must report, or as a grant
    /// it was entitled to because the ranges were disjoint. Locks on DIFFERENT
    /// inodes never conflict: their keys share no prefix.
    ///
    /// The retry budget is `with_serial_retry`'s, and the contract inherited from
    /// PG holds: a `not_committed` must never reach the POSIX caller — contention
    /// on a lock is normal, and EIO for it would be a bug the caller cannot fix.
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
        self.touch();
        let keys = self.keys.clone();
        // The same column widths the SQLite and PG arms use, so one `fcntl`
        // request stores identical numbers in all three backends: the range
        // clamps to `i64::MAX` (an open-ended `EOF` lock is the common case),
        // owner/sid/pid are plain casts.
        let s = start.min(i64::MAX as u64) as i64;
        let e = end.min(i64::MAX as u64) as i64;
        let owner_i = owner as i64;
        let ltype = ty.to_u8() as i64;
        let want_write = ltype == 1;
        let out = self.with_serial_retry(|tx| {
            let now = secs_now();
            sweep_expired_tx(tx, &keys, now)?;
            let rows = lock_rows_of_ino(tx, &keys, ino, now)?;
            if ty == LockType::Unlock {
                // F_UNLCK releases the caller's own interval and nothing else —
                // another owner's overlapping lock survives it, which is why the
                // delete below is filtered by owner before overlap is even asked.
                for r in rows
                    .iter()
                    .filter(|r| r.owner == owner_i && locks_overlap(r.start, r.end, s, e))
                {
                    delete_lock_tx(tx, &keys, r);
                }
                return Ok(None);
            }
            // The ruling `fcntl` read-lock compatibility turns on, kept verbatim
            // from `pg_store.rs:1274-1277`: a READ request is blocked only by a
            // WRITE holder, a WRITE request by anyone, and the caller's own locks
            // never block the caller.
            if let Some(c) = rows.iter().find(|r| {
                r.owner != owner_i
                    && locks_overlap(r.start, r.end, s, e)
                    && (want_write || r.val.ltype == 1)
            }) {
                return Ok(Some(LockConflict {
                    start: c.start.max(0) as u64,
                    end: c.end.max(0) as u64,
                    ty: LockType::from_u8(c.val.ltype as u8).unwrap_or(LockType::Read),
                    pid: c.val.pid as u32,
                }));
            }
            // Convert: re-declaring a range this owner already holds replaces that
            // row instead of stacking a second lock on it (PG's step ③ DELETE).
            for r in rows
                .iter()
                .filter(|r| r.owner == owner_i && locks_overlap(r.start, r.end, s, e))
            {
                delete_lock_tx(tx, &keys, r);
            }
            put_lock_tx(
                tx,
                &keys,
                &LockRow {
                    ino,
                    start: s,
                    end: e,
                    owner: owner_i,
                    val: LockVal {
                        ltype,
                        pid: pid as i64,
                        sid: sid as i64,
                        expires: now + crate::meta::LockLease::secs(),
                    },
                },
            );
            Ok(None)
        })?;
        Ok(out)
    }

    /// F_GETLK: name the blocker, write nothing.
    ///
    /// PG fans this out to its read pool at READ COMMITTED (`pg_store.rs:1334`);
    /// the FDB counterpart of "a read that joins no conflict set" is a
    /// transaction that is never committed — one consistent snapshot, zero write
    /// set, so a thousand `F_GETLK` polls cannot abort a writer.
    ///
    /// Which blocker is reported is the same unspecified choice PG makes with its
    /// un-ordered `LIMIT 1`: the scan returns keys in `(l_start, l_end, owner)`
    /// order, so this answers "the lowest-starting blocker", deterministically.
    pub fn get_lock(
        &self,
        ino: i64,
        owner: u64,
        start: u64,
        end: u64,
        ty: LockType,
    ) -> Option<LockConflict> {
        self.touch();
        let s = start.min(i64::MAX as u64) as i64;
        let e = end.min(i64::MAX as u64) as i64;
        let owner_i = owner as i64;
        let want_write = ty.to_u8() == 1;
        let tx = self.read_tx().ok()?;
        let rows = lock_rows_of_ino(&tx, &self.keys, ino, secs_now()).ok()?;
        rows
            .into_iter()
            .find(|r| {
                r.owner != owner_i
                    && locks_overlap(r.start, r.end, s, e)
                    && (want_write || r.val.ltype == 1)
            })
            .map(|c| LockConflict {
                start: c.start.max(0) as u64,
                end: c.end.max(0) as u64,
                ty: LockType::from_u8(c.val.ltype as u8).unwrap_or(LockType::Read),
                pid: c.val.pid as u32,
            })
    }

    /// Release every lock of one file description — FUSE `release` on the last
    /// close, and the path the "implicit release must not leak" pitfall is about.
    /// Idempotent: a second call for the same owner is a zero-key no-op.
    pub fn release_locks_of_owner(&self, owner: u64) {
        let keys = self.keys.clone();
        let owner_i = owner as i64;
        let _ = self.with_serial_retry(|tx| {
            // `'lo` is why a close costs a prefix instead of a scan of the view,
            // and why that key carries `l_end`: each entry names its primary
            // exactly. `delete_lock_tx` then takes the `'l` row and the `'lx`/
            // `'ls` mirrors with it — the owner axis alone would leave a session
            // mirror pointing at a row that no longer exists (§3.7's dual-bookkeeping
            // requirement, the pitfall this backend may not re-introduce).
            for (k, _) in tx.scan(&keys.lock_owner_prefix(owner_i))? {
                let Some((_o, ino, start, end)) = keys.decode_lock_owner(&k) else {
                    return Err(TxErr::Hard(keys_corrupt(&k)));
                };
                match tx.get(&keys.lock(ino, start, end, owner_i))? {
                    Some(v) => {
                        if let Some(val) = LockVal::unpack(&v) {
                            delete_lock_tx(
                                tx,
                                &keys,
                                &LockRow {
                                    ino,
                                    start,
                                    end,
                                    owner: owner_i,
                                    val,
                                },
                            );
                            continue;
                        }
                    }
                    None => {}
                }
                // A mirror with no row behind it is stale bookkeeping, not a lock:
                // drop the pointer so the sweep never revisits it.
                tx.clear(&k);
            }
            Ok(())
        });
    }

    /// Reap every lock of one mount session the moment its connection drops — the
    /// fast path of the lease, so a crashed client stops blocking peers without
    /// waiting a full lease out. Rides `'ls` for the same reason the release above
    /// rides `'lo`.
    pub fn release_locks_of_session(&self, sid: u64) {
        let keys = self.keys.clone();
        let sid_i = sid as i64;
        let _ = self.with_serial_retry(|tx| {
            for (k, _) in tx.scan(&keys.lock_sid_prefix(sid_i))? {
                let Some((_s, ino, start, end, owner)) = keys.decode_lock_sid(&k) else {
                    return Err(TxErr::Hard(keys_corrupt(&k)));
                };
                match tx.get(&keys.lock(ino, start, end, owner))? {
                    Some(v) => {
                        if let Some(val) = LockVal::unpack(&v) {
                            delete_lock_tx(
                                tx,
                                &keys,
                                &LockRow {
                                    ino,
                                    start,
                                    end,
                                    owner,
                                    val,
                                },
                            );
                            continue;
                        }
                    }
                    None => {}
                }
                tx.clear(&k);
            }
            Ok(())
        });
    }

    /// Push every lock of session `sid` out by one full lease. Driven by the mount
    /// daemon on a `lease/3` cadence, so a live holder never expires while a dead
    /// one stops renewing and ages out (§3.7).
    ///
    /// Renewal is deliberately unconditional on the current expiry, exactly as
    /// SQLite's `UPDATE locks SET expires_at = ?1 WHERE sid = ?2` and PG's are:
    /// resurrecting a lease that already ran out is those arms' behavior, and an
    /// FDB arm that filtered it out here would be a fourth semantic, not a fix.
    pub fn renew_locks(&self, sid: u64) {
        let keys = self.keys.clone();
        let sid_i = sid as i64;
        let mut renewed = 0u64;
        let _ = self.with_serial_retry(|tx| {
            renewed = 0;
            let until = secs_now() + crate::meta::LockLease::secs();
            for (k, _) in tx.scan(&keys.lock_sid_prefix(sid_i))? {
                let Some((_s, ino, start, end, owner)) = keys.decode_lock_sid(&k) else {
                    return Err(TxErr::Hard(keys_corrupt(&k)));
                };
                let pk = keys.lock(ino, start, end, owner);
                let Some(v) = tx.get(&pk)? else {
                    tx.clear(&k);
                    continue;
                };
                let Some(mut val) = LockVal::unpack(&v) else {
                    return Err(TxErr::Hard(keys_corrupt(&k)));
                };
                // §3.7's point for keeping `expires_at` out of the primary key:
                // a renewal rewrites the value and swaps the ONE expire-ordered
                // existence key; the lock's identity does not move.
                tx.clear(&keys.lock_expire(val.expires, ino, start, end, owner));
                val.expires = until;
                put_lock_tx(
                    tx,
                    &keys,
                    &LockRow {
                        ino,
                        start,
                        end,
                        owner,
                        val,
                    },
                );
                renewed += 1;
            }
            Ok(())
        });
        if renewed > 0 {
            crate::stats::count(crate::stats::Counter::LockRenew, renewed);
        }
    }

    /// Drive the lease sweep from outside (tests, and any operator tooling that
    /// wants leases reaped now rather than on the next contended lock). The same
    /// bounded `'lx` range step ① uses.
    pub fn sweep_expired_locks(&self, now_secs: i64) {
        let keys = self.keys.clone();
        let _ = self.with_serial_retry(|tx| {
            sweep_expired_tx(tx, &keys, now_secs)?;
            Ok(())
        });
    }

    /// Delete this view's whole subspace (§1's teardown prefix, one range clear).
    /// Two users need it: `view rm` under an FDB deployment (P5's governance work)
    /// and the P3/P6 differential harness, which must not leave scratch views
    /// scattered in the cluster. The shared ledger (§3.6) is deliberately NOT
    /// touched here — those rows belong to other views too, and unwinding this
    /// view's bookings plus GC-ing the bytes they free is `view rm`'s job in P5,
    /// not something a subspace clear may fake.
    pub fn clear_view(&self) -> Result<(), MetaError> {
        self.touch();
        let keys = self.keys.clone();
        self.with_serial_retry(move |tx| tx.clear_prefix(keys.view_prefix().bytes()))
    }

    /// Unbook `refs` from this view, 256 at a time: for each, delete this view's
    /// `'r` rows for that content key AND decrement the shared ledger (deleting
    /// the row at zero), both halves in the same transaction so the `'r` rows are
    /// the re-run marker (§`remove_view`). Returns `(unbooked, still_held,
    /// unbooked_refs)` — the refs that reached zero are the caller's byte reclaim
    /// list.
    ///
    /// Shared by `view rm --base` and by the multi-transaction whole-tree writes,
    /// which have to give a previous tree's bookings back before they rewrite it
    /// in chunks (a single-transaction write does that inline, in the one
    /// transaction that also writes the new rows).
    fn unbook_refs(&self, refs: Vec<String>) -> Result<(usize, usize, Vec<String>), MetaError> {
        const UNBOOK_CHUNK: usize = 256;
        let keys = self.keys.clone();
        let mut unbooked = 0usize;
        let mut still_held = 0usize;
        let mut unbooked_refs: Vec<String> = Vec::new();
        for chunk in refs.chunks(UNBOOK_CHUNK) {
            let k = keys.clone();
            let chunk: Vec<String> = chunk.to_vec();
            let (u, h) = self.with_serial_retry(move |tx| {
                let (mut u, mut h) = (Vec::new(), 0usize);
                for dr in &chunk {
                    let rows = k.dataref_prefix_of(dr);
                    tx.clear_prefix(rows.begin.key().to_vec().as_slice())?;
                    let left = tx.bump(&k.ledger(dr), -1)?;
                    if left <= 0 {
                        tx.clear(&k.ledger(dr));
                        u.push(dr.clone());
                    } else {
                        h += 1;
                    }
                }
                Ok((u, h))
            })?;
            unbooked += u.len();
            unbooked_refs.extend(u);
            still_held += h;
        }
        Ok((unbooked, still_held, unbooked_refs))
    }

    /// The DISTINCT content keys this view books, read from its own `'r` family.
    /// One range scan, so the caller can hand the result to [`Self::unbook_refs`]
    /// without re-deriving what a booking means.
    fn booked_refs(&self) -> Result<Vec<String>, MetaError> {
        let mut out = std::collections::BTreeSet::new();
        for (k, _) in self.with_serial_retry(|tx| tx.scan(&self.keys.dataref_all_range()))? {
            if let Some(dr) = self.keys.decode_dataref(&k) {
                out.insert(dr);
            }
        }
        Ok(out.into_iter().collect())
    }

    /// Does this view book ANY content key? A one-key probe, for callers that are
    /// about to clear the prefix and have to know whether the chunked unwind
    /// ([`Self::unbook_refs`]) is needed — knowing the COUNT would mean scanning
    /// the family, and the answer is only ever "none" or "some".
    fn has_bookings(&self) -> Result<bool, MetaError> {
        self.with_serial_retry(|tx| {
            let mut probe = self.keys.dataref_all_range();
            probe.limit = Some(1);
            Ok(!tx.get_range(&probe)?.is_empty())
        })
    }

    /// Keys but no root row: the shape a SPLIT import/fork leaves behind when it
    /// is interrupted before its last transaction (the root row is written last
    /// on purpose). `view scan` calls it a CORPSE.
    ///
    /// Read before the write branch, because `import --replace` adopts such a
    /// subspace through the CHUNKED unwind only: the leftovers can be several
    /// transactions' worth of rows, so the inline unwind the single-transaction
    /// path uses for a real tree would itself hit the 10 MB wall.
    fn residue_without_root(&self) -> Result<bool, MetaError> {
        self.with_serial_retry(|tx| {
            let mut probe = self.keys.all_range();
            probe.limit = Some(1);
            let has_keys = !tx.get_range(&probe)?.is_empty();
            if !has_keys || tx.get(&self.keys.name(ROOT_PARENT, ""))?.is_some() {
                return Ok(false);
            }
            Ok(true)
        })
    }

    /// Throw away every key this view owns, in ONE range mutation (its width is a
    /// round trip, not the width of the tree). Bookings are NOT released here —
    /// [`Self::unbook_refs`] must have run first, or the ledger keeps counting
    /// names that no longer exist.
    fn clear_view_prefix(&self) -> Result<(), MetaError> {
        let keys = self.keys.clone();
        self.with_serial_retry(move |tx| tx.clear_prefix(keys.view_prefix().bytes()))
    }

    /// Tear this view's whole subspace down AND unwind the shared byte ledger —
    /// what `view rm --base` means under `fdb://`.
    ///
    /// [`Self::clear_view`] alone is not that: the ledger is cluster-wide (§3.6),
    /// and a view's bookings are one unit per content key it names, not one per
    /// name. Deleting the subspace without unbooking would leave every blob it
    /// referenced booked forever — the conservative direction (bytes kept too
    /// long, never freed early), but an over-count nothing lowers on its own: the
    /// row outlives the only tree that would have read it, and `ledger_holders`
    /// cannot even show it (absent and zero are the same number through that
    /// door). `refs rebuild` can repair that shape, but only quiesced and only by
    /// recounting every tree. So the unwind is part of the same operation, and its
    /// result is reported instead of assumed.
    ///
    /// Order, and the reason for it — this is a destructive path:
    /// 1. inventory (read-only): entry count and the DISTINCT data refs the `'r`
    ///    family books for this view;
    /// 2. unwind, one transaction per chunk of refs. Each chunk does BOTH halves
    ///    for every ref in it: deletes this view's `'r` rows for that ref AND
    ///    decrements the shared ledger row (deleting it at zero);
    /// 3. the subspace itself, one range clear.
    ///
    /// The pair in step 2 is what makes re-running after any crash land on the
    /// right answer, and it is a correctness property, not tidiness. Progress
    /// through the work list is measured by the `'r` rows that are still there:
    /// a chunk that committed has none left for its refs, so a second run's
    /// inventory does not contain them and cannot decrement them again
    /// (under-counting the ledger is the UNSAFE direction — it frees bytes a
    /// surviving name still reads). A crash INSIDE a chunk aborts that whole
    /// transaction, so no half-chunk can exist. A crash between 2 and 3 leaves a
    /// tree whose bookings are already released; re-running sees no `'r` rows,
    /// so it releases nothing and goes straight to the range clear. Running this
    /// on an already-removed view is therefore a no-op returning zeros, which is
    /// also why the report describes ONE run rather than the total ever unwound.
    ///
    /// The dangerous window is the mirror image of that safety: after step 2 a
    /// reader under a live mount would still find names whose bytes are no longer
    /// booked, i.e. free for GC. That window cannot be closed from here — this
    /// process cannot see another machine's mount table — which is why the order
    /// is "unbook, then clear" and never the reverse, and why the clear is ONE
    /// range mutation (its width is a round trip, not the width of the tree).
    ///
    /// Callers must have stopped every mount of this view first: FDB is happy to
    /// serve a mount whose keys are being cleared, and nothing here can see
    /// another machine's mount table (the same caveat `view rm --base` documents
    /// for the SQLite arm).
    pub fn remove_view(&self) -> Result<PurgeReport, MetaError> {
        self.touch();
        let keys = self.keys.clone();
        let prefix = keys.view_prefix().bytes().to_vec();
        let end = prefix_upper_bound(&prefix).ok_or_else(|| {
            MetaError::Db("fdb_store: cannot clear a view prefix ending in 0xff".to_string())
        })?;

        let mut entries = 0u64;
        let mut blobs: std::collections::BTreeSet<String> = Default::default();
        let keys_removed = {
            let mut visit = |key: &[u8]| -> Result<(), MetaError> {
                let rest = key.get(prefix.len()..).unwrap_or(&[]);
                if rest.starts_with(b"\x02n\x00") {
                    entries += 1;
                } else if rest.starts_with(b"\x02r\x00") {
                    if let Some(dr) = keys.decode_dataref(key) {
                        blobs.insert(dr);
                    }
                }
                Ok(())
            };
            for_each_key(&self.db, prefix.clone(), end, &mut visit)?
        };

        // One ledger row per content key per view, so the DISTINCT refs above are
        // exactly what this view booked. Unwound through the shared helper, whose
        // per-256-ref transactions are what let a view holding a million blobs be
        // torn down without one transaction touching a million ledger keys.
        let refs: Vec<String> = blobs.into_iter().collect();
        let (unbooked, still_held, unbooked_refs) = self.unbook_refs(refs)?;
        let k = keys.clone();
        self.with_serial_retry(move |tx| tx.clear_prefix(k.view_prefix().bytes()))?;
        Ok(PurgeReport {
            view: self.view.clone(),
            keys: keys_removed,
            entries,
            unbooked,
            unbooked_refs,
            still_held,
        })
    }

    // ---- fork / graft (P4) ---------------------------------------------------
    //
    // A fork here is a COPY OF ONE SUBSPACE into another, and a graft is a subtree
    // placed into an existing one. Both are ONE transaction while the plan fits
    // one, the FDB counterpart of the file arm's `IMMEDIATE` transaction and of
    // PG's `INSERT ... SELECT` over schemas: a crash leaves either the whole copy
    // or nothing, never a half tree that a later `refs rebuild` would have to
    // interpret. A plan that does NOT fit one transaction is split (see
    // `TX_CHUNK_BUDGET`), root row LAST, so a crash leaves an unmountable partial
    // tree — visible as a corpse `view scan` names, never as half a view.

    /// Remove `view`'s tree — subspace AND its bookings — by opening it read-only
    /// and running the same unwind `view rm --base` uses.
    ///
    /// The read-only open is load-bearing: [`FdbStore::open`]'s writable form
    /// seeds the root row, so a drop of a name that was never seeded would create
    /// the very thing it is meant to delete.
    pub fn drop_tree(uri: &str, view: &str) -> Result<(), String> {
        let st = Self::open(uri, view, true).map_err(|e| e.to_string())?;
        if !Self::subspace_has_keys(uri, view)? {
            return Ok(());
        }
        st.remove_view().map(|_| ()).map_err(|e| e.to_string())
    }

    /// Seed `name` as a point-in-time copy of `base`'s tree, with content bytes
    /// SHARED rather than copied: every distinct `data_ref` is booked once against
    /// the new view, so a delete in either tree frees a blob only when its last
    /// name in the last holding view disappears — the same contract the file arm
    /// (`fork_view_with`) and the PG arm (`PgEngine::fork_tree`) implement.
    ///
    /// Not copied on purpose: `('c',epoch)` (the HA fencing epoch — a fork is a new
    /// view, not a standby replica) and the `'l'`/`'lo'`/`'ls'`/`'lx'` lock rows
    /// (transient leases of the source mount). Both live in the view prefix, so
    /// the copy simply does not carry them: rows come from the `'n'` family only.
    ///
    /// Inode numbers ARE copied, so hard links inside the base stay hard links in
    /// the fork.
    ///
    /// `seed == false` creates the fork's OWN tree (root only), so a curated
    /// subset can be placed later with [`Self::graft_tree`].
    pub fn fork_tree(
        uri: &str,
        base: &str,
        name: &str,
        seed: bool,
    ) -> Result<crate::meta::ForkStats, String> {
        // ONE snapshot read of the base, inodes included: names that shared an
        // inode stay hard links in the fork, the same guarantee the file arm gets
        // from its read-only connection and PG from `ORDER BY id`.
        let src = Self::open(uri, base, true).map_err(|e| e.to_string())?;
        let rows = {
            let tx = src.read_tx().map_err(|e| e.to_string())?;
            scan_all_entries(&tx, &src.keys)
        };
        let root_rec = rows
            .iter()
            .find(|(p, n, _)| *p == ROOT_PARENT && n.is_empty())
            .map(|(_, _, r)| r.clone())
            .ok_or_else(|| {
                format!(
                    "no physical base tree '{base}' in {uri} — `fork` copies a whole view \
                     tree, so the base must exist first (`view create --name {base}`)"
                )
            })?;
        let nodes_all = rows.len() as i64;
        let blobs_all = rows
            .iter()
            .filter_map(|(_, _, r)| r.data_ref.clone())
            .collect::<std::collections::HashSet<_>>()
            .len() as i64;
        let (out_rows, nodes, blobs) = if seed {
            let out: Vec<(Option<i64>, String, Rec)> = rows
                .into_iter()
                .map(|(p, n, r)| {
                    if p == ROOT_PARENT && n.is_empty() {
                        (None, "/".to_string(), r)
                    } else {
                        (Some(p), n, r)
                    }
                })
                .collect();
            (out, nodes_all, blobs_all)
        } else {
            // Root only. The root row is COPIED rather than re-seeded, so the
            // fork's root metadata matches its base instead of becoming a fresh
            // 0755-by-this-process root the way a brand-new view would.
            (vec![(None, "/".to_string(), root_rec)], 1, 0)
        };

        let dst = Self::open(uri, name, true).map_err(|e| e.to_string())?;
        let keys = dst.keys.clone();
        let out_blobs = out_rows
            .iter()
            .filter_map(|(_, _, r)| r.data_ref.clone())
            .collect::<std::collections::HashSet<_>>()
            .len();
        // The root row goes LAST, the same rule the import follows: the root row
        // is what makes a tree mountable, so a split fork stays invisible until
        // its final transaction instead of exposing a half-copied tree.
        let (mut write_order, root_row) = root_last(out_rows);
        write_order.extend(root_row);
        let total = write_order.len();
        let chunks = chunk_lengths(total, out_blobs, 0);

        // A destination that still BOOKS something is residue from an interrupted
        // attempt (its root row is written last, so `view scan` calls it a corpse)
        // or from an out-of-band clear. The write below replaces the destination's
        // rows wholesale, and only the chunked unwind further down can give those
        // bookings back: a bare `clear_prefix` would drop the `'r` rows while the
        // ledger kept counting them, and no later pass could tell they were stale.
        // So any residue takes the split branch even when the copy fits one
        // transaction.
        let residue = dst.has_bookings().map_err(|e| e.to_string())?;
        if !residue && chunks.len() <= 1 {
            dst.with_serial_retry(move |tx| {
                // The destination REPLACES its contents instead of merging into them —
                // PG's `DELETE FROM <dst>.node` before the `INSERT ... SELECT`, said
                // once here. The control plane has already refused a name holding a
                // live tree, so what this clears is residue (keys a crashed teardown
                // or an out-of-band `clearrange` left), which a pure append would
                // otherwise inherit.
                tx.clear_prefix(keys.view_prefix().bytes())?;
                write_rows_tx(tx, &keys, &write_order)
            })
            .map_err(|e| e.to_string())?;
            return Ok(crate::meta::ForkStats { nodes, blobs });
        }

        // A split fork: give back whatever an earlier interrupted attempt booked
        // before clearing the prefix. Without this the re-run would inherit
        // bookings for names it is about to replace, i.e. ledger units nothing
        // would ever release.
        let refs = dst.booked_refs().map_err(|e| e.to_string())?;
        dst.unbook_refs(refs).map_err(|e| e.to_string())?;
        dst.clear_view_prefix().map_err(|e| e.to_string())?;
        let tx_count = chunks.len();
        let mut offset = 0usize;
        for (i, &len) in chunks.iter().enumerate() {
            let slice: Vec<(Option<i64>, String, Rec)> = write_order[offset..offset + len].to_vec();
            let keys = keys.clone();
            let done = offset;
            dst.with_serial_retry(move |tx| write_rows_tx(tx, &keys, &slice))
                .map_err(|e| {
                    format!(
                        "fork of '{base}' into '{name}' aborted after {done} of {total} node(s) \
                         (transaction {} of {tx_count}): {e} — the root row is written last, so \
                         nothing can mount the partial fork; drop it with `grovefs view \
                         --meta-store fdb://… rm --name {name} --base` and re-run",
                        i + 1
                    )
                })?;
            offset += len;
            tracing::info!(
                nodes = offset,
                total,
                transaction = i + 1,
                of = tx_count,
                "fdb fork: transaction committed"
            );
        }
        Ok(crate::meta::ForkStats { nodes, blobs })
    }

    /// Graft a curated subset of `src`'s tree into `dst`'s tree: for every
    /// [`crate::meta::GraftItem`], copy that entry (a directory brings its whole
    /// subtree) to `dest/<dst>`, renaming it to the destination basename. Bytes
    /// are shared: every distinct `data_ref` is booked against the destination
    /// exactly like a fork's, and one the destination already named is not booked
    /// again (the ledger counts holders per VIEW, not per name).
    ///
    /// All-or-nothing while the plan fits one transaction: the source is ONE
    /// snapshot read, every plan path is resolved before the first write, and the
    /// whole plan lands in one transaction, so a bad line writes nothing. A plan
    /// that does not fit is placed in several transactions instead (see
    /// [`TX_CHUNK_BUDGET`]): an item too large for one transaction is split into
    /// its own entry plus one unit per child, and the ones already committed stay
    /// if a later one fails — the error says so and names `--skip-existing` as the
    /// continuation. Missing parent directories under `dest` are created (0755).
    /// `skip_existing` turns a conflict into "leave what is there and keep going",
    /// merging into an existing directory and skipping colliding files.
    pub fn graft_tree(
        uri: &str,
        dst: &str,
        src: &str,
        items: &[crate::meta::GraftItem],
        dest: &str,
        skip_existing: bool,
    ) -> Result<crate::meta::GraftStats, String> {
        if items.is_empty() {
            return Err("empty graft plan — nothing to copy".to_string());
        }
        let dest_prefix = if dest.trim().is_empty() {
            String::new()
        } else {
            crate::meta::normalize_plan_path(dest)?
        };
        let src_store = Self::open(uri, src, true).map_err(|e| e.to_string())?;
        let rows = {
            let tx = src_store.read_tx().map_err(|e| e.to_string())?;
            scan_all_entries(&tx, &src_store.keys)
        };
        let root = rows
            .iter()
            .find(|(p, n, _)| *p == ROOT_PARENT && n.is_empty())
            .map(|(_, _, r)| r.id)
            .ok_or_else(|| {
                format!(
                    "source view '{src}' has no tree in {uri} — refusing to graft from \
                     an empty store"
                )
            })?;
        let children = graft_children(&rows);
        // Resolve every source path BEFORE writing anything, so a typo on line 900
        // of a 100k-line plan cannot leave a half-grafted tree behind.
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

        // ONE transaction for the whole plan (all-or-nothing) whenever it fits one,
        // so its size is decided BEFORE the destination is opened for writing
        // (`open` provisions the subspace) — after the plan resolves, which is
        // when the subtree sizes are known. That transaction also READS the
        // destination's whole namespace (`scan_all_entries` is what continues its
        // id/ino allocator) and FDB charges reads against the same budget: that is
        // why the identical plan fits an empty tree and not a large one.
        let plan_rows = graft_plan_nodes(&rows, &children, &plan);
        let dest_rows = {
            let probe = Self::open(uri, dst, true).map_err(|e| e.to_string())?;
            let tx = probe.read_tx().map_err(|e| e.to_string())?;
            scan_all_entries(&tx, &probe.keys)
        };

        // The plan is placed in ONE transaction when it fits, and in several when
        // it does not: `graft_units` expands any item too large for a transaction
        // into its own entry plus one unit per child, so "too big" stops meaning
        // "refused" and starts meaning "more transactions".
        let units = graft_units(&rows, &children, &plan, TX_CHUNK_BUDGET);
        // "Does it fit one transaction" is answered WITH the destination scan
        // charged, because that path scans inside the transaction itself.
        let fits_one = graft_chunk_lengths(&units, dest_rows.len()).len() <= 1;
        if fits_one {
            let dst_store = Self::open(uri, dst, true).map_err(|e| e.to_string())?;
            let keys = dst_store.keys.clone();
            return dst_store
                .with_serial_retry(move |tx| {
                    // A graft lands in a BASE view or a fork — a tree with a root row.
                    // Asking the cluster (rather than trusting a definition) is what
                    // keeps a lens/alias name from receiving rows nothing can mount.
                    if tx.get(&keys.name(ROOT_PARENT, ""))?.is_none() {
                        return Err(TxErr::Hard(MetaError::Db(format!(
                            "no destination tree '{dst}' in {uri} — a graft lands in a BASE \
                             view or a fork (create it first)"
                        ))));
                    }
                    // Fresh ids/ino numbers start ABOVE both the existing id space and
                    // the existing inode numbers, so a graft never collides with rows
                    // copied by an earlier fork (whose inos came from another tree).
                    let existing = scan_all_entries(tx, &keys);
                    let max_id = existing.iter().map(|(_, _, r)| r.id).max().unwrap_or(0);
                    let max_ino = existing.iter().map(|(_, _, r)| r.ino).max().unwrap_or(0);
                    let mut ctx = GraftFdbCtx {
                        next: max_id.max(max_ino).max(crate::meta::ROOT_INO) + 1,
                        skip_existing,
                        stats: crate::meta::GraftStats::default(),
                        blobs: Default::default(),
                        inos: Default::default(),
                    };
                    let mut w = RowWriter::new();
                    for (idx, full) in &plan {
                        let (parent_path, name) = match full.rfind('/') {
                            Some(k) => (&full[..k], &full[k + 1..]),
                            None => ("", full.as_str()),
                        };
                        let parent = graft_ensure_dir(tx, &keys, &mut w, parent_path, &mut ctx)?;
                        graft_copy(
                            tx, &keys, &mut w, &rows, &children, *idx, parent, name, &mut ctx,
                        )?;
                    }
                    // ONE finish pass for everything this graft wrote: the `('i',ino)`
                    // pointers and the allocator continuation, by the same rules an
                    // import applies. Bookings were made per row by `put_row_tx`, so
                    // there is no batch-level booking loop here — that loop is what
                    // left a holder nobody could ever unbook on the file backend.
                    finish_rows_tx(tx, &keys, &w)?;
                    ctx.stats.blobs = ctx.blobs.len() as i64;
                    Ok(ctx.stats)
                })
                .map_err(|e| e.to_string());
        }

        // A split graft. The destination scan above already answered what the
        // first transaction would have asked, so the allocator continues from
        // there and no transaction rescans the tree (an O(n) read per chunk is how
        // a chunked writer would make itself quadratic) — which is also why these
        // chunks are packed at the FULL budget: the read that made the single
        // transaction too big is not in them.
        let chunks = graft_chunk_lengths(&units, 0);
        let dst_store = Self::open(uri, dst, true).map_err(|e| e.to_string())?;
        let tx_count = chunks.len();
        let mut inos: std::collections::HashMap<i64, i64> = Default::default();
        let mut next = dest_rows
            .iter()
            .map(|(_, _, r)| r.id.max(r.ino))
            .max()
            .unwrap_or(0)
            .max(crate::meta::ROOT_INO)
            + 1;
        let mut stats = crate::meta::GraftStats::default();
        let mut blobs: std::collections::HashSet<String> = Default::default();
        let mut placed = 0usize;
        let mut offset = 0usize;
        for (i, &len) in chunks.iter().enumerate() {
            let out = dst_store
                .graft_chunk(
                    &rows,
                    &children,
                    &units[offset..offset + len],
                    dst,
                    next,
                    &inos,
                    skip_existing,
                )
                .map_err(|e| {
                    format!(
                        "graft into '{dst}' aborted after {placed} of {plan_rows} node(s) \
                         (transaction {} of {tx_count}): {e} — the transactions already \
                         committed stay; re-run with --skip-existing to continue where it \
                         stopped",
                        i + 1
                    )
                })?;
            stats.nodes += out.stats.nodes;
            stats.dirs += out.stats.dirs;
            stats.skipped += out.stats.skipped;
            placed += out.stats.nodes as usize;
            next = out.next;
            inos = out.inos;
            blobs.extend(out.blobs);
            offset += len;
            tracing::info!(
                nodes = placed,
                total = plan_rows,
                transaction = i + 1,
                of = tx_count,
                "fdb graft: transaction committed"
            );
        }
        stats.blobs = blobs.len() as i64;
        Ok(stats)
    }

    /// One transaction of a SPLIT graft: place `units` under `dst`, continuing the
    /// id/ino allocator from `base_next` and the hard-link map from `inos`.
    ///
    /// Both are inputs and the grown values are outputs, so a serialization retry
    /// re-runs from the same base instead of from a half-advanced allocator (the
    /// single-transaction path gets the same guarantee by building its context
    /// inside the body).
    #[allow(clippy::too_many_arguments)]
    fn graft_chunk(
        &self,
        rows: &[(i64, String, Rec)],
        children: &std::collections::HashMap<i64, Vec<usize>>,
        units: &[GraftUnit],
        dst: &str,
        base_next: i64,
        inos: &std::collections::HashMap<i64, i64>,
        skip_existing: bool,
    ) -> Result<GraftChunk, MetaError> {
        let keys = self.keys.clone();
        self.with_serial_retry(move |tx| {
            if tx.get(&keys.name(ROOT_PARENT, ""))?.is_none() {
                return Err(TxErr::Hard(MetaError::Db(format!(
                    "no destination tree '{dst}' in this store — a graft lands in a BASE \
                     view or a fork (create it first)"
                ))));
            }
            let mut ctx = GraftFdbCtx {
                next: base_next,
                skip_existing,
                stats: crate::meta::GraftStats::default(),
                blobs: Default::default(),
                inos: inos.clone(),
            };
            let mut w = RowWriter::new();
            for u in units {
                let (parent_path, name) = match u.full.rfind('/') {
                    Some(k) => (&u.full[..k], &u.full[k + 1..]),
                    None => ("", u.full.as_str()),
                };
                let parent = graft_ensure_dir(tx, &keys, &mut w, parent_path, &mut ctx)?;
                if u.subtree {
                    graft_copy(tx, &keys, &mut w, rows, children, u.idx, parent, name, &mut ctx)?;
                } else {
                    graft_copy_one(tx, &keys, &mut w, rows, u.idx, parent, name, &mut ctx)?;
                }
            }
            finish_rows_tx(tx, &keys, &w)?;
            Ok(GraftChunk {
                stats: ctx.stats,
                next: ctx.next,
                inos: ctx.inos,
                blobs: ctx.blobs,
            })
        })
    }

    /// Flat dump for server-side `du` (§3.3's full-prefix scan). The P6
    /// differential harness compares backends through exactly this tuple, so its
    /// field order is fixed by [`crate::meta::MetaProvider::dump_tree`].
    ///
    /// The root is reported as `(1, 0, "/", dir, None)` — the same row SQL dumps.
    /// Its stored name is the empty string, because the byte-frozen root KEY is
    /// `('n', ROOT_PARENT, '')` (§1, pinned by `key_encoding_is_byte_frozen`), and
    /// a name of `"/"` would collide with nothing but would also rewrite the key
    /// format for a purely cosmetic gain. The display name is therefore mapped HERE
    /// rather than in the key: both SQL backends seed the root with the literal
    /// name `'/'` (`pg_store.rs:456`, `engine.rs`'s open-time INSERT), and a dump
    /// that differs on the root's name alone would make the differential harness
    /// report a phantom divergence.
    pub fn dump_tree(&self) -> Vec<(i64, i64, String, bool, Option<String>)> {
        let Ok(tx) = self.read_tx() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (parent, name, rec) in scan_all_entries(&tx, &self.keys) {
            let name = if parent == ROOT_PARENT && name.is_empty() {
                "/".to_string()
            } else {
                name
            };
            out.push((rec.id, parent, name, rec.is_dir, rec.data_ref));
        }
        out.sort();
        out
    }

    // ---- cross-backend migration (P8) ---------------------------------------
    //
    // `view export` / `view import` are the migration tool: the line-delimited
    // JSON they speak is already backend-neutral (`VIEW_EXPORT_FORMAT`, the same
    // twelve columns `pg_store` dumps), so "SQLite -> FDB" and "FDB -> SQLite"
    // are one pair of formats, not two dialects. No new command is introduced for
    // this, and that is deliberate: a migration path that only exists as its own
    // subcommand is a path nobody maintains, and the round-trip tests below would
    // be the only thing keeping it honest.

    /// Dump this subspace into the portable export file. The read is one
    /// snapshot transaction (`read_tx`), which is FDB's exact counterpart of the
    /// read-only-connection-inside-a-snapshot that `export_view` uses on SQLite:
    /// a concurrent writer cannot make the dump tear.
    ///
    /// `definition` is the view's own definition text when one exists. The CLI
    /// reads it from the control-plane store — the cluster's `ctl` subspace under
    /// `fdb://`, the same text a SQLite `<meta-dir>/meta/views/<view>.layout`
    /// holds — so the header is identical across backends.
    pub fn export_jsonl(
        &self,
        view: &str,
        out: &Path,
        force: bool,
        definition: Option<String>,
    ) -> Result<crate::meta::ViewExportStats, std::io::Error> {
        self.touch();
        if out.exists() && !force {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!(
                    "output {} already exists — pass --force to overwrite it",
                    out.display()
                ),
            ));
        }
        let kind = match definition.as_deref() {
            // A fork tree IS possible here now (see `fork_tree`), and the header
            // reports what the view actually is so a migration back to SQLite
            // keeps the definition's meaning. A stray `fork` definition with no
            // tree behind it exports as what it is: a fork that names nothing.
            Some(t) if t.lines().any(|l| l.trim_start().starts_with("fork ")) => "fork",
            _ => "physical",
        };
        let tx = self.read_tx().map_err(std::io::Error::other)?;
        let rows = scan_all_entries(&tx, &self.keys);
        // Two different refusals hide behind "no root row", and they have
        // different remedies: a name nobody ever provisioned is fixed by
        // `view create`, while a CORPSE (keys left, root row gone — the shape a
        // partial `clearrange` of the `'n` family leaves) is fixed only by
        // `view rm --base`, and telling the operator to create it would send them
        // straight into `view create`'s own corpse refusal. The probe is the same
        // one-key range `view scan` uses for its `KIND` column.
        let has_root = rows
            .iter()
            .any(|(parent, name, _)| *parent == ROOT_PARENT && name.is_empty());
        let leftovers = if has_root {
            false
        } else {
            let mut probe = self.keys.all_range();
            probe.limit = Some(1);
            !tx.get_range(&probe)
                .map_err(|e| std::io::Error::other(e.hard_or_exhausted().to_string()))?
                .is_empty()
        };
        drop(tx);
        // A name nobody provisioned must not export as an EMPTY tree. The SQLite
        // arm refuses this case outright, and a header claiming `nodes:0` would be
        // a silent downgrade of that contract — importing such a dump then
        // fabricates a root row for a view that never existed, which is exactly
        // how a typo in a migration script turns into a provisioned-but-empty tree
        // that every later check reads as "it worked".
        if !has_root {
            if leftovers {
                return Err(std::io::Error::other(format!(
                    "view '{view}' holds keys but no root row (a corpse — nothing can mount \
                     it), so there is no tree to export — clear it with \
                     `grovefs view --meta-store fdb://… rm --name {view} --base`"
                )));
            }
            return Err(std::io::Error::other(format!(
                "view '{view}' is not a provisioned subspace of the cluster — create one with \
                 `view create --meta-store fdb://… --view {view}` first (a tree that does not \
                 exist is not exported, it is refused)"
            )));
        }

        // Same root-name mapping as `dump_tree`: the frozen root KEY stores the
        // empty name, the export must carry "/" and a NULL parent like both SQL
        // dumps, or a round trip would differ on the root row alone.
        let mut objs: Vec<(i64, Value)> = rows
            .into_iter()
            .map(|(parent, name, rec)| {
                let root = parent == ROOT_PARENT && name.is_empty();
                let (pid, nm) = if root {
                    (Value::Null, Value::String("/".to_string()))
                } else {
                    (json!(parent), json!(name))
                };
                let obj = json!({
                    "id": rec.id,
                    "parent_id": pid,
                    "name": nm,
                    "is_dir": rec.is_dir as i64,
                    "data_ref": rec.data_ref,
                    "link_target": rec.link_target,
                    "mode": rec.mode as i64,
                    "uid": rec.uid as i64,
                    "gid": rec.gid as i64,
                    "mtime": rec.mtime,
                    "atime": rec.atime,
                    "ctime": rec.ctime,
                    "ino": rec.ino,
                });
                (rec.id, obj)
            })
            .collect();
        objs.sort_by_key(|(id, _)| *id);
        let blobs = {
            let mut set: std::collections::HashSet<String> = Default::default();
            for (_, o) in &objs {
                if let Some(d) = o["data_ref"].as_str() {
                    set.insert(d.to_string());
                }
            }
            set.len() as i64
        };

        // Write beside the destination, then rename: an interrupted export never
        // leaves a half file at the requested path (the SQLite export does the
        // same, `engine.rs`'s `.{name}.tmp-{pid}`).
        let mut tmp = out.to_path_buf();
        let name = out.file_name().and_then(|n| n.to_str()).unwrap_or("export");
        tmp.set_file_name(format!(".{name}.tmp-{}", std::process::id()));
        let write = (|| -> Result<(), std::io::Error> {
            let f = std::fs::File::create(&tmp)?;
            let mut w = BufWriter::new(f);
            let header = json!({
                "format": crate::meta::VIEW_EXPORT_FORMAT,
                "version": crate::meta::VIEW_EXPORT_VERSION,
                "view": view,
                "kind": kind,
                "definition": definition,
                "exported_at": secs_now(),
                "nodes": objs.len() as i64,
                "blobs": blobs,
            });
            writeln!(w, "{header}")?;
            for (_, o) in &objs {
                writeln!(w, "{o}")?;
            }
            w.flush()?;
            w.get_ref().sync_all()
        })();
        if let Err(e) = write {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        std::fs::rename(&tmp, out)?;
        Ok(crate::meta::ViewExportStats {
            nodes: objs.len() as i64,
            blobs,
            kind: kind.to_string(),
        })
    }

    /// Install an export file as this view's tree. One transaction while the dump
    /// fits one, so the target is at every instant either the old tree or the new
    /// one — never a mixture, which is what SQLite gets from building a scratch db
    /// and renaming it. A larger dump is written in several (see
    /// [`TX_CHUNK_BUDGET`]), root row LAST, so an interrupted run leaves an
    /// unmountable partial tree rather than half a view a client would read as
    /// real — and `--replace` picks that shape up again through the chunked
    /// unwind.
    ///
    /// The caller opens this store WITHOUT seeding a root: the dump carries its
    /// own root row and both allocator water marks, so an invalid dump leaves NO
    /// key behind and the name stays unprovisioned — the exact counterpart of
    /// SQLite's build-beside-and-rename refusing to leave a `bad3.db`.
    ///
    /// Three things this must get right and one of them has no SQL analogue:
    ///
    /// * **ids and inodes keep their exported values.** Re-issuing them would
    ///   break every cached client handle and make `.grove-stats` counts stop
    ///   matching the view they were taken on, and it would collapse hard links
    ///   into separate files (two rows, one `ino`, is the whole hard link).
    /// * **the allocator continues from the imported maximum** — this backend's
    ///   [`PgEngine::setval`](crate::pg_store) counterpart, i.e. `raise_to_max` on
    ///   both water marks in the same transaction. Without it the next create
    ///   would hand out an id that already exists.
    /// * **the authoritative `('i',ino)` pointer is recomputed, not copied.**
    ///   §3.2 keeps one name per inode, chosen as the smallest remaining
    ///   `(parent_id, name)`; recomputing it here with the very function
    ///   `link_node` uses is what stops an import from pointing every sibling at a
    ///   name that the source dump happened to list first.
    pub fn import_jsonl(
        &self,
        input: &Path,
        replace: bool,
    ) -> Result<crate::meta::ViewImportStats, std::io::Error> {
        self.touch();
        let f = std::fs::File::open(input)
            .map_err(|e| std::io::Error::other(format!("cannot open {}: {e}", input.display())))?;
        let mut rdr = BufReader::new(f);
        let mut header_line = String::new();
        if rdr.read_line(&mut header_line)? == 0 {
            return Err(std::io::Error::other(format!(
                "{} is empty — not a grovefs view export",
                input.display()
            )));
        }
        let header: Value = serde_json::from_str(header_line.trim_end()).map_err(|e| {
            std::io::Error::other(format!(
                "{}:1: not a grovefs view export header ({e})",
                input.display()
            ))
        })?;
        if header["format"] != crate::meta::VIEW_EXPORT_FORMAT {
            return Err(std::io::Error::other(format!(
                "{}:1: unknown format '{}' (expected '{}')",
                input.display(),
                header["format"].as_str().unwrap_or("<missing>"),
                crate::meta::VIEW_EXPORT_FORMAT
            )));
        }
        let version = header["version"].as_u64().unwrap_or(0);
        if version != crate::meta::VIEW_EXPORT_VERSION {
            return Err(std::io::Error::other(format!(
                "{}:1: unsupported export version {version} (this build reads {})",
                input.display(),
                crate::meta::VIEW_EXPORT_VERSION
            )));
        }
        let kind = header["kind"].as_str().unwrap_or("physical").to_string();

        // Rows first, in full, then one transaction. The structural checks are
        // the SQLite import's checks — same one-root rule, same dangling-parent
        // rule, same header-count rule — because a migration that validates less
        // strictly than the tool it replaces would let a tree through that the
        // SQLite backend would refuse.
        // The root row's portable name is "/", mapped back to the frozen root
        // name by `root_entry_name` (see that function); every place a row is
        // turned into a stored name goes through that one rule.
        let mut rows: Vec<(Option<i64>, String, Rec)> = Vec::new();
        let mut line = String::new();
        let mut lineno = 1usize;
        loop {
            line.clear();
            if rdr.read_line(&mut line)? == 0 {
                break;
            }
            lineno += 1;
            if line.trim().is_empty() {
                continue;
            }
            let v: Value = serde_json::from_str(line.trim_end())
                .map_err(|e| std::io::Error::other(format!("{}:{lineno}: invalid JSON: {e}", input.display())))?;
            let id = v["id"].as_i64().ok_or_else(|| {
                std::io::Error::other(format!("{}:{lineno}: missing integer \"id\"", input.display()))
            })?;
            let parent_id = match v.get("parent_id") {
                None | Some(Value::Null) => None,
                Some(x) => Some(x.as_i64().ok_or_else(|| {
                    std::io::Error::other(format!(
                        "{}:{lineno}: \"parent_id\" must be an integer or null",
                        input.display()
                    ))
                })?),
            };
            let name = v["name"].as_str().ok_or_else(|| {
                std::io::Error::other(format!("{}:{lineno}: missing string \"name\"", input.display()))
            })?;
            // The root row is exported the way BOTH SQL dumps export it: name
            // "/" with a NULL parent. "/" is precisely the one spelling that is not
            // a legal directory entry, because it is the frozen root KEY's portable
            // name (§1) and is mapped back to the stored empty name below — so the
            // shape check applies to every row except that one, and a root row
            // spelled any other way (empty name, or "/" with a parent) is still
            // refused rather than quietly accepted.
            if !(parent_id.is_none() && name == "/") && (name.is_empty() || name.contains('/')) {
                return Err(std::io::Error::other(format!(
                    "{}:{lineno}: name {name:?} cannot be a directory entry",
                    input.display()
                )));
            }
            let is_dir = v["is_dir"].as_i64().ok_or_else(|| {
                std::io::Error::other(format!("{}:{lineno}: missing integer \"is_dir\"", input.display()))
            })?;
            if is_dir != 0 && is_dir != 1 {
                return Err(std::io::Error::other(format!(
                    "{}:{lineno}: \"is_dir\" must be 0 or 1, got {is_dir}",
                    input.display()
                )));
            }
            let mtime = v["mtime"].as_i64().unwrap_or_else(secs_now);
            rows.push((
                parent_id,
                name.to_string(),
                Rec {
                    id,
                    is_dir: is_dir == 1,
                    data_ref: v["data_ref"].as_str().map(str::to_string),
                    link_target: v["link_target"].as_str().map(str::to_string),
                    mode: v["mode"].as_i64().unwrap_or(if is_dir == 1 { 0o755 } else { 0o644 }) as u32,
                    uid: v["uid"].as_i64().unwrap_or(0) as u32,
                    gid: v["gid"].as_i64().unwrap_or(0) as u32,
                    mtime,
                    atime: v["atime"].as_i64().unwrap_or(mtime),
                    ctime: v["ctime"].as_i64().unwrap_or(mtime),
                    ino: v["ino"].as_i64().ok_or_else(|| {
                        std::io::Error::other(format!("{}:{lineno}: missing integer \"ino\"", input.display()))
                    })?,
                },
            ));
        }

        let keys = self.keys.clone();
        let view_name = self.view.clone();
        let input_rows = rows.len() as i64;
        let claimed_nodes = header["nodes"].as_i64();
        let claimed_blobs = header["blobs"].as_i64();
        // Distinct content references, counted before the transaction: the
        // header's `blobs` claim is checked the same way `import_view` checks it,
        // and the number is reported back to the caller either way.
        let blobs = {
            let mut set: std::collections::HashSet<&str> = Default::default();
            for (_, _, r) in &rows {
                if let Some(d) = &r.data_ref {
                    set.insert(d.as_str());
                }
            }
            set.len() as i64
        };
        if let Some(c) = claimed_blobs {
            if c != blobs {
                return Err(std::io::Error::other(format!(
                    "import rejected: header declares {c} blob reference(s), file carries {blobs}"
                )));
            }
        }
        // Structural validation is PURE, so a malformed dump writes nothing whether
        // the write is one transaction or several.
        validate_import_rows(&rows, claimed_nodes).map_err(std::io::Error::other)?;

        // The root row is written LAST. It is what makes a tree mountable, so a
        // SPLIT import stays invisible until its final transaction: an interrupted
        // run leaves keys no mount can reach, not a half-populated tree a client
        // would read as real.
        let (mut write_order, root_row) = root_last(rows);
        write_order.extend(root_row);
        let total = write_order.len();
        let chunks = chunk_lengths(total, blobs as usize, 0);

        // One transaction is the whole contract whenever the dump fits one: the
        // target is at every instant either the old tree or the new one. A CORPSE
        // the dump can adopt (`--replace`) is the exception: the leftovers may be
        // many transactions' worth of rows, so clearing them inline would hit the
        // very wall this split exists to get around — take the chunked unwind even
        // when the dump itself is small.
        let corpse = self.residue_without_root().map_err(std::io::Error::other)?;
        let force_split = corpse && replace;
        if !force_split && chunks.len() <= 1 {
            let replaced = self
                .with_serial_retry(move |tx| {
                    let replaced = prep_import_target(tx, &keys, &view_name, replace, true)?;
                    // The rows land through the writer shared with `fork_tree` and
                    // `graft_tree`, so an import books the ledger, names every inode,
                    // and continues both water marks by exactly the rules a fork does.
                    write_rows_tx(tx, &keys, &write_order)?;
                    Ok(replaced)
                })
                .map_err(std::io::Error::other)?;
            return Ok(crate::meta::ViewImportStats {
                nodes: input_rows,
                blobs,
                kind,
                replaced,
            });
        }

        // Several transactions. The gate runs first, on its own: it is the same
        // refusal set as the single-transaction path, but the teardown that
        // follows is chunked, because one transaction touching a million ledger
        // keys is exactly the wall this branch exists to get around.
        let replaced = self
            .with_serial_retry({
                let (keys, view_name) = (keys.clone(), view_name.clone());
                move |tx| prep_import_target(tx, &keys, &view_name, replace, false)
            })
            .map_err(std::io::Error::other)?;
        if replaced {
            let refs = self.booked_refs().map_err(std::io::Error::other)?;
            self.unbook_refs(refs).map_err(std::io::Error::other)?;
            self.clear_view_prefix().map_err(std::io::Error::other)?;
        }

        let tx_count = chunks.len();
        let mut offset = 0usize;
        for (i, &len) in chunks.iter().enumerate() {
            let slice: Vec<(Option<i64>, String, Rec)> = write_order[offset..offset + len].to_vec();
            let keys = keys.clone();
            let done = offset;
            self.with_serial_retry(move |tx| write_rows_tx(tx, &keys, &slice))
                .map_err(|e| {
                    std::io::Error::other(format!(
                        "import aborted after {done} of {total} row(s) (transaction {} of \
                         {tx_count}): {e} — the root row is written last, so nothing can mount \
                         the partial tree; re-run with `--replace`, or drop it with `grovefs view \
                         --meta-store fdb://… rm --name {view_name} --base` and import again",
                        i + 1
                    ))
                })?;
            offset += len;
            tracing::info!(
                rows = offset,
                total,
                transaction = i + 1,
                of = tx_count,
                "fdb import: transaction committed"
            );
        }
        Ok(crate::meta::ViewImportStats {
            nodes: input_rows,
            blobs,
            kind,
            replaced,
        })
    }
}


// ---- graft helpers -----------------------------------------------------------
//
// These are the file arm's graft rules (`meta::engine`'s `graft_children` /
// `graft_resolve` / `graft_ensure_dir` / `graft_copy`) restated for this backend's
// row shape, which is `(parent, name, Rec)` rather than a SQLite `GraftRow`. The
// RULES are the contract — a plan means the same thing on every backend — and only
// the row type is why the code cannot be shared.

/// Mutable state carried through one graft transaction: the id/ino allocator, the
/// merge rule, the running totals and the inode mapping.
struct GraftFdbCtx {
    next: i64,
    skip_existing: bool,
    stats: crate::meta::GraftStats,
    /// Distinct content keys this graft referenced, reported as `blobs` — the file
    /// arm counts the same set (every inserted `data_ref`), not the ledger delta.
    blobs: std::collections::HashSet<String>,
    /// source `ino` -> destination `ino`: names that shared an inode in the source
    /// stay hard links of each other in the destination.
    inos: std::collections::HashMap<i64, i64>,
}

/// Nodes and blob set ONE plan item writes. A directory item brings its whole
/// subtree, so an item's line count says nothing about its size; `seen` is per
/// item because the same source may be named twice under two destinations.
fn graft_item_cost<'a>(
    rows: &'a [(i64, String, Rec)],
    children: &std::collections::HashMap<i64, Vec<usize>>,
    idx: usize,
) -> (usize, std::collections::HashSet<&'a str>) {
    let mut nodes = 0usize;
    let mut blobs: std::collections::HashSet<&'a str> = Default::default();
    let mut stack = vec![idx];
    let mut seen: std::collections::HashSet<usize> = Default::default();
    while let Some(i) = stack.pop() {
        if !seen.insert(i) {
            continue;
        }
        nodes += 1;
        if let Some(dr) = rows[i].2.data_ref.as_deref() {
            blobs.insert(dr);
        }
        if let Some(kids) = children.get(&rows[i].2.id) {
            stack.extend(kids.iter().copied());
        }
    }
    (nodes, blobs)
}

/// One unit a split graft places on its own. A plan item whose whole subtree fits
/// one transaction stays a single SUBTREE unit ([`graft_copy`]); one that does not
/// is split into its own entry (`subtree == false`, so the directory's mode, owner
/// and timestamps survive the split) followed by one unit per child. That is what
/// lets a 50,000-entry directory graft at all instead of being refused.
struct GraftUnit {
    idx: usize,
    full: String,
    subtree: bool,
    /// Estimated transaction cost of this unit, computed once. Recomputing it
    /// while packing would be O(subtree) per look at the same item.
    cost: usize,
}

fn graft_units(
    rows: &[(i64, String, Rec)],
    children: &std::collections::HashMap<i64, Vec<usize>>,
    plan: &[(usize, String)],
    budget: usize,
) -> Vec<GraftUnit> {
    let mut out = Vec::new();
    for (idx, full) in plan {
        graft_units_of(rows, children, *idx, full, budget, &mut out);
    }
    out
}

fn graft_units_of(
    rows: &[(i64, String, Rec)],
    children: &std::collections::HashMap<i64, Vec<usize>>,
    idx: usize,
    full: &str,
    budget: usize,
    out: &mut Vec<GraftUnit>,
) {
    let (nodes, blobs) = graft_item_cost(rows, children, idx);
    let cost = nodes
        .saturating_mul(TX_ROW_BYTES)
        .saturating_add(blobs.len().saturating_mul(TX_BLOB_BYTES));
    // A single entry is one row and always fits, so the recursion always
    // terminates: split only directories, and only while they are too big.
    if cost <= budget || !rows[idx].2.is_dir {
        out.push(GraftUnit {
            idx,
            full: full.to_string(),
            subtree: true,
            cost,
        });
        return;
    }
    let own = TX_ROW_BYTES
        + if rows[idx].2.data_ref.is_some() {
            TX_BLOB_BYTES
        } else {
            0
        };
    out.push(GraftUnit {
        idx,
        full: full.to_string(),
        subtree: false,
        cost: own,
    });
    if let Some(kids) = children.get(&rows[idx].2.id) {
        for k in kids {
            let child_full = format!("{full}/{}", rows[*k].1);
            graft_units_of(rows, children, *k, &child_full, budget, out);
        }
    }
}

/// Lengths of the transactions a split graft becomes: greedy packing of its
/// units, with the FIRST transaction able to reserve `first_read` rows of read
/// budget. The caller passes the destination's row count when asking "would this
/// fit ONE transaction" (that path scans the destination inside it), and `0` when
/// packing the chunks it will actually commit — a split chunk continues the
/// allocator from a snapshot and scans nothing.
fn graft_chunk_lengths(units: &[GraftUnit], first_read: usize) -> Vec<usize> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut first = true;
    while start < units.len() {
        let mut room = TX_CHUNK_BUDGET.saturating_sub(if first {
            first_read.saturating_mul(TX_READ_ROW_BYTES)
        } else {
            0
        });
        let mut n = 0usize;
        while start + n < units.len() {
            let cost = units[start + n].cost;
            if n > 0 && cost > room {
                break;
            }
            room = room.saturating_sub(cost);
            n += 1;
        }
        out.push(n);
        start += n;
        first = false;
    }
    out
}

/// One transaction's worth of a split graft, and the state the next one continues
/// from. Kept out of the loop so a serialization retry re-runs the body from the
/// same base (`base_next`, `inos`) instead of from a half-advanced one — the
/// `next` is passed IN and the grown map is passed OUT only after the commit.
struct GraftChunk {
    stats: crate::meta::GraftStats,
    next: i64,
    inos: std::collections::HashMap<i64, i64>,
    blobs: std::collections::HashSet<String>,
}

/// Node count one RESOLVED graft plan will write. A directory item brings its
/// whole subtree, so the plan's line count is not its size; the same source may
/// appear twice under two destinations and is then written twice, so `seen` is
/// per item rather than spanning the plan.
fn graft_plan_nodes(
    rows: &[(i64, String, Rec)],
    children: &std::collections::HashMap<i64, Vec<usize>>,
    plan: &[(usize, String)],
) -> usize {
    plan.iter()
        .map(|(idx, _)| graft_item_cost(rows, children, *idx).0)
        .sum()
}

fn graft_children(
    rows: &[(i64, String, Rec)],
) -> std::collections::HashMap<i64, Vec<usize>> {
    let mut m: std::collections::HashMap<i64, Vec<usize>> = Default::default();
    for (i, (parent, _, _)) in rows.iter().enumerate() {
        // `ROOT_PARENT` is not a row id, so the root row (parent = 0) has no
        // parent to be listed under — the same skip the file arm makes for
        // `parent_id IS NULL`.
        if *parent != ROOT_PARENT {
            m.entry(*parent).or_default().push(i);
        }
    }
    m
}

/// Resolve a plan path (`a/b/c`) against the source rows. `None` means the path
/// does not exist in that tree — the caller names the view in the error.
fn graft_resolve(
    rows: &[(i64, String, Rec)],
    children: &std::collections::HashMap<i64, Vec<usize>>,
    root: i64,
    path: &str,
) -> Option<usize> {
    let mut parent = root;
    let mut cur: Option<usize> = None;
    for comp in path.split('/') {
        let idx = *children
            .get(&parent)?
            .iter()
            .find(|&&k| rows[k].1 == comp)?;
        parent = rows[idx].2.id;
        cur = Some(idx);
    }
    cur
}

/// `mkdir -p` inside the destination tree: walk `path` from the root, creating
/// any missing directory. Returns the directory's row id.
fn graft_ensure_dir(
    tx: &Tx,
    keys: &Keys,
    w: &mut RowWriter,
    path: &str,
    ctx: &mut GraftFdbCtx,
) -> Result<i64, TxErr> {
    let mut cur = crate::meta::ROOT_INO;
    if path.is_empty() {
        return Ok(cur);
    }
    for comp in path.split('/') {
        let key = keys.name(cur, comp);
        match tx.get(&key)? {
            Some(v) => {
                let rec = Rec::unpack(&v).ok_or_else(|| TxErr::Hard(keys_corrupt(&key)))?;
                if !rec.is_dir {
                    return Err(TxErr::Hard(MetaError::Db(format!(
                        "'{comp}' in the destination path is a file, not a directory"
                    ))));
                }
                cur = rec.id;
            }
            None => {
                let id = ctx.next;
                ctx.next += 1;
                let t = secs_now();
                let rec = Rec {
                    id,
                    is_dir: true,
                    data_ref: None,
                    link_target: None,
                    mode: 0o755,
                    uid: crate::platform::geteuid(),
                    gid: crate::platform::getgid(),
                    mtime: t,
                    atime: t,
                    ctime: t,
                    ino: id,
                };
                put_row_tx(tx, keys, w, cur, comp, &rec)?;
                ctx.stats.nodes += 1;
                ctx.stats.dirs += 1;
                cur = id;
            }
        }
    }
    Ok(cur)
}

/// Copy one source entry (and, for a directory, its whole subtree) under `parent`
/// as `name`. Rows get fresh ids/ino numbers in the destination; the source's
/// bytes are referenced, never copied.
#[allow(clippy::too_many_arguments)]
fn graft_copy(
    tx: &Tx,
    keys: &Keys,
    w: &mut RowWriter,
    rows: &[(i64, String, Rec)],
    children: &std::collections::HashMap<i64, Vec<usize>>,
    idx: usize,
    parent: i64,
    name: &str,
    ctx: &mut GraftFdbCtx,
) -> Result<(), TxErr> {
    let is_dir = rows[idx].2.is_dir;
    let src_id = rows[idx].2.id;
    let Some(dir_id) = graft_copy_one(tx, keys, w, rows, idx, parent, name, ctx)? else {
        return Ok(());
    };
    if !is_dir {
        return Ok(());
    }
    if let Some(kids) = children.get(&src_id) {
        for k in kids {
            let child_name = rows[*k].1.clone();
            graft_copy(tx, keys, w, rows, children, *k, dir_id, &child_name, ctx)?;
        }
    }
    Ok(())
}

/// The entry half of [`graft_copy`]: create this ONE source entry under `parent`
/// as `name` (or merge into the directory already there), without descending.
///
/// Returns the destination directory id children belong under, or `None` when
/// there are none to copy: a newly created file, or a file `--skip-existing`
/// skipped. Split grafts call this directly so a directory too large for one
/// transaction can still be placed, one entry per unit.
fn graft_copy_one(
    tx: &Tx,
    keys: &Keys,
    w: &mut RowWriter,
    rows: &[(i64, String, Rec)],
    idx: usize,
    parent: i64,
    name: &str,
    ctx: &mut GraftFdbCtx,
) -> Result<Option<i64>, TxErr> {
    let r = &rows[idx].2;
    let key = keys.name(parent, name);
    let dir_id = match tx.get(&key)? {
        None => {
            let id = ctx.next;
            ctx.next += 1;
            // Hard links: every source name sharing `r.ino` maps to ONE fresh
            // destination inode (the first one it gets), which is what keeps a
            // linked pair linked after the graft.
            let ino = *ctx.inos.entry(r.ino).or_insert(id);
            let rec = Rec {
                id,
                is_dir: r.is_dir,
                data_ref: r.data_ref.clone(),
                link_target: r.link_target.clone(),
                mode: r.mode,
                uid: r.uid,
                gid: r.gid,
                mtime: r.mtime,
                atime: r.atime,
                ctime: r.ctime,
                ino,
            };
            put_row_tx(tx, keys, w, parent, name, &rec)?;
            ctx.stats.nodes += 1;
            if r.is_dir {
                ctx.stats.dirs += 1;
            }
            if let Some(dr) = &r.data_ref {
                ctx.blobs.insert(dr.clone());
            }
            if !r.is_dir {
                return Ok(None);
            }
            id
        }
        Some(v) => {
            let existing = Rec::unpack(&v).ok_or_else(|| TxErr::Hard(keys_corrupt(&key)))?;
            if !ctx.skip_existing {
                return Err(TxErr::Hard(MetaError::Db(format!(
                    "'{name}' already exists in the destination — rename it in the plan \
                     or pass --skip-existing"
                ))));
            }
            if !existing.is_dir || !r.is_dir {
                ctx.stats.skipped += 1;
                return Ok(None);
            }
            // Both are directories: keep the one already there and merge the
            // missing children into it (re-running a curation script must not
            // fail on the skeleton it made last time).
            existing.id
        }
    };
    Ok(Some(dir_id))
}

impl TxErr {
    /// A non-retriable error from *before* a body could even run (transaction
    /// creation) is reported the way PG reports a checkout failure: as a db error.
    fn hard_or_exhausted(self) -> MetaError {
        match self {
            TxErr::Hard(me) => me,
            TxErr::Retry(e) => MetaError::Db(format!("cannot start transaction: {e}")),
        }
    }
}

/// The record stored under a name key: `COLS minus parent_id/name`, i.e. the same
/// field order as `pg_store::COLS` so a diff between backends reads as a delta and
/// not a re-invention (§2). New fields may only be appended, and readers must
/// tolerate a short tuple, so that an old cluster stays readable after an upgrade.
#[derive(Clone, Debug)]
pub struct Rec {
    pub id: i64,
    pub is_dir: bool,
    pub data_ref: Option<String>,
    pub link_target: Option<String>,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub mtime: i64,
    pub atime: i64,
    pub ctime: i64,
    pub ino: i64,
}

impl Rec {
    /// §2's value tuple, in order.
    pub fn pack(&self) -> Vec<u8> {
        (
            self.id,
            self.is_dir,
            self.data_ref.as_deref(),
            self.link_target.as_deref(),
            self.mode as i64,
            self.uid as i64,
            self.gid as i64,
            self.mtime,
            self.atime,
            self.ctime,
            self.ino,
        )
            .pack_to_vec()
    }

    /// Decode a value written by [`Rec::pack`}, tolerating a trailing-short tuple.
    pub fn unpack(bytes: &[u8]) -> Option<Rec> {
        // §2's value tuple has 11 columns, so it is far past the 12-element
        // arity the crate's tuple impls stop at, and a reader that named each
        // column's type could not satisfy "tolerate a short tuple" either.
        // `Vec<Element>` is the flat depth-0 form the packer actually produced:
        // decode positionally and let a missing column be `None`.
        let items = Vec::<Element>::unpack_root(bytes).ok()?;
        let i = |k: usize| -> Option<i64> { items.get(k)?.as_i64() };
        let opt_i = |k: usize| -> Option<i64> { items.get(k).and_then(|e| e.as_i64()) };
        let s = |k: usize| -> Option<Option<String>> {
            match items.get(k)? {
                Element::String(c) => Some(Some(c.to_string())),
                Element::Nil => Some(None),
                _ => None,
            }
        };
        // §2: new columns may only be APPENDED, so the only short tuples that can
        // exist are ones cut at the tail. The first eight columns are the original
        // frozen set and are required; the appended three fall back the way the
        // semantics demand rather than the way zero happens to be convenient:
        // a missing `ino` is the id (§4's C1 parity, and §2's "写入即带 ino"
        // guarantee), and a missing `atime`/`ctime` follows `mtime` — which is
        // what `set_times` would have written into them anyway.
        let mtime = i(7)?;
        Some(Rec {
            id: i(0)?,
            is_dir: matches!(items.get(1)?, Element::Bool(true)),
            data_ref: s(2).flatten(),
            link_target: s(3).flatten(),
            mode: i(4)? as u32,
            uid: i(5)? as u32,
            gid: i(6)? as u32,
            mtime,
            atime: opt_i(8).unwrap_or(mtime),
            ctime: opt_i(9).unwrap_or(mtime),
            ino: opt_i(10).unwrap_or_else(|| i(0).unwrap_or(0)),
        })
    }
}

fn secs_now() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// The trait surface lives HERE, not in the dispatcher: `MetaProvider` is the
// vocabulary every backend speaks, and implementing it on `FdbStore` is what lets
// `MetaStore`'s third arm be a one-line delegation instead of a second copy of
// the semantics. `PgEngine` follows the same shape.
impl MetaProvider for FdbStore {
    fn lookup(&self, parent: i64, name: &str) -> Option<NodeInfo> {
        self.lookup(parent, name)
    }
    fn getattr(&self, ino: i64) -> Option<NodeInfo> {
        self.getattr(ino)
    }
    fn readdir(&self, parent: i64) -> Vec<NodeInfo> {
        self.readdir(parent)
    }
    fn readdir_page(&self, parent: i64, offset: usize, limit: usize) -> Vec<NodeInfo> {
        self.readdir_page(parent, offset, limit)
    }
    fn supports_paged_readdir(&self) -> bool {
        self.supports_paged_readdir()
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
        self.create_node(parent, name, is_dir, data_ref, mode, uid, gid)
    }
    fn create_symlink(
        &self,
        parent: i64,
        name: &str,
        target: &str,
        uid: u32,
        gid: u32,
    ) -> Result<i64, MetaError> {
        self.create_symlink(parent, name, target, uid, gid)
    }
    fn link_node(&self, src_ino: i64, new_parent: i64, new_name: &str) -> Result<i64, MetaError> {
        self.link_node(src_ino, new_parent, new_name)
    }
    fn unlink(&self, parent: i64, name: &str) -> Result<Option<String>, MetaError> {
        self.unlink(parent, name)
    }
    fn rename(
        &self,
        parent: i64,
        name: &str,
        new_parent: i64,
        new_name: &str,
        noreplace: bool,
    ) -> Result<Option<String>, MetaError> {
        self.rename(parent, name, new_parent, new_name, noreplace)
    }
    fn set_mode(&self, ino: i64, mode: u32) {
        self.set_mode(ino, mode);
    }
    fn set_uid_gid(&self, ino: i64, uid: u32, gid: u32) {
        self.set_uid_gid(ino, uid, gid);
    }
    fn set_times(&self, ino: i64, atime: Option<i64>, mtime: Option<i64>) {
        self.set_times(ino, atime, mtime);
    }
    fn touch_mtime(&self, ino: i64) {
        self.touch_mtime(ino);
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
        self.lock_op(ino, owner, start, end, ty, pid, sid)
    }
    fn get_lock(
        &self,
        ino: i64,
        owner: u64,
        start: u64,
        end: u64,
        ty: LockType,
    ) -> Option<LockConflict> {
        self.get_lock(ino, owner, start, end, ty)
    }
    fn release_locks_of_owner(&self, owner: u64) {
        self.release_locks_of_owner(owner);
    }
    fn renew_locks(&self, sid: u64) {
        self.renew_locks(sid);
    }
    fn fence_handle(&self) -> std::sync::Arc<AtomicU64> {
        self.fence_handle()
    }
    /// Re-read the epoch key rather than reporting the cached handle: after a
    /// failover the promoted authority has raised it, and the data plane needs
    /// the new token to unfence itself (`pg_store`'s PG counterpart reads the
    /// same row for the same reason).
    fn fetch_fence_epoch(&self) -> u64 {
        self.get_epoch().max(0) as u64
    }
    fn dump_tree(&self) -> Vec<(i64, i64, String, bool, Option<String>)> {
        self.dump_tree()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The split planner has one job that must never break: every chunk fits the
    /// budget, and the chunks together are exactly the rows. A planner that drops a
    /// row would import a tree with a hole; one that overruns would turn the split
    /// back into a `2101`.
    #[test]
    fn chunk_lengths_cover_every_row_within_budget() {
        // The frontier case: 7,700 rows with one ref each used to be the ceiling.
        // Chunked, it must become 2 transactions (70% of 10 MB is 7 MB).
        for (rows, blobs, read_rows) in [
            (1usize, 0usize, 0usize),
            (7_700, 7_699, 0),
            (17_000, 0, 0),
            (50_000, 50_000, 0),
        ] {
            let lens = chunk_lengths(rows, blobs, read_rows);
            assert_eq!(lens.iter().sum::<usize>(), rows, "covers {rows} rows");
            assert!(lens.iter().all(|&n| n > 0), "no empty transaction");
            let per_row = TX_ROW_BYTES + blobs.saturating_mul(TX_BLOB_BYTES).div_ceil(rows);
            for (i, &n) in lens.iter().enumerate() {
                let read = if i == 0 { read_rows } else { 0 };
                let cost = n * per_row + read * TX_READ_ROW_BYTES;
                assert!(
                    cost <= TX_CHUNK_BUDGET.max(per_row),
                    "chunk {i} costs {cost} > {TX_CHUNK_BUDGET}"
                );
            }
        }
        // A dump that already fits stays ONE transaction: the split must not
        // change the all-or-nothing shape of every small import.
        assert_eq!(chunk_lengths(1_000, 1_000, 0).len(), 1);
        assert_eq!(chunk_lengths(7_700, 0, 0).len(), 1);
        assert!(chunk_lengths(0, 0, 0).is_empty());
    }

    /// One directory too large for a transaction is split into its own entry plus
    /// its children — the case a plan-item split alone would still refuse.
    #[test]
    fn graft_units_split_an_oversized_directory() {
        let dir = |id: i64, ino: i64| Rec {
            id,
            is_dir: true,
            data_ref: None,
            link_target: None,
            mode: 0o755,
            uid: 0,
            gid: 0,
            mtime: 0,
            atime: 0,
            ctime: 0,
            ino,
        };
        let file = |id: i64| Rec {
            data_ref: Some(format!("d{id}")),
            is_dir: false,
            ..dir(id, id)
        };
        // root(1) -> big(2) -> 8,000 files. At 625+660 B/row that subtree is past
        // the 7 MB budget, so it must split; a small sibling must not.
        let mut rows: Vec<(i64, String, Rec)> = vec![(ROOT_PARENT, String::new(), dir(1, 1))];
        rows.push((1, "big".into(), dir(2, 2)));
        for i in 0..8_000i64 {
            rows.push((2, format!("f{i}"), file(1_000 + i)));
        }
        rows.push((1, "small".into(), dir(9_000, 9_000)));
        rows.push((9_000, "one".into(), file(9_001)));
        let children = graft_children(&rows);
        // The index of "small" is its position in `rows`.
        let small_idx = rows.iter().position(|(_, n, _)| n == "small").unwrap();
        let plan = vec![(1usize, "big".to_string()), (small_idx, "small".to_string())];
        let units = graft_units(&rows, &children, &plan, TX_CHUNK_BUDGET);
        // big: its own entry (dir keeps its mode/uid) then one unit per child;
        // small: one untouched subtree unit.
        assert!(!units[0].subtree, "oversized dir becomes an entry-only unit");
        assert_eq!(units[0].idx, 1);
        assert_eq!(units.iter().filter(|u| u.subtree).count(), 8_001);
        assert_eq!(units.len(), 8_002);
        assert_eq!(units.last().unwrap().full, "small");
        let chunks = graft_chunk_lengths(&units, 0);
        assert_eq!(chunks.iter().sum::<usize>(), units.len());
        assert!(chunks.len() > 1, "an 8k-file directory needs several transactions");
    }

    /// The on-cluster byte format is irreversible once written (D1/D2), so it is
    /// pinned here rather than left to a prose table. Every expected byte below is
    /// derived from the tuple layer's published typecodes: BYTES 0x01, STRING 0x02,
    /// NIL 0x00 terminator, INT 0x14 + n length bytes, and a depth-0 tuple writing
    /// its elements inline with no NESTED wrapper.
    #[test]
    fn key_encoding_is_byte_frozen() {
        let k = Keys::for_view("v1");

        // root: 0x01 "grovefs" 0x00 — the BYTES typecode, the raw prefix, and the
        // terminator the subspace packer writes for an element packed at depth > 0.
        // View: + 0x02 "v1" 0x00, family tag: + 0x02 "n" 0x00, then the key tuple's
        // own elements inline (0x15 = INT of length 1, so 7 is 0x15 0x07).
        assert_eq!(
            k.name(7, "a"),
            b"\x01grovefs\x00\x02v1\x00\x02n\x00\x15\x07\x02a\x00".to_vec()
        );
        // ino pointer key: same family tag, one int
        assert_eq!(
            k.ino(1),
            b"\x01grovefs\x00\x02v1\x00\x02i\x00\x15\x01".to_vec()
        );
        // epoch: (v,'c','epoch')
        assert_eq!(
            k.epoch(),
            b"\x01grovefs\x00\x02v1\x00\x02c\x00\x02epoch\x00".to_vec()
        );
        // allocator marks are two distinct keys (§4)
        assert_ne!(k.alloc(AllocKind::Id), k.alloc(AllocKind::Ino));
        assert_eq!(
            k.alloc(AllocKind::Id),
            b"\x01grovefs\x00\x02v1\x00\x02m\x00\x02id\x00".to_vec()
        );
        // the ledger lives OUTSIDE the view prefix (D5): same root, different tag
        assert_eq!(
            k.ledger("abc"),
            b"\x01grovefs\x00\x02refs\x00\x02abc\x00".to_vec()
        );
        assert!(!k.ledger("abc").starts_with(k.view_prefix().bytes()));
    }

    /// §1's reason for using the tuple layer at all: two different view names must
    /// not share a prefix the way `"ab"` + `"c"` and `"a"` + `"bc"` would when
    /// hand-concatenated.
    #[test]
    fn view_prefixes_are_unambiguous_and_ordered() {
        let a = Keys::for_view("a").view_prefix().bytes().to_vec();
        let ab = Keys::for_view("ab").view_prefix().bytes().to_vec();
        assert!(!ab.starts_with(&a), "prefix-free encoding required");

        // Negative ids sort before positive ones because the tuple layer is
        // sign-preserving (§9's hard requirement against hand-rolled big-endian).
        let neg = Keys::for_view("v").name(-1, "x");
        let pos = Keys::for_view("v").name(1, "x");
        assert!(neg < pos);

        // Within one directory the keys sort by name, which is what makes readdir
        // an unsorted range scan (§3.3).
        let p = Keys::for_view("v");
        let mut names: Vec<Vec<u8>> = ["b", "a", "c"]
            .iter()
            .map(|n| p.name(5, n))
            .collect();
        names.sort();
        assert_eq!(names, vec![p.name(5, "a"), p.name(5, "b"), p.name(5, "c")]);
    }

    /// readdir's range bound must cover exactly the children of one parent, and a
    /// sibling directory's entries must fall outside it.
    #[test]
    fn children_range_bounds_one_directory_exactly() {
        let k = Keys::for_view("v1");
        let opt = k.children_range(7);
        let (begin, end) = (opt.begin.key().to_vec(), opt.end.key().to_vec());
        let inside = |name: &str| {
            let key = k.name(7, name);
            key >= begin && key < end
        };
        assert!(inside("a") && inside("zzz"));
        // A byte range is a bound, not a validity filter: `name(7, "")` does sit
        // inside it. It can still never be PRESENT, because `create_node` refuses
        // an empty name (§2: names are never empty), and the one empty-named key
        // in the tree is the root's `name(0, "")` — whose parent 0 is not a
        // directory and is never listed, which is what keeps the root out of
        // every `readdir`.
        assert!(inside(""));
        assert!(k.name(ROOT_PARENT, "") < begin, "the root lies outside any real directory's range");
        // parent 8's entry must not be swept up by parent 7's scan
        let other = k.name(8, "a");
        assert!(!(other >= begin && other < end));
        // the cursor form must exclude the entry it starts after and include the
        // next one (§3.3 paging correctness). A key selector carries its anchor
        // key plus the resolution rule, so the property to pin is the rule —
        // `first_greater_than` is (or_equal, +1), which resolves to the first key
        // strictly after the cursor.
        let cursor = k.name(7, "b");
        let cur = k.children_range_after(7, "b");
        assert_eq!(cur.begin.key(), cursor.as_slice());
        assert!(cur.begin.or_equal() && cur.begin.offset() == 1);
        assert!(k.name(7, "c") > cursor, "the next entry must sort after the cursor");
        assert!(k.name(7, "a") < cursor, "and the previous one before it");
        assert_eq!(cur.end.key().to_vec(), end);
        assert!(!cur.end.or_equal() && cur.end.offset() == 1);
    }

    #[test]
    fn record_round_trips_and_tolerates_short_tuples() {
        let rec = Rec {
            id: 42,
            is_dir: false,
            data_ref: Some("sha:abc".to_string()),
            link_target: None,
            mode: 0o644,
            uid: 1000,
            gid: 1000,
            mtime: 1_700_000_000,
            atime: -5, // negative timestamps must survive (§9: sign-preserving)
            ctime: 1_700_000_000,
            ino: 42,
        };
        let bytes = rec.pack();
        let back = Rec::unpack(&bytes).expect("decode");
        assert_eq!(back.id, 42);
        assert!(!back.is_dir);
        assert_eq!(back.data_ref.as_deref(), Some("sha:abc"));
        assert_eq!(back.link_target, None);
        assert_eq!(back.mode, 0o644);
        assert_eq!(back.atime, -5);
        assert_eq!(back.ino, 42);

        // Forward compatibility: a value written before `atime`/`ctime`/`ino` were
        // appended must still decode, with `ino` falling back to the id (§4's C1
        // parity) and the two timestamps following `mtime`.
        let short = (8i64, true, None::<&str>, None::<&str>, 0o755i64, 0i64, 0i64, 5i64)
            .pack_to_vec();
        let decoded = Rec::unpack(&short).expect("short tuple decodes");
        assert_eq!(decoded.id, 8);
        assert_eq!(decoded.ino, 8, "a missing ino is the id, never 0");
        assert_eq!(decoded.atime, 5);
        assert_eq!(decoded.ctime, 5);
        // But a value cut inside the original eight columns is not a version of
        // this format — it is corruption, and must decode to nothing.
        let truncated = (8i64, true, None::<&str>).pack_to_vec();
        assert!(Rec::unpack(&truncated).is_none());
    }

    #[test]
    fn i64_and_pointer_values_decode() {
        assert_eq!(unpack_i64(&(-7i64).pack_to_vec()), Some(-7));
        assert_eq!(unpack_i64(b"not a tuple"), None);
        let ptr = (12i64, "name").pack_to_vec();
        assert_eq!(unpack_ptr(&ptr), Some((12, "name".to_string())));
    }

    #[test]
    fn config_parse_accepts_cluster_forms() {
        let c = FdbConfig::parse("fdb:///tmp/x.cluster", "v1").unwrap();
        assert_eq!(c.cluster_file, "/tmp/x.cluster");
        let c = FdbConfig::parse("fdb://?cluster=/tmp/y.cluster", "v1").unwrap();
        assert_eq!(c.cluster_file, "/tmp/y.cluster");
        assert!(FdbConfig::parse("postgres://h/db", "v1").is_err());
        // no path and no env var -> a clear error, not a silent /etc default
        std::env::remove_var("GROVEFS_FDB_CLUSTER_FILE");
        assert!(FdbConfig::parse("fdb://", "v1").is_err());
    }

    #[test]
    fn config_rejects_view_names_pg_would_reject() {
        assert!(FdbConfig::parse("fdb:///tmp/x.cluster", "").is_err());
        let long = "v".repeat(300);
        assert!(FdbConfig::parse("fdb:///tmp/x.cluster", &long).is_err());
    }

    #[test]
    fn only_abort_codes_are_retriable() {
        for c in [
            CODE_NOT_COMMITTED,
            CODE_TRANSACTION_TOO_OLD,
            CODE_TRANSACTION_TIMED_OUT,
            CODE_READY_WRITE_DISABLED,
            CODE_COMMIT_ON_FIRST_PROXY,
            CODE_PROXY_METADATA_VERSION_MISMATCH,
        ] {
            assert!(is_retriable(c), "{c} must retry");
        }
        // §6: these must surface, or a bug becomes a hang.
        for c in [2000 /* key_outside_legal_range */, 2101 /* transaction_too_large */] {
            assert!(!is_retriable(c), "{c} must not retry");
        }
    }

    #[test]
    fn backoff_doubles_and_caps() {
        // Same shape as pg_store: 200us doubling, capped at 25ms.
        let mut delay = SERIALIZATION_BACKOFF_MIN;
        let mut seen = vec![];
        for _ in 0..10 {
            seen.push(delay);
            delay = (delay * 2).min(SERIALIZATION_BACKOFF_MAX);
        }
        assert_eq!(seen[0], std::time::Duration::from_micros(200));
        assert_eq!(seen.last().unwrap(), &SERIALIZATION_BACKOFF_MAX);
        assert!(seen.windows(2).all(|w| w[1] >= w[0]));
    }

    /// §3.7's encodings, isolated from the cluster: every lock path below is a
    /// prefix scan whose entries must reconstruct their primary key exactly, so
    /// the four key forms and the value codec are the parts that can be settled
    /// without a running database — and the ones whose failure mode is a lock that
    /// silently survives (or vanishes from) one of its four keys.
    #[test]
    fn lock_keys_reconstruct_their_primary_exactly() {
        let keys = Keys::for_view("v");
        let (ino, s, e, owner, sid, expires) = (41i64, 5i64, 9i64, 7i64, 3i64, 1_700_000_060i64);
        let pk = keys.lock(ino, s, e, owner);
        // Each secondary key decodes back to the exact primary key it mirrors.
        let xk = keys.lock_expire(expires, ino, s, e, owner);
        assert_eq!(keys.decode_lock_expire(&xk), Some((expires, ino, s, e, owner)));
        assert_eq!(keys.decode_lock(&pk), Some((ino, s, e, owner)));
        let ok_ = keys.lock_owner(owner, ino, s, e);
        assert_eq!(
            keys.decode_lock_owner(&ok_),
            Some((owner, ino, s, e)),
            "the owner mirror must name l_end too, or it cannot find its row"
        );
        let sk = keys.lock_sid(sid, ino, s, e, owner);
        assert_eq!(
            keys.decode_lock_sid(&sk),
            Some((sid, ino, s, e, owner)),
            "the session mirror must name owner too: two owners in one session can hold the same start"
        );
        // And each is inside the range its scan uses — a key that falls outside
        // the prefix its own operation scans is an entry no release can ever find.
        let (b, en) = keys.family(TAG_LOCK).subspace(&(ino,)).range();
        assert!(pk.as_slice() >= b.as_slice() && pk.as_slice() < en.as_slice());
        let sw = keys.lock_expired_upto(expires + 1);
        let (bb, eb) = (sw.begin.key().to_vec(), sw.end.key().to_vec());
        assert!(
            xk.as_slice() >= bb.as_slice() && xk.as_slice() < eb.as_slice(),
            "a lock expiring before `now + 1` must be inside the sweep range"
        );
        // The sweep range must NOT reach a lock that has not expired: its end is
        // the pack of `(now,)`, so `expires_at == now` is excluded, matching
        // `DELETE ... WHERE expires_at < $1` and `get_lock`'s `>=` filter.
        let live = keys.lock_expired_upto(expires);
        let e2 = live.end.key().to_vec();
        assert!(
            xk.as_slice() >= e2.as_slice(),
            "expires_at == now is a live lease, not a swept one"
        );
        // The owner/session mirrors are scanned by prefix, so each must sit inside
        // the prefix its release or renewal path reads.
        let (ob, oe) = keys.family(TAG_LOCK_OWNER).subspace(&(owner,)).range();
        assert!(ok_.as_slice() >= ob.as_slice() && ok_.as_slice() < oe.as_slice());
        let (sb, se) = keys.family(TAG_LOCK_SID).subspace(&(sid,)).range();
        assert!(sk.as_slice() >= sb.as_slice() && sk.as_slice() < se.as_slice());
    }

    /// The value tuple and the overlap condition, both spelled as PG spells them.
    #[test]
    fn lock_value_and_overlap_follow_pg() {
        let v = LockVal {
            ltype: 1,
            pid: 22,
            sid: 33,
            expires: 44,
        };
        assert_eq!(LockVal::unpack(&v.pack()), Some(v));
        // A short or foreign tuple decodes to None rather than to a zeroed lock:
        // `lock_op` would otherwise grant against a lock whose pid/sid read 0.
        assert_eq!(LockVal::unpack(&44i64.pack_to_vec()), None);
        // PG: `l_start<=$3 AND l_end>=$4` with (e, s) crossed. Same condition,
        // including the crossed form itself: the stored row's START is compared
        // against the request's END and its END against the request's START, which
        // is what makes an inclusive byte range overlap by exactly one byte.
        assert!(locks_overlap(5, 9, 9, 12), "sharing only the last byte");
        assert!(locks_overlap(5, 9, 4, 6), "a request reaching into the stored range");
        assert!(locks_overlap(0, i64::MAX, 7, 7), "an EOF lock covers a byte");
        assert!(!locks_overlap(5, 9, 10, 12), "disjoint ranges do not overlap");
        assert!(!locks_overlap(10, 12, 5, 9), "and not in the other direction");
        assert!(
            !locks_overlap(5, 9, 0, 4),
            "byte 4 and byte 5 are adjacent, not overlapping — ranges are inclusive"
        );
    }

    /// §4's reservation arithmetic, isolated because the failure it prevents is
    /// invisible in a test that only counts rows: an id that equals `ROOT_INO`
    /// overwrites the root's own `'d`/`'i` keys, so the tree still LOOKS intact
    /// until the first getattr on `/` reads a file record through the root's id.
    #[test]
    fn id_block_bounds_are_root_safe() {
        // A freshly seeded view: `ensure_root` raises the counter to ROOT_INO, so
        // the first block ends at 1 + ID_BLOCK and must START at 2.
        let hi = crate::meta::ROOT_INO + ID_BLOCK;
        let (next, limit) = block_bounds(hi, ID_BLOCK);
        assert_eq!((next, limit), (2, hi));
        assert!(
            next > crate::meta::ROOT_INO,
            "the first block of a seeded view must never reissue the root's id"
        );
        // Contiguity across blocks: the second reservation picks up exactly where
        // the first ended, so ids are neither skipped nor (worse) reused.
        assert_eq!(block_bounds(hi + ID_BLOCK, ID_BLOCK).0, limit + 1);
        // A counter that was never seeded (a view whose root key vanished) still
        // cannot go below 1 — the clamp is a floor, not an offset.
        assert_eq!(block_bounds(ID_BLOCK, ID_BLOCK).0, 1);
    }

    /// The 0xff case of [`prefix_upper_bound`] is not theoretical: a subspace whose
    /// last byte were 0xff has no exclusive bound inside its own keyspace, and
    /// carrying into the previous byte would clear a NEIGHBOURING view's keys. The
    /// tuple layer's `0x00` string terminator means GroveFS never generates such a
    /// prefix, so `clear_prefix` refusing is a guard that must stay armed.
    #[test]
    fn prefix_upper_bound_refuses_an_overflow_instead_of_carrying() {
        assert_eq!(prefix_upper_bound(b"grovefs").unwrap(), b"groveft".to_vec());
        assert_eq!(prefix_upper_bound(b"grovefs\x00").unwrap(), b"grovefs\x01".to_vec());
        assert!(prefix_upper_bound(b"grovefs\xff").is_none());
        assert!(prefix_upper_bound(b"").is_none(), "an empty prefix is every key");
    }
}


//! Opt-in, per-stage latency + counter telemetry.
//!
//! This is a **development / diagnostic facility**, not a throughput claim: it
//! exists so a developer or test harness can see *where* wall-clock time goes
//! inside a read/write (or metadata) operation — key resolution vs the data
//! backend vs a remote TCP round-trip vs per-server striping fan-out vs the
//! SQLite/PostgreSQL query — and aggregate those into offline percentiles
//! (p50/p90/p99/max) to locate a bottleneck. Nothing here is read back into the
//! filesystem's correctness model, and the TOS paper deliberately makes no
//! throughput/scalability claims, so no number produced here belongs in it.
//!
//! Cost when disabled (the default): each instrument site does one relaxed
//! `AtomicBool` load and a branch; no `Instant::now()`, no allocation, no lock.
//! When enabled, timings land in a high-dynamic-range histogram per stage,
//! sharded by worker thread to keep the hot path off a single global lock.

use hdrhistogram::Histogram;
use parking_lot::Mutex;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Upper bound of a single recorded latency, in microseconds. Anything slower
/// (pathological) saturates into the top bucket; 60 s is far above any expected
/// per-stage cost while keeping the HDR bucket layout compact.
const HIST_HIGH_US: u64 = 60_000_000;
/// Significant-figure precision (2 halves memory vs 3; plenty for triage).
const HIST_SIGFIG: u8 = 2;
/// Thread shards per stage. Worker threads hash into one shard and record
/// without a shared lock across all threads; `report` merges the shards. The
/// merge is only done when a report is generated (offline), never per op.
const SHARDS: usize = 8;

/// Every instrumented *duration*. A fixed set so the registry is a plain array
/// indexed by `Stage as usize` — no map lookup on the hot path. Keep `ALL` and
/// [`Stage::name`] in sync (a unit test asserts `ALL.len()` covers every
/// variant's name).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stage {
    // --- outermost per-FUSE-op end-to-end latency (the real "read/write speed") ---
    OpRead,
    OpWrite,
    // --- P0: data read / write path ---
    ReadKeyResolve,
    ReadBackendTotal,
    WriteKeyResolve,
    WriteBackendTotal,
    WriteTouchMtime,
    IoLocalRead,
    IoLocalWrite,
    IoLocalLen,
    IoRemoteRead,
    IoRemoteWrite,
    IoRemoteLen,
    IoRemoteChunked,
    IoRemotePoolWait,
    IoShardRead,
    IoShardWrite,
    IoShardServerOp,
    IoShardSplit,
    IoShardConcat,
    // --- P1: metadata path ---
    MetaLookup,
    MetaGetattr,
    MetaReaddir,
    MetaReaddirPage,
    MetaCreate,
    MetaUnlink,
    MetaRename,
    MetaSetattr,
    MetaStatfs,
    SqliteRead,
    SqliteWriteTxn,
    SqliteFinalizeCount,
    PgQuery,
    PgWriteTxn,
    PgPoolWait,
    PgSsiRetry,
    RemoteMetaRpc,
}

impl Stage {
    pub const ALL: &'static [Stage] = &[
        Stage::OpRead,
        Stage::OpWrite,
        Stage::ReadKeyResolve,
        Stage::ReadBackendTotal,
        Stage::WriteKeyResolve,
        Stage::WriteBackendTotal,
        Stage::WriteTouchMtime,
        Stage::IoLocalRead,
        Stage::IoLocalWrite,
        Stage::IoLocalLen,
        Stage::IoRemoteRead,
        Stage::IoRemoteWrite,
        Stage::IoRemoteLen,
        Stage::IoRemoteChunked,
        Stage::IoRemotePoolWait,
        Stage::IoShardRead,
        Stage::IoShardWrite,
        Stage::IoShardServerOp,
        Stage::IoShardSplit,
        Stage::IoShardConcat,
        Stage::MetaLookup,
        Stage::MetaGetattr,
        Stage::MetaReaddir,
        Stage::MetaReaddirPage,
        Stage::MetaCreate,
        Stage::MetaUnlink,
        Stage::MetaRename,
        Stage::MetaSetattr,
        Stage::MetaStatfs,
        Stage::SqliteRead,
        Stage::SqliteWriteTxn,
        Stage::SqliteFinalizeCount,
        Stage::PgQuery,
        Stage::PgWriteTxn,
        Stage::PgPoolWait,
        Stage::PgSsiRetry,
        Stage::RemoteMetaRpc,
    ];

    /// Stable snake-case label for reports (the CSV key and the human table).
    pub fn name(self) -> &'static str {
        match self {
            Stage::OpRead => "read.op_total",
            Stage::OpWrite => "write.op_total",
            Stage::ReadKeyResolve => "read.key_resolve",
            Stage::ReadBackendTotal => "read.backend_total",
            Stage::WriteKeyResolve => "write.key_resolve",
            Stage::WriteBackendTotal => "write.backend_total",
            Stage::WriteTouchMtime => "write.touch_mtime",
            Stage::IoLocalRead => "io.local.read",
            Stage::IoLocalWrite => "io.local.write",
            Stage::IoLocalLen => "io.local.len",
            Stage::IoRemoteRead => "io.remote.read_rtt",
            Stage::IoRemoteWrite => "io.remote.write_rtt",
            Stage::IoRemoteLen => "io.remote.len_rtt",
            Stage::IoRemoteChunked => "io.remote.chunked",
            Stage::IoRemotePoolWait => "io.remote.pool_wait",
            Stage::IoShardRead => "io.shard.read_total",
            Stage::IoShardWrite => "io.shard.write_total",
            Stage::IoShardServerOp => "io.shard.server_op",
            Stage::IoShardSplit => "io.shard.split",
            Stage::IoShardConcat => "io.shard.concat",
            Stage::MetaLookup => "meta.lookup",
            Stage::MetaGetattr => "meta.getattr",
            Stage::MetaReaddir => "meta.readdir",
            Stage::MetaReaddirPage => "meta.readdir_page",
            Stage::MetaCreate => "meta.create",
            Stage::MetaUnlink => "meta.unlink",
            Stage::MetaRename => "meta.rename",
            Stage::MetaSetattr => "meta.setattr",
            Stage::MetaStatfs => "meta.statfs",
            Stage::SqliteRead => "db.sqlite_read",
            Stage::SqliteWriteTxn => "db.sqlite_write_txn",
            Stage::SqliteFinalizeCount => "db.sqlite_finalize_count",
            Stage::PgQuery => "db.pg_query",
            Stage::PgWriteTxn => "db.pg_write_txn",
            Stage::PgPoolWait => "db.pg_pool_wait",
            Stage::PgSsiRetry => "db.pg_ssi_retry",
            Stage::RemoteMetaRpc => "remeta.rpc",
        }
    }

    /// Coarse grouping so the human report can section itself.
    fn group(self) -> &'static str {
        match self {
            Stage::OpRead
            | Stage::OpWrite
            | Stage::ReadKeyResolve
            | Stage::ReadBackendTotal
            | Stage::WriteKeyResolve
            | Stage::WriteBackendTotal
            | Stage::WriteTouchMtime => "op",
            Stage::IoLocalRead
            | Stage::IoLocalWrite
            | Stage::IoLocalLen
            | Stage::IoRemoteRead
            | Stage::IoRemoteWrite
            | Stage::IoRemoteLen
            | Stage::IoRemoteChunked
            | Stage::IoRemotePoolWait
            | Stage::IoShardRead
            | Stage::IoShardWrite
            | Stage::IoShardServerOp
            | Stage::IoShardSplit
            | Stage::IoShardConcat => "data-io",
            _ => "meta",
        }
    }
}

/// Monotonic non-timing counters: pool contention, retries, reconnects, GC, HA
/// fencing, byte volumes — cheap signals a bottleneck *explains* the timing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Counter {
    PoolLazyOpen,
    PgSsiRetry,
    RemoteReconnect,
    BroadcastTimeout,
    HaFencing,
    HaQuorumRefuse,
    LockRenew,
    LockExpireReclaim,
    GcFreed,
    BytesRead,
    BytesWritten,
}

impl Counter {
    pub const ALL: &'static [Counter] = &[
        Counter::PoolLazyOpen,
        Counter::PgSsiRetry,
        Counter::RemoteReconnect,
        Counter::BroadcastTimeout,
        Counter::HaFencing,
        Counter::HaQuorumRefuse,
        Counter::LockRenew,
        Counter::LockExpireReclaim,
        Counter::GcFreed,
        Counter::BytesRead,
        Counter::BytesWritten,
    ];
    pub fn name(self) -> &'static str {
        match self {
            Counter::PoolLazyOpen => "sqlite read-pool lazy-opens",
            Counter::PgSsiRetry => "pg ssi-retries",
            Counter::RemoteReconnect => "remote reconnects",
            Counter::BroadcastTimeout => "broadcast send timeouts",
            Counter::HaFencing => "ha fencing demotions",
            Counter::HaQuorumRefuse => "ha quorum refusals",
            Counter::LockRenew => "lock lease renewals",
            Counter::LockExpireReclaim => "lock expired reclaims",
            Counter::GcFreed => "gc objects freed",
            Counter::BytesRead => "bytes read",
            Counter::BytesWritten => "bytes written",
        }
    }
}

struct StageStat {
    shards: Vec<Mutex<Histogram<u64>>>,
    bytes: AtomicU64,
}

/// The whole registry. Kept independent of the process-global enable gate so
/// its math (record → shard → merge → percentiles) is unit-testable on a
/// standalone instance.
pub struct Registry {
    stages: Vec<StageStat>,
    counters: Vec<AtomicU64>,
}

fn new_hist() -> Histogram<u64> {
    Histogram::<u64>::new_with_bounds(1, HIST_HIGH_US, HIST_SIGFIG)
        .expect("valid HDR bounds (low=1 <= high, sigfig in range)")
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    pub fn new() -> Self {
        let stages = Stage::ALL
            .iter()
            .map(|_| StageStat {
                shards: (0..SHARDS).map(|_| Mutex::new(new_hist())).collect(),
                bytes: AtomicU64::new(0),
            })
            .collect();
        let counters = Counter::ALL.iter().map(|_| AtomicU64::new(0)).collect();
        Self { stages, counters }
    }

    #[inline]
    pub fn record(&self, stage: Stage, micros: u64, shard: usize) {
        let s = &self.stages[stage as usize];
        // saturating_record clamps above HIST_HIGH_US instead of erroring.
        s.shards[shard % SHARDS].lock().saturating_record(micros);
    }

    #[inline]
    pub fn add_bytes(&self, stage: Stage, n: u64) {
        self.stages[stage as usize]
            .bytes
            .fetch_add(n, Ordering::Relaxed);
    }

    #[inline]
    pub fn count(&self, c: Counter, delta: u64) {
        self.counters[c as usize].fetch_add(delta, Ordering::Relaxed);
    }

    /// Merge this stage's shards into one histogram (offline, report-time only).
    pub fn merged(&self, stage: Stage) -> Histogram<u64> {
        let mut m = new_hist();
        for h in &self.stages[stage as usize].shards {
            let g = h.lock();
            let _ = m.add(&*g); // identical bounds; AdditionError only on overflow past HIGH
        }
        m
    }

    /// Human-readable table + counter block. Empty stages are listed with 0 ops
    /// so the shape stays stable across dumps (diff-friendly for A/B).
    pub fn report_text(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        out.push_str("GroveFS per-stage latency telemetry  (durations in \u{b5}s)\n");
        out.push_str("  diagnostic tooling only \u{2014} not a throughput/scalability claim\n");
        out.push_str(&format!(
            "  snapshot @ {}\n\n",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
        ));
        let mut last_group = "";
        for &stage in Stage::ALL {
            let grp = stage.group();
            if grp != last_group {
                let _ = writeln!(out, "[{grp}]");
                last_group = grp;
            }
            let h = self.merged(stage);
            let n = h.len();
            let bytes = self.stages[stage as usize].bytes.load(Ordering::Relaxed);
            if n == 0 {
                let _ = writeln!(out, "  {:<26} ops=0", stage.name());
                continue;
            }
            let _ = writeln!(
                out,
                "  {:<26} ops={:<8} p50={:<8} p90={:<8} p99={:<9} max={:<9} mean={:<8} total_s={:<7} bytes={}",
                stage.name(),
                n,
                h.value_at_quantile(0.50),
                h.value_at_quantile(0.90),
                h.value_at_quantile(0.99),
                h.max(),
                format!("{:.0}", h.mean()),
                format!("{:.2}", h.len() as f64 * h.mean() / 1e6),
                bytes,
            );
        }
        let _ = writeln!(out, "\n[counters]");
        for &c in Counter::ALL {
            let v = self.counters[c as usize].load(Ordering::Relaxed);
            let _ = writeln!(out, "  {:<32} {v}", c.name());
        }
        out
    }

    /// CSV export for offline plotting (one row per stage).
    pub fn report_csv(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        let _ = writeln!(
            out,
            "stage,group,ops,p50_us,p90_us,p99_us,max_us,mean_us,total_us,bytes"
        );
        for &stage in Stage::ALL {
            let h = self.merged(stage);
            let n = h.len();
            let bytes = self.stages[stage as usize].bytes.load(Ordering::Relaxed);
            let total = if n > 0 {
                (n as f64 * h.mean()) as u64
            } else {
                0
            };
            let _ = writeln!(
                out,
                "{},{},{},{},{},{},{},{:.0},{},{},",
                stage.name(),
                stage.group(),
                n,
                if n > 0 { h.value_at_quantile(0.50) } else { 0 },
                if n > 0 { h.value_at_quantile(0.90) } else { 0 },
                if n > 0 { h.value_at_quantile(0.99) } else { 0 },
                h.max(),
                h.mean(),
                total,
                bytes,
            );
        }
        for &c in Counter::ALL {
            let v = self.counters[c as usize].load(Ordering::Relaxed);
            let _ = writeln!(out, "counter:{},count,,,,,,,{},,", c.name(), v);
        }
        out
    }
}

// ---- process-global gate + registry ----------------------------------------

static ENABLED: AtomicBool = AtomicBool::new(false);
static REGISTRY: OnceLock<Registry> = OnceLock::new();
static NEXT_SHARD: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// Each worker thread pins to a shard once, so `record` never rehashes.
    static SHARD: usize = NEXT_SHARD.fetch_add(1, Ordering::Relaxed) % SHARDS;
}

/// True while telemetry is on. Instrument sites read this (one relaxed load)
/// via [`start`]/[`add_bytes`]/[`count`] and skip all real work when off.
#[inline]
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// RAII latency guard: `let _t = stats::start(Stage::X);` records the elapsed
/// window when it drops (even on an error return / panic unwind, which is
/// exactly when you most want the sample). `None` when disabled.
pub struct Timer {
    stage: Stage,
    start: Instant,
}

impl Drop for Timer {
    fn drop(&mut self) {
        if let Some(r) = REGISTRY.get() {
            let micros = self.start.elapsed().as_micros().min(u64::MAX as u128) as u64;
            let shard = SHARD.with(|s| *s);
            r.record(self.stage, micros, shard);
        }
    }
}

/// Begin a timed window for `stage`, or `None` if telemetry is disabled (the
/// cheap default). The returned guard records on drop.
#[inline]
pub fn start(stage: Stage) -> Option<Timer> {
    if !enabled() {
        return None;
    }
    Some(Timer {
        stage,
        start: Instant::now(),
    })
}

/// Add `n` bytes to a stage's running total (read/write throughput breakdown in
/// the tool's own report). No-op when disabled.
#[inline]
pub fn add_bytes(stage: Stage, n: u64) {
    if !enabled() {
        return;
    }
    if let Some(r) = REGISTRY.get() {
        r.add_bytes(stage, n);
    }
}

/// Bump a non-timing counter by `delta`. No-op when disabled.
#[inline]
pub fn count(c: Counter, delta: u64) {
    if !enabled() {
        return;
    }
    if let Some(r) = REGISTRY.get() {
        r.count(c, delta);
    }
}

/// Where the periodic / exit writers emit to (recorded at [`enable`] time).
/// The dump cadence itself is owned by the interval thread's captured sleep, so
/// only the sink path is stored here.
struct Emit {
    out_file: Option<PathBuf>,
}
static EMIT: OnceLock<Emit> = OnceLock::new();

/// Turn telemetry on. Called once at mount when `--stats` / `GROVEFS_STATS=1`.
/// If an `interval` is set, spawn a daemon that rewrites `out_file` (or the log)
/// every period; the final report is emitted by the mount's exit guard
/// ([`emit_stderr`]). With no `out_file`, the periodic writer is skipped (the
/// exit + hidden node still work).
pub fn enable(out_file: Option<PathBuf>, interval: Option<Duration>) {
    REGISTRY.get_or_init(Registry::new);
    let _ = EMIT.set(Emit {
        out_file: out_file.clone(),
    });
    ENABLED.store(true, Ordering::Relaxed);
    if let Some(dur) = interval {
        std::thread::spawn(move || loop {
            std::thread::sleep(dur);
            emit_to_file();
        });
    }
}

/// Read a global snapshot as text (used by the `.grove-stats` control node).
pub fn snapshot_text() -> String {
    match REGISTRY.get() {
        Some(r) => r.report_text(),
        None => String::from("telemetry not enabled (mount with --stats)\n"),
    }
}

fn write_report_to(path: &PathBuf) {
    if let Some(r) = REGISTRY.get() {
        use std::io::Write;
        // Atomic-ish: write to a temp then rename, so a concurrent `cat` never
        // sees a half-written file.
        let tmp = path.with_extension("grovestats.tmp");
        if let Ok(mut f) = std::fs::File::create(&tmp) {
            let _ = f.write_all(r.report_text().as_bytes());
            let _ = f.write_all(b"\n--- csv ---\n");
            let _ = f.write_all(r.report_csv().as_bytes());
            let _ = f.sync_all();
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

/// Write the current report to the configured `--stats-file` (if any).
pub fn emit_to_file() {
    if let Some(e) = EMIT.get() {
        if let Some(p) = &e.out_file {
            write_report_to(p);
        }
    }
}

/// Print the final report to stderr at unmount (the exit-dump contract).
pub fn emit_stderr() {
    if enabled() {
        eprint!("\n{}", snapshot_text());
        emit_to_file();
    }
}

/// RAII exit guard: drops (prints + flushes the report) when the mount's serve
/// scope ends. Held as `Some(ExitDump)` for the lifetime of `session.run()`, so
/// the dump fires on a clean unmount *and* when `main` bails via `?`.
pub struct ExitDump;

impl Drop for ExitDump {
    fn drop(&mut self) {
        emit_stderr();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_all_is_complete_and_labels_unique() {
        let mut seen = std::collections::HashSet::new();
        for &s in Stage::ALL {
            assert!(seen.insert(s.name()), "duplicate stage label {}", s.name());
        }
        // The discriminant must equal the ALL position, since the registry is
        // indexed by `Stage as usize`.
        for (i, &s) in Stage::ALL.iter().enumerate() {
            assert_eq!(
                s as usize, i,
                "Stage discriminant drifted from ALL order at {i}"
            );
        }
    }

    #[test]
    fn counter_all_labels_unique() {
        let mut seen = std::collections::HashSet::new();
        for &c in Counter::ALL {
            assert!(
                seen.insert(c.name()),
                "duplicate counter label {}",
                c.name()
            );
        }
        for (i, &c) in Counter::ALL.iter().enumerate() {
            assert_eq!(
                c as usize, i,
                "Counter discriminant drifted from ALL order at {i}"
            );
        }
    }

    #[test]
    fn registry_merges_shards_and_reports_quantiles() {
        let r = Registry::new();
        // Spread 100 samples of 100us across shards to exercise the merge path.
        for i in 0..100u64 {
            r.record(
                Stage::ReadBackendTotal,
                100 + (i % 10),
                (i as usize) % SHARDS,
            );
        }
        r.add_bytes(Stage::ReadBackendTotal, 4096);
        let merged = r.merged(Stage::ReadBackendTotal);
        assert_eq!(merged.len(), 100, "all shards must fold into the merge");
        assert!(merged.max() >= 100);
        let m = r.report_text();
        assert!(
            m.contains("read.backend_total"),
            "report must list the stage"
        );
        assert!(m.contains("ops=100"), "report must show the count");

        // Unrecorded stage is present but zero, keeping the dump shape stable.
        let z = r.merged(Stage::PgSsiRetry);
        assert_eq!(z.len(), 0);
        assert!(r.report_text().contains("db.pg_ssi_retry"));

        // CSV has a header + one data row per stage.
        let csv = r.report_csv();
        assert!(csv.starts_with("stage,group,ops"));
        assert_eq!(
            csv.lines().count(),
            1 + Stage::ALL.len() + Counter::ALL.len()
        );
    }

    #[test]
    fn counters_accumulate() {
        let r = Registry::new();
        r.count(Counter::BytesRead, 1000);
        r.count(Counter::BytesRead, 500);
        r.count(Counter::PgSsiRetry, 3);
        assert_eq!(
            r.counters[Counter::BytesRead as usize].load(Ordering::Relaxed),
            1500
        );
        assert_eq!(
            r.counters[Counter::PgSsiRetry as usize].load(Ordering::Relaxed),
            3
        );
    }

    #[test]
    fn saturated_record_does_not_panic_on_huge_value() {
        let r = Registry::new();
        // A pathologically large duration must saturate into the top bucket
        // rather than error or panic; HDR rounds its ceiling up to a power-of-two
        // sub-bucket, so we only assert it landed as exactly one sample.
        r.record(Stage::IoShardRead, u64::MAX, 0);
        let h = r.merged(Stage::IoShardRead);
        assert_eq!(
            h.len(),
            1,
            "saturating_record still adds exactly one sample"
        );
        assert!(h.max() > 0);
    }

    #[test]
    fn disabled_by_default_start_is_none() {
        // The global gate is off unless a mount turns it on; unit tests never do,
        // so the hot-path helper must hand back nothing and touch no registry.
        assert!(!enabled(), "telemetry must default off");
        assert!(start(Stage::ReadBackendTotal).is_none());
        add_bytes(Stage::ReadBackendTotal, 10); // must be a silent no-op
        count(Counter::BytesRead, 1); // must be a silent no-op
    }
}

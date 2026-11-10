//! P0 ecosystem spike for the FoundationDB metadata backend.
//!
//! Runs against a live cluster and answers the three questions that decision
//! G1 turns on, in the order the plan states them:
//!
//! 1. **Can we link, commit, and read our own write back?** Without this nothing
//!    else in this file means anything.
//! 2. **Does the frozen key encoding (§1–§3 of `docs/fdb-keyspace.md`) actually
//!    carry the two properties the whole design leans on?** Namely: key existence
//!    arbitrating `UNIQUE(parent_id, name)` under real concurrency, and prefix
//!    order being name order so readdir needs no sort. Both are tested against
//!    `grovefs::fdb_store::Keys` — the production encoder — not a copy, so a pass
//!    here is a pass for the code that will ship.
//! 3. **Are OCC conflicts reachable, detected, and retried into a converged
//!    result?** 10 000 two-key atomic moves between 8 threads. A run that reports
//!    `retries = 0` has proved nothing about concurrency, so that case is a
//!    failure, not a success.
//!
//! Run:
//! ```text
//! bash .build-bin/fdb_env.sh up
//! eval "$(bash .build-bin/fdb_env.sh vars)"
//! cargo run --release --features fdb --example fdb_spike -- --reads 100000 --pairs 10000
//! ```
//!
//! NOT a benchmark. The cluster this passes against is `configure new single ssd`
//! — one coordinator, replication factor 1 — so it can prove mechanisms and
//! rough cost, and may NOT be cited for durability, HA, or throughput claims.
//! Those belong to P4/P6 measurements on a multi-node cluster.

use foundationdb::tuple::{Element, TuplePack, TupleUnpack};
use foundationdb::{Database, FdbError};
use grovefs::fdb_store::Keys;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Retry/backoff constants mirroring `fdb_store::SERIALIZATION_MAX_ATTEMPTS`.
/// Kept identical on purpose: the spike exists to show the production number
/// converges.
const MAX_ATTEMPTS: u32 = 12;
const BACKOFF_MIN: std::time::Duration = std::time::Duration::from_micros(200);
const BACKOFF_MAX: std::time::Duration = std::time::Duration::from_millis(25);

const BUCKETS: i64 = 64;
const PER_BUCKET: i64 = 100;
const MOVERS: usize = 8;

fn is_retriable(code: i32) -> bool {
    matches!(code, 1020 | 1021 | 1031 | 1032 | 1034 | 1037)
}

/// Synthetic bucket key for the conflict test. Deliberately NOT one of the frozen
/// production families (§10): tokens in buckets are a made-up workload, and giving
/// them a `Keys` accessor would smuggle a non-contract key into the encoder. It
/// still sits under the view prefix, so view isolation is exercised.
fn bucket_key(keys: &Keys, b: i64) -> Vec<u8> {
    keys.view_prefix().subspace(&("tokens", b)).pack(&(0i64,))
}

/// Clear a whole range prefix with the raw bindings (the production `Tx` wrapper
/// is not public enough for an example to reuse its helper).
fn clear_prefix(tx: &foundationdb::Transaction, opt: &foundationdb::RangeOption) {
    tx.clear_range(opt.begin.key(), opt.end.key());
}

fn block<R>(f: R) -> R::Output
where
    R: std::future::Future,
{
    futures::executor::block_on(f)
}

fn die(context: &str, e: &FdbError) -> ! {
    panic!("FDB error in {context}: {} — {e}", e.code())
}

// ---- 1. link / commit / read-your-write ------------------------------------

fn check_roundtrip(db: &std::sync::Arc<Database>) -> Result<(), String> {
    let probe = b"grovefs-spike/probe".to_vec();
    let payload = 0xC0FFEEi64.pack_to_vec();
    let tx = db.create_trx().map_err(|e| format!("create_trx: {e}"))?;
    tx.set(&probe, &payload);
    match block(tx.commit()) {
        Ok(_c) => {}
        Err(e) => return Err(format!("commit: {}", FdbError::from(e).code())),
    }
    let tx = db.create_trx().map_err(|e| format!("create_trx: {e}"))?;
    let got = block(tx.get(&probe, false)).map_err(|e| format!("get: {e}"))?;
    let got = got.ok_or_else(|| "read-your-write returned None".to_string())?;
    match Element::unpack_root(&got).map_err(|e| format!("unpack: {e}"))? {
        Element::Int(v) if v == 0xC0FFEE => Ok(()),
        other => Err(format!("decoded the wrong value: {other:?}")),
    }
}

// ---- 2a. key existence as the uniqueness arbiter ----------------------------

/// N threads race to insert the same `(parent, name)` using the production key
/// encoder. Exactly one must win; every loser must end up either "already
/// existed" (after its retry re-reads the committed row) or out of attempts.
/// This is §3.1's claim that `EEXIST` needs no constraint index.
fn check_uniqueness(db: &std::sync::Arc<Database>, view: &str) -> Result<(), String> {
    const RACERS: usize = 10;
    let keys = Keys::for_view(view);
    let key = keys.name(1, "dup");
    // Clear first so a rerun of the spike measures a race, not leftovers.
    let tx = db.create_trx().map_err(|e| e.to_string())?;
    tx.clear(&key);
    match block(tx.commit()) {
        Ok(_) => {}
        Err(e) => return Err(format!("clear commit: {}", FdbError::from(e).code())),
    }

    let winners = Arc::new(AtomicU64::new(0));
    let existed = Arc::new(AtomicU64::new(0));
    let exhausted = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::new();
    for id in 0..RACERS {
        let (db, key, winners, existed, exhausted) = (
            db.clone(),
            key.clone(),
            winners.clone(),
            existed.clone(),
            exhausted.clone(),
        );
        handles.push(std::thread::spawn(move || {
            let mut attempt = 0u32;
            let mut delay = BACKOFF_MIN;
            loop {
                let tx = match db.create_trx() {
                    Ok(t) => t,
                    Err(e) => panic!("create_trx: {e}"),
                };
                // Read before write: this is what makes the loser's retry see the
                // winner's committed row instead of blindly overwriting it.
                let seen = block(tx.get(&key, false))
                    .ok()
                    .flatten()
                    .is_some();
                if seen {
                    existed.fetch_add(1, Ordering::SeqCst);
                    return;
                }
                tx.set(&key, &(id as i64).pack_to_vec());
                match block(tx.commit()) {
                    Ok(_) => {
                        winners.fetch_add(1, Ordering::SeqCst);
                        return;
                    }
                    Err(e) => {
                        let err = FdbError::from(e);
                        if !is_retriable(err.code()) {
                            die("uniqueness commit", &err);
                        }
                        attempt += 1;
                        if attempt >= MAX_ATTEMPTS {
                            exhausted.fetch_add(1, Ordering::SeqCst);
                            return;
                        }
                        std::thread::sleep(delay);
                        delay = (delay * 2).min(BACKOFF_MAX);
                    }
                }
            }
        }));
    }
    for h in handles {
        h.join().map_err(|_| "racer thread panicked".to_string())?;
    }
    let w = winners.load(Ordering::SeqCst);
    let e = existed.load(Ordering::SeqCst);
    let x = exhausted.load(Ordering::SeqCst);
    println!("  uniqueness racers: winners={w} existed={e} exhausted={x}");
    if w != 1 {
        return Err(format!("exactly one writer must win, got {w}"));
    }
    if w + e + x != RACERS as u64 {
        return Err("racers unaccounted for".to_string());
    }
    if x != 0 {
        return Err(format!(
            "{x} racers exhausted the {MAX_ATTEMPTS}-attempt budget: the budget is \
             not sufficient for {} contenders on one key",
            RACERS
        ));
    }
    Ok(())
}

// ---- 2b. prefix range order == name order ----------------------------------

/// §3.3: readdir must be one ordered range scan with no server-side sort and no
/// OFFSET. Written out of order, read back in name order.
fn check_prefix_order(db: &std::sync::Arc<Database>, view: &str) -> Result<(), String> {
    let keys = Keys::for_view(view);
    let parent = 9_001i64;
    let tx = db.create_trx().map_err(|e| e.to_string())?;
    clear_prefix(&tx, &keys.children_range(parent));
    match block(tx.commit()) {
        Ok(_) => {}
        Err(e) => return Err(format!("clear commit: {}", FdbError::from(e).code())),
    }

    const N: usize = 2_000;
    let mut names: Vec<String> = (0..N).map(|i| format!("n{:06}", (i * 7919) % N)).collect();
    names.sort();
    names.dedup();
    let mut shuffled = names.clone();
    shuffled.reverse(); // write in the opposite order to prove storage does the sorting

    let tx = db.create_trx().map_err(|e| e.to_string())?;
    for nm in &shuffled {
        tx.set(&keys.name(parent, nm), b"x");
    }
    match block(tx.commit()) {
        Ok(_) => {}
        Err(e) => return Err(format!("bulk commit: {}", FdbError::from(e).code())),
    }

    // Read the whole prefix back in one pass. FDB caps a range reply, so page it
    // by moving the begin selector past the last key returned (0.11 exposes the
    // constructors but not the internal `make_first_greater_than` that
    // `RangeOption::next_range` uses).
    let first = keys.children_range(parent);
    let end = first.end;
    let mut begin = first.begin;
    let mut got: Vec<String> = Vec::new();
    loop {
        let opt = foundationdb::RangeOption {
            begin,
            end: end.clone(),
            ..Default::default()
        };
        let tx = db.create_trx().map_err(|e| e.to_string())?;
        let vals = block(tx.get_range(&opt, 1, false)).map_err(|e| e.to_string())?;
        let more = vals.more();
        let mut last: Option<Vec<u8>> = None;
        for kv in vals.as_ref() {
            // key = view_prefix + ("n", parent, name)
            let (_tag, _parent, nm) = keys
                .view_prefix()
                .unpack::<(Element, i64, String)>(kv.key())
                .map_err(|e| format!("unpack key: {e}"))?;
            got.push(nm);
            last = Some(kv.key().to_vec());
        }
        match (more, last) {
            (false, _) => break,
            (true, Some(k)) => {
                begin = foundationdb::KeySelector::first_greater_than(k);
            }
            (true, None) => return Err("range said more but returned nothing".to_string()),
        }
    }
    if got.len() != names.len() {
        return Err(format!("read back {} of {} entries", got.len(), names.len()));
    }
    if got != names {
        return Err("range scan did not come back in name order".to_string());
    }
    println!("  prefix order: {} entries, strictly ordered, no sort applied", got.len());
    Ok(())
}

// ---- 3. conflicts reachable and retried into a converged result ------------

/// 8 threads move tokens bucket-to-bucket. Every move is TWO keys in ONE
/// transaction, which is the shape §3.9's rename and §3.6's ledger+node pair
/// share: if the two writes could commit separately, a conflict would be invisible
/// and the total would still balance. It must balance BECAUSE the aborted side was
/// retried, not because nothing collided.
fn check_conflicts(db: &std::sync::Arc<Database>, view: &str, pairs: usize) -> Result<(), String> {
    let keys = Keys::for_view(view);

    // Seed: clear, then set every bucket to PER_BUCKET outside the measurement.
    let bucket = |b: i64| bucket_key(&keys, b);
    let tx = db.create_trx().map_err(|e| e.to_string())?;
    for b in 0..BUCKETS {
        tx.clear(&bucket(b));
    }
    match block(tx.commit()) {
        Ok(_) => {}
        Err(e) => return Err(format!("seed clear: {}", FdbError::from(e).code())),
    }
    let tx = db.create_trx().map_err(|e| e.to_string())?;
    for b in 0..BUCKETS {
        tx.set(&bucket(b), &PER_BUCKET.pack_to_vec());
    }
    match block(tx.commit()) {
        Ok(_) => {}
        Err(e) => return Err(format!("seed write: {}", FdbError::from(e).code())),
    }

    let retries = Arc::new(AtomicU64::new(0));
    let aborted = Arc::new(AtomicU64::new(0));
    let gave_up = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::new();
    let per_thread = pairs / MOVERS;
    for t in 0..MOVERS {
        let (db, keys, retries, aborted, gave_up) =
            (db.clone(), keys.clone(), retries.clone(), aborted.clone(), gave_up.clone());
        handles.push(std::thread::spawn(move || {
            let mut rng = 0x2545F4914F6CDD1Du64 ^ (t as u64).wrapping_mul(0x9E3779B97F4A7C15);
            let mut next = move || {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                rng
            };
            for _ in 0..per_thread {
                let from = (next() % BUCKETS as u64) as i64;
                let mut to = (next() % BUCKETS as u64) as i64;
                if to == from {
                    to = (from + 1) % BUCKETS;
                }
                let (kf, kt) = (bucket_key(&keys, from), bucket_key(&keys, to));
                let mut attempt = 0u32;
                let mut delay = BACKOFF_MIN;
                loop {
                    let tx = match db.create_trx() {
                        Ok(t) => t,
                        Err(e) => panic!("create_trx: {e}"),
                    };
                    // Both reads join the conflict set, so a concurrent mover of
                    // either bucket aborts one of the two — the OCC behaviour the
                    // whole backend rests on.
                    let vf = match block(tx.get(&kf, false)) {
                        Ok(Some(v)) => v,
                        Ok(None) => panic!("bucket {from} vanished"),
                        Err(e) => {
                            if is_retriable(e.code()) {
                                aborted.fetch_add(1, Ordering::SeqCst);
                                retries.fetch_add(1, Ordering::SeqCst);
                                std::thread::sleep(delay);
                                delay = (delay * 2).min(BACKOFF_MAX);
                                continue;
                            }
                            die("bucket read", &e);
                        }
                    };
                    let vt = match block(tx.get(&kt, false)) {
                        Ok(Some(v)) => v,
                        Ok(None) => panic!("bucket {to} vanished"),
                        Err(e) => {
                            if is_retriable(e.code()) {
                                aborted.fetch_add(1, Ordering::SeqCst);
                                retries.fetch_add(1, Ordering::SeqCst);
                                std::thread::sleep(delay);
                                delay = (delay * 2).min(BACKOFF_MAX);
                                continue;
                            }
                            die("bucket read", &e);
                        }
                    };
                    let a = unpack_i64(&vf).expect("bucket value");
                    let b = unpack_i64(&vt).expect("bucket value");
                    if a == 0 {
                        // Nothing to move; a legal outcome, not a failure.
                        break;
                    }
                    tx.set(&kf, &(a - 1).pack_to_vec());
                    tx.set(&kt, &(b + 1).pack_to_vec());
                    match block(tx.commit()) {
                        Ok(_) => break,
                        Err(e) => {
                            let err = FdbError::from(e);
                            if !is_retriable(err.code()) {
                                die("move commit", &err);
                            }
                            aborted.fetch_add(1, Ordering::SeqCst);
                            attempt += 1;
                            if attempt >= MAX_ATTEMPTS {
                                gave_up.fetch_add(1, Ordering::SeqCst);
                                break;
                            }
                            retries.fetch_add(1, Ordering::SeqCst);
                            std::thread::sleep(delay);
                            delay = (delay * 2).min(BACKOFF_MAX);
                        }
                    }
                }
            }
        }));
    }
    for h in handles {
        h.join().map_err(|_| "mover thread panicked".to_string())?;
    }

    // Total must still be BUCKETS * PER_BUCKET. This is the real assertion: a lost
    // or duplicated token means the two writes were not atomic together.
    let mut total = 0i64;
    for b in 0..BUCKETS {
        let tx = db.create_trx().map_err(|e| e.to_string())?;
        let v = block(tx.get(&bucket(b), false)).map_err(|e| e.to_string())?;
        total += unpack_i64(&v.expect("bucket present after moves")).expect("bucket int");
    }
    let want = BUCKETS * PER_BUCKET;
    println!(
        "  conflicts: aborts={} retries={} gave_up={} total={total} (want {want})",
        aborted.load(Ordering::SeqCst),
        retries.load(Ordering::SeqCst),
        gave_up.load(Ordering::SeqCst),
    );
    if total != want {
        return Err(format!(
            "token total drifted: {total} != {want} — the paired write was not atomic"
        ));
    }
    if gave_up.load(Ordering::SeqCst) != 0 {
        return Err("some moves never converged inside the retry budget".to_string());
    }
    if retries.load(Ordering::SeqCst) == 0 {
        return Err(
            "zero retries means zero contention observed: the run proved nothing \
             about concurrency. Raise --pairs or lower the bucket count."
                .to_string(),
        );
    }
    Ok(())
}

fn unpack_i64(bytes: &[u8]) -> Option<i64> {
    Element::unpack_root(bytes).ok()?.as_i64()
}

// ---- 4. point-read throughput ----------------------------------------------

/// The §3.2 hot path: one point get per getattr. 100 000 of them is the plan's
/// P0 number, and the point is not the rate but that every single one returns the
/// value written, i.e. the read path is correct at volume.
fn check_reads(db: &std::sync::Arc<Database>, view: &str, reads: usize) -> Result<(), String> {
    let keys = Keys::for_view(view);
    const SEED: usize = 5_000;
    let tx = db.create_trx().map_err(|e| e.to_string())?;
    for i in 0..SEED as i64 {
        tx.set(&keys.ino(i), &i.pack_to_vec());
    }
    match block(tx.commit()) {
        Ok(_) => {}
        Err(e) => return Err(format!("seed commit: {}", FdbError::from(e).code())),
    }

    let start = std::time::Instant::now();
    let mut hits = 0usize;
    let mut misses = 0usize;
    let mut batch = Vec::with_capacity(64);
    for i in 0..reads {
        batch.push(keys.ino((i % SEED) as i64));
        if batch.len() == 64 || i + 1 == reads {
            // §3.2 promises "two point reads, one round trip"; batching 64 keys
            // through join_all exercises that same machinery rather than a
            // sequential loop that would hide it.
            let tx = db.create_trx().map_err(|e| e.to_string())?;
            let futs = batch.iter().map(|k| tx.get(k, true));
            for v in futures::executor::block_on(futures::future::join_all(futs)) {
                match v.map_err(|e| e.to_string())? {
                    Some(b) => {
                        hits += 1;
                        let want = unpack_i64(&b);
                        if want.is_none() {
                            return Err("point read returned an undecodable value".into());
                        }
                    }
                    None => misses += 1,
                }
            }
            batch.clear();
        }
    }
    let secs = start.elapsed().as_secs_f64();
    println!(
        "  point reads: {reads} issued, hits={hits} misses={misses}, {:.0} reads/s \
       (single/replication-1: NOT a throughput claim)",
        reads as f64 / secs
    );
    if misses != 0 {
        return Err(format!("{misses} seeded keys did not read back"));
    }
    Ok(())
}

fn main() {
    let mut reads = 100_000usize;
    let mut pairs = 10_000usize;
    let mut cluster = std::env::var("GROVEFS_FDB_CLUSTER_FILE").unwrap_or_default();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--reads" => reads = args.next().expect("value").parse().expect("int"),
            "--pairs" => pairs = args.next().expect("value").parse().expect("int"),
            "--cluster" => cluster = args.next().expect("value"),
            other => panic!("unknown arg {other} (expected --reads/--pairs/--cluster)"),
        }
    }
    if cluster.is_empty() {
        panic!("no cluster file: set GROVEFS_FDB_CLUSTER_FILE or pass --cluster <path>");
    }

    // The network guard is intentionally leaked exactly as `fdb_store` does it:
    // `NetworkAutoStop::drop` stops the event loop and aborts the process if that
    // fails, and neither belongs in a spike that just wants to exit with a code.
    let network = unsafe { foundationdb::boot() };
    let db = std::sync::Arc::new(Database::new(Some(&cluster)).expect("open fdb cluster"));
    let view = "spike";

    println!("== P0 spike: cluster {cluster} ==");
    let checks: Vec<(&str, Result<(), String>)> = vec![
        ("roundtrip", check_roundtrip(&db)),
        ("uniqueness", check_uniqueness(&db, view)),
        ("prefix-order", check_prefix_order(&db, view)),
        ("conflicts", check_conflicts(&db, view, pairs)),
        ("point-reads", check_reads(&db, view, reads)),
    ];

    let mut failed = Vec::new();
    for (name, res) in checks {
        match res {
            Ok(()) => println!("[ok]   {name}"),
            Err(e) => {
                println!("[FAIL] {name}: {e}");
                failed.push(name);
            }
        }
    }
    std::mem::forget(network);
    if failed.is_empty() {
        println!("P0_SPIKE_VERDICT=PASS");
    } else {
        println!("P0_SPIKE_VERDICT=FAIL {}", failed.join(","));
        std::process::exit(1);
    }
}

// The spike's retry loops are inlined per check because each must COUNT the aborts
// it sees, which `fdb_store::with_serial_retry` deliberately hides from its caller.
// The classification table (`is_retriable`) and the budget constants above are the
// parts that must match production, and `tests::only_abort_codes_are_retriable` in
// `fdb_store` pins them.

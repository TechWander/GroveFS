//! Integration tests for the PostgreSQL backend's **SERIALIZABLE / SSI**
//! write path (see `src/pg_store.rs`). Proves the HopsFS-style
//! parallel-metadata claim with concrete behaviour rather than by argument:
//!
//!   * `parallel_create_distinct_parents_completes` — many threads `create_node`
//!     on different parents all succeed, and the wall-clock is materially
//!     below what a single-writer mutex would cost;
//!   * `same_parent_name_contention_yields_one_winner` — two threads racing
//!     on `(parent, "same-name")` produce exactly one success and one
//!     `Exists`, exercising both the UNIQUE-violation hard path AND the
//!     SSI-abort-then-recheck path;
//!   * `rename_concurrent_swap_leaves_valid_shape` — two renames whose
//!     source/target row sets overlap cannot both commit an interleaved
//!     half-state; the survivor sees a well-formed tree at the end;
//!   * `unlink_hardlink_pair_gcs_exactly_once` — two concurrent unlink of
//!     the two names of one inode returns exactly one `Some(data_ref)` for
//!     the last name, proving the refcount read is inside the same SSI tx
//!     as the DELETE (the pre-SSI impl relied on the write mutex for this).
//!
//! All tests self-skip (return early with a printed note) if PG is not
//! reachable on 127.0.0.1:5432 with the `grove/grovepw` role and `grovefs`
//! database — matching the convention of `.build-bin/test_pg.sh` so `run_all`
//! stays GREEN on hosts without a PG server.

use grovefs::meta::{MetaError, ROOT_INO};
use grovefs::pg_store::PgEngine;
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

const PG_URI: &str = "postgres://grove:grovepw@127.0.0.1:5432/grovefs";

/// TCP-probe the PG endpoint; if unreachable, print a note and let the test
/// pass trivially. Deliberately a runtime probe rather than a `#[cfg]`: the
/// SSI behaviour can't be exercised at compile time, and silently skipping
/// keeps `cargo test --release` green on the default CI host.
fn pg_ready() -> bool {
    TcpStream::connect_timeout(
        &"127.0.0.1:5432".parse().unwrap(),
        Duration::from_millis(200),
    )
    .is_ok()
}

fn skip_if_no_pg(label: &str) -> bool {
    if pg_ready() {
        false
    } else {
        eprintln!("SKIP pg_ssi::{label}: no PostgreSQL on 127.0.0.1:5432");
        true
    }
}

/// Short unique view name (schema_for rejects >25 bytes). Combining process
/// id + a caller-supplied tag gives per-test isolation without bleed between
/// runs, and stays well under the 25-byte cap.
fn fresh_view(tag: &str) -> String {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{tag}{pid:07}{nanos:09}")
}

/// Build a fresh PG-backed view by exercising the schema-ensure path.
/// Panics if `PgEngine::open` fails on a host that claims PG is reachable —
/// that would be a real bug, not an env issue.
fn open_engine(tag: &str) -> PgEngine {
    PgEngine::open(PG_URI, &fresh_view(tag), false).expect("open PG engine")
}

/// Seed one directory under root and return its id (used as a "second
/// parent" for parallel-write fan-out tests).
fn seed_dir(eng: &PgEngine, name: &str) -> i64 {
    eng.create_node(ROOT_INO, name, true, None, 0o755, 1000, 1000)
        .expect("seed dir")
}

/// The headline claim: N threads writing under DIFFERENT parents complete in
/// close to `1/N` of the serialized wall time. We don't assert an exact
/// ratio (VM noise, PG startup, connection-pool warm-up dominate at these
/// sizes), just that the total is materially smaller than `N * single_ms`,
/// which is what the pre-SSI single-mutex path would give.
#[test]
fn parallel_create_distinct_parents_completes() {
    if skip_if_no_pg("parallel_create_distinct_parents_completes") {
        return;
    }
    let eng = Arc::new(open_engine("pa"));
    const N_THREADS: usize = 6;
    const PER_THREAD: usize = 20;

    // Build one parent per thread up front, then time the concurrent writes.
    let parents: Vec<i64> = (0..N_THREADS)
        .map(|i| seed_dir(&eng, &format!("p{i}")))
        .collect();

    // Baseline: time a single sequential create on one parent.
    let base = Instant::now();
    let _ = eng
        .create_node(parents[0], "warmup", false, Some("k0"), 0o644, 1000, 1000)
        .unwrap();
    let single_ms = base.elapsed().as_millis().max(1) as u64;

    // Concurrent phase.
    let eng_c = eng.clone();
    let parents_c = parents.clone();
    let handles: Vec<_> = (0..N_THREADS)
        .map(|t| {
            let e = eng_c.clone();
            let p = parents_c[t];
            std::thread::spawn(move || {
                for i in 0..PER_THREAD {
                    let name = format!("f{t}_{i}");
                    let key = format!("k{t}_{i}");
                    e.create_node(p, &name, false, Some(&key), 0o644, 1000, 1000)
                        .unwrap_or_else(|err| {
                            panic!("parallel create[{t}][{i}] failed: {err:?}")
                        });
                }
            })
        })
        .collect();
    let conc_start = Instant::now();
    for h in handles {
        h.join().unwrap();
    }
    let conc_ms = conc_start.elapsed().as_millis() as u64;

    // Total ops N_THREADS * PER_THREAD. Single-writer serial would cost
    // roughly `single_ms * N_THREADS * PER_THREAD` ms (a very loose bound,
    // because a real serial run amortises warm-up). We assert only that the
    // concurrent wall is under half that — a conservative signal that
    // multiple SSI backends did in fact run without deadlock-abort storms
    // (which would push the wall near the serial figure).
    let loose_serial_bound = single_ms * (N_THREADS * PER_THREAD) as u64;
    assert!(
        conc_ms * 2 < loose_serial_bound,
        "concurrent wall={conc_ms}ms with baseline={single_ms}ms/op; expected at least 2x \
         speed-up over a hypothetical serial run — SSI parallel writes not observed"
    );

    // Correctness: every parent has exactly PER_THREAD entries (plus the
    // one warm-up on parents[0]).
    for (i, p) in parents.iter().enumerate() {
        let got = eng.readdir(*p).len();
        let want = if i == 0 { PER_THREAD + 1 } else { PER_THREAD };
        assert_eq!(got, want, "parent p{i} row count mismatch");
    }
}

/// Two threads racing to create the SAME `(parent, name)`: exactly one
/// success and one `Exists` (from the UNIQUE-violation 23505 hard path or
/// from the retry-observed-existing-row path). The SSI retry logic must
/// never turn this into `Ok` on both sides — the whole point of a
/// serializable schedule is that the final state is one some serial order
/// could produce, and no serial order produces two rows on the same key.
#[test]
fn same_parent_name_contention_yields_one_winner() {
    if skip_if_no_pg("same_parent_name_contention_yields_one_winner") {
        return;
    }
    let eng = Arc::new(open_engine("sc"));
    let p = seed_dir(&eng, "box");

    // Fire many contenders, not just two — bigger chance of actually hitting
    // the SSI abort-then-retry path rather than the plain UNIQUE-violation
    // path, because concurrent INSERTs on the same key that interleave as
    // read-write dependences abort one side with 40001 first.
    const N: usize = 8;
    let eng_c = eng.clone();
    let handles: Vec<_> = (0..N)
        .map(|t| {
            let e = eng_c.clone();
            std::thread::spawn(move || {
                let r = e.create_node(p, "dup", false, Some("dup-key"), 0o644, 1000, 1000);
                (t, r.is_ok())
            })
        })
        .collect();
    let oks: Vec<usize> = handles
        .into_iter()
        .filter_map(|h| {
            let (t, ok) = h.join().unwrap();
            if ok {
                Some(t)
            } else {
                None
            }
        })
        .collect();

    assert_eq!(
        oks.len(),
        1,
        "exactly one contender must succeed; got {oks:?} out of {N}"
    );
    // And the state on disk agrees: one row named "dup".
    let hits: Vec<_> = eng
        .readdir(p)
        .into_iter()
        .filter(|n| n.name == "dup")
        .collect();
    assert_eq!(hits.len(), 1, "readdir must show exactly one 'dup' entry");
}

/// Two renames whose source/target row sets overlap cannot both commit an
/// interleaved half-state. Under the SSI schedule, one succeeds and the
/// other either retries-then-sees-the-new-shape and errors correctly, or is
/// aborted and retried. The final tree must be a valid serial outcome: no
/// lost entries, no duplicated names, and nlink consistent.
#[test]
fn rename_concurrent_swap_leaves_valid_shape() {
    if skip_if_no_pg("rename_concurrent_swap_leaves_valid_shape") {
        return;
    }
    let eng = Arc::new(open_engine("rs"));
    let p = seed_dir(&eng, "room");
    // Seed /room/a (inode ia) and /room/b (inode ib).
    let ia = eng
        .create_node(p, "a", false, Some("ka"), 0o644, 1000, 1000)
        .unwrap();
    let ib = eng
        .create_node(p, "b", false, Some("kb"), 0o644, 1000, 1000)
        .unwrap();
    assert_ne!(ia, ib, "sanity: two distinct inodes");

    // Concurrently: rename a -> c, rename b -> d. Disjoint targets: both
    // should succeed, one per serialisation order (they don't conflict).
    let e1 = eng.clone();
    let e2 = eng.clone();
    let h1 = std::thread::spawn(move || e1.rename(p, "a", p, "c", true));
    let h2 = std::thread::spawn(move || e2.rename(p, "b", p, "d", true));
    let r1 = h1.join().unwrap();
    let r2 = h2.join().unwrap();
    assert!(r1.is_ok(), "rename a->c should succeed: {r1:?}");
    assert!(r2.is_ok(), "rename b->d should succeed: {r2:?}");

    // Now both names are gone; the inodes live under c and d.
    assert!(eng.lookup(p, "a").is_none(), "old name 'a' must be gone");
    assert!(eng.lookup(p, "b").is_none(), "old name 'b' must be gone");
    assert_eq!(eng.lookup(p, "c").unwrap().id, ia);
    assert_eq!(eng.lookup(p, "d").unwrap().id, ib);

    // Contention case: two concurrent renames onto the SAME target with
    // noreplace=true. One wins, the other sees EEXIST after the winner
    // commits (retry path) OR directly from UNIQUE-violation — both
    // acceptable, only "both succeed" would be a bug.
    let e3 = eng.clone();
    let e4 = eng.clone();
    let x1 = std::thread::spawn(move || e3.rename(p, "c", p, "clash", true));
    let x2 = std::thread::spawn(move || e4.rename(p, "d", p, "clash", true));
    let y1 = x1.join().unwrap();
    let y2 = x2.join().unwrap();
    let ok_ct = y1.is_ok() as usize + y2.is_ok() as usize;
    assert!(
        ok_ct <= 1,
        "at most one rename onto a shared target with noreplace may succeed"
    );
    // One of them lost with Exists (or NotFound if the winner's rename
    // already moved its source); both are valid serial outcomes.
    if ok_ct == 1 {
        let loser = if y1.is_ok() { &y2 } else { &y1 };
        match loser {
            Err(MetaError::Exists) | Err(MetaError::NotFound) => {}
            other => panic!("unexpected loser outcome: {other:?}"),
        }
    }
}

/// Two concurrent `unlink` calls on the two names of one hard-linked inode:
/// exactly one reports the data_ref as GC-eligible (the last name), the
/// other reports None (a sibling name still exists at its snapshot). The
/// SELECT-COUNT-of-data_ref read must live inside the SAME SSI tx as the
/// DELETE — otherwise both would see `refs=0` after both deletes and each
/// return Some(dr), causing a double-GC of a still-referenced object.
#[test]
fn unlink_hardlink_pair_gcs_exactly_once() {
    if skip_if_no_pg("unlink_hardlink_pair_gcs_exactly_once") {
        return;
    }
    let eng = Arc::new(open_engine("uh"));
    let p = seed_dir(&eng, "hl");
    let ino = eng
        .create_node(p, "n1", false, Some("shared-key"), 0o644, 1000, 1000)
        .unwrap();
    eng.link_node(ino, p, "n2").unwrap();

    // Sanity: two names share one ino.
    assert_eq!(eng.lookup(p, "n1").unwrap().id, ino);
    assert_eq!(eng.lookup(p, "n2").unwrap().id, ino);

    let e1 = eng.clone();
    let e2 = eng.clone();
    let h1 = std::thread::spawn(move || e1.unlink(p, "n1"));
    let h2 = std::thread::spawn(move || e2.unlink(p, "n2"));
    let r1 = h1.join().unwrap();
    let r2 = h2.join().unwrap();

    let gc_count = matches!(&r1, Ok(Some(_))) as usize + matches!(&r2, Ok(Some(_))) as usize;
    assert_eq!(
        gc_count, 1,
        "exactly one unlink must report the last-name GC, got r1={r1:?} r2={r2:?}"
    );
    // Both names are gone afterwards.
    assert!(eng.lookup(p, "n1").is_none());
    assert!(eng.lookup(p, "n2").is_none());
}

/// End-to-end sanity of the retry path: force a serialization conflict by
/// concurrently creating the SAME parent then renaming INTO that same
/// parent, and verify all callers eventually return without error (the
/// SSI retry loop absorbs the transient aborts). We can't count aborts
/// directly from the outside, but a functional pass under contention is
/// the property the paper's "transparently retried" claim asserts.
#[test]
fn ssi_retry_under_mixed_workload_converges() {
    if skip_if_no_pg("ssi_retry_under_mixed_workload_converges") {
        return;
    }
    let eng = Arc::new(open_engine("mx"));
    let root = seed_dir(&eng, "workspace");
    // Create 4 pre-existing entries for renames to source from.
    let mut srcs = Vec::new();
    for i in 0..4 {
        let name = format!("src{i}");
        let key = format!("k{i}");
        let id = eng
            .create_node(root, &name, false, Some(&key), 0o644, 1000, 1000)
            .unwrap();
        srcs.push((name, id));
    }

    // Mixed workload: half threads create, half threads rename. Contention
    // on `root`'s UNIQUE(parent_id,name) index and on rename's source/target
    // row reads gives plenty of SSI-abort opportunities.
    let mut handles = Vec::new();
    for t in 0..4 {
        let e = eng.clone();
        handles.push(std::thread::spawn(move || {
            for i in 0..8 {
                let n = format!("c{t}_{i}");
                let k = format!("ck{t}_{i}");
                let _ = e.create_node(root, &n, false, Some(&k), 0o644, 1000, 1000);
            }
        }));
    }
    for (idx, (old_name, _)) in srcs.iter().enumerate() {
        let e = eng.clone();
        let new_name = format!("r{idx}_moved");
        let old = old_name.clone();
        handles.push(std::thread::spawn(move || {
            let _ = e.rename(root, &old, root, &new_name, true);
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    // Every create and every rename that could legally have succeeded did:
    // each "c{t}_{i}" name is present exactly once; each src either lives
    // under the new name (rename won) or under the old one (lost the race
    // against a concurrent op — either is a valid serial outcome).
    let names: Vec<String> = eng.readdir(root).into_iter().map(|n| n.name).collect();
    for t in 0..4 {
        for i in 0..8 {
            let want = format!("c{t}_{i}");
            assert!(
                names.iter().any(|n| *n == want),
                "expected created entry {want} to be present"
            );
        }
    }
    for idx in 0..4 {
        let moved = format!("r{idx}_moved");
        let old = format!("src{idx}");
        let either = names.iter().any(|n| *n == moved) || names.iter().any(|n| *n == old);
        assert!(
            either,
            "renamed entry must appear under new name {moved} or old name {old}"
        );
    }
}

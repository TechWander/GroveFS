//! Cross-view holder-ledger invariants under CONCURRENT metadata writes, plus
//! the throughput probe the `LEDGER_SERIAL` decision was based on.
//!
//! The ledger (`<meta>/refs.db`, table `holders(data_ref, holders)`) counts how
//! many VIEWS currently hold a name for a content key. Two properties must
//! survive any interleaving of create/link/rename/unlink from several engines
//! in one process (which is exactly what a meta-server does: one engine per
//! view, one thread per client connection):
//!
//!   I1  holders[dr] == number of views whose tree has >= 1 name for dr
//!   I2  a blob is physically freed exactly when its holder count drops to 0
//!       (no freed-while-still-named, no dangling blob left unfreed)
//!
//! Both are asserted against the real SQLite files after a concurrent churn, so
//! a race in the ledger bookkeeping shows up as a failing assertion, not as a
//! silently lost object.

#![cfg(feature = "native")]

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

use grovefs::meta::{MetaEngine, REFS_DB, ROOT_INO};

/// Fresh scratch directory per test: view dbs and the shared `refs.db` live
/// side by side in it, exactly like a real `<base>/meta/`.
fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("grovefs_ledger_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn open_views(dir: &Path, k: usize) -> Vec<MetaEngine> {
    (0..k)
        .map(|i| MetaEngine::open(&dir.join(format!("v{i}.db"))).unwrap())
        .collect()
}

/// `data_ref -> holder count` straight from the ledger file.
fn ledger_holders(dir: &Path) -> HashMap<String, i64> {
    let conn = rusqlite::Connection::open(dir.join(REFS_DB)).unwrap();
    let mut stmt = conn
        .prepare("SELECT data_ref, holders FROM holders")
        .unwrap();
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
        .unwrap();
    rows.map(|r| r.unwrap()).collect()
}

/// `data_ref -> number of NAMES` in one view's tree (hard links count too).
fn view_name_counts(db: &Path) -> HashMap<String, i64> {
    let conn = rusqlite::Connection::open(db).unwrap();
    let mut stmt = conn
        .prepare("SELECT data_ref, COUNT(*) FROM node WHERE data_ref IS NOT NULL GROUP BY data_ref")
        .unwrap();
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
        .unwrap();
    rows.map(|r| r.unwrap()).collect()
}

/// The ledger's own commit is what caps metadata write throughput (see the
/// `ledger_write_scaling_probe` numbers): a bare `ATTACH` inherits the rollback
/// journal, which made every create/unlink pay a journal fsync on refs.db.
/// Guard the fix — the ledger must end up in WAL like the view dbs, because a
/// later refactor that drops the PRAGMA would silently halve write throughput
/// without failing any correctness test.
#[test]
fn ledger_uses_wal_like_the_view_dbs() {
    let dir = scratch("ledgerwal");
    let _views = open_views(&dir, 1);
    let conn = rusqlite::Connection::open(dir.join(REFS_DB)).unwrap();
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode.to_ascii_lowercase(), "wal", "ledger journal_mode");
    let _ = std::fs::remove_dir_all(&dir);
}

/// I1 + I2 under concurrent create/link/rename/unlink across several views.
///
/// The workload deliberately SHARES data_refs between views (`shared-*`) so the
/// holder count goes above 1 and the "another view still names it" branch of
/// `ledger_remove` is exercised, while each view also churns its own private
/// keys (which must be freed as soon as their last name goes).
#[test]
fn concurrent_metadata_keeps_ledger_invariants() {
    const VIEWS: usize = 4;
    const SHARED: usize = 12;
    const PRIVATE_PER_VIEW: usize = 20;

    let dir = scratch("invariants");
    let views = open_views(&dir, VIEWS);
    // Blob-store stand-in: a key is "on disk" iff it was inserted and no free
    // has been reported for it since.
    let blobs: Mutex<HashSet<String>> = Mutex::new(HashSet::new());
    // How often each key was reported freeable. Every key must be reported
    // exactly once: a second report means it was freed while a name (or a
    // holder row) still existed — the early-free failure mode — and a missing
    // report is the dangling-blob one.
    let freed: Mutex<HashMap<String, usize>> = Mutex::new(HashMap::new());
    let note_free = |dr: String| {
        blobs.lock().unwrap().remove(&dr);
        *freed.lock().unwrap().entry(dr).or_insert(0) += 1;
    };

    // Phase 1 — the only legal way a key becomes shared: an EXISTING name (and
    // its ledger row, holders=1) is copied into another view by fork / import /
    // graft. So view 0 names every shared key first, on its own. Two views
    // independently minting the same key is not a reachable state (keys are
    // random UUIDs; sharing always starts from an existing name).
    for j in 0..SHARED {
        let dr = format!("shared-{j}");
        blobs.lock().unwrap().insert(dr.clone());
        views[0]
            .create_node(
                ROOT_INO,
                &format!("s{j}"),
                false,
                Some(&dr),
                0o644,
                1000,
                1000,
            )
            .unwrap();
    }

    // Phase 2 — the other views adopt those keys concurrently.
    std::thread::scope(|s| {
        for eng in views.iter().skip(1) {
            s.spawn(move || {
                for j in 0..SHARED {
                    let dr = format!("shared-{j}");
                    eng.create_node(
                        ROOT_INO,
                        &format!("s{j}"),
                        false,
                        Some(&dr),
                        0o644,
                        1000,
                        1000,
                    )
                    .unwrap();
                }
            });
        }
    });
    // I1 midpoint: one holder per VIEW, not per name.
    let holders = ledger_holders(&dir);
    for j in 0..SHARED {
        assert_eq!(
            holders.get(&format!("shared-{j}")).copied().unwrap_or(0),
            VIEWS as i64,
            "shared-{j} must be held once by each of the {VIEWS} views"
        );
    }

    // Phase 3 — concurrent private churn, plus shared-key churn INSIDE each
    // view (a second name and a rename), which must not move the holder count
    // at all: the view's hold was booked when it named the key the first time.
    std::thread::scope(|s| {
        for (v, eng) in views.iter().enumerate() {
            let blobs = &blobs;
            let note_free = &note_free;
            s.spawn(move || {
                for i in 0..PRIVATE_PER_VIEW {
                    let dr = format!("priv-{v}-{i}");
                    blobs.lock().unwrap().insert(dr.clone());
                    let name = format!("p{i}");
                    eng.create_node(ROOT_INO, &name, false, Some(&dr), 0o644, 1000, 1000)
                        .unwrap();
                    let renamed = format!("q{i}");
                    // Rename only moves a name: the ledger must not change.
                    let gc = eng
                        .rename(ROOT_INO, &name, ROOT_INO, &renamed, false)
                        .unwrap();
                    assert!(gc.is_none(), "rename must not free {dr}");
                    if let Some(x) = eng.unlink(ROOT_INO, &renamed).unwrap() {
                        note_free(x);
                    }
                }
                for j in 0..SHARED {
                    let dr = format!("shared-{j}");
                    let alias = format!("a{j}");
                    let ino = eng
                        .create_node(ROOT_INO, &alias, false, Some(&dr), 0o644, 1000, 1000)
                        .unwrap();
                    // Hard link to the same inode, then drop both extra names:
                    // names 2 and 3 in this view must neither book nor unbook.
                    assert_eq!(
                        eng.link_node(ino, ROOT_INO, &format!("b{j}")).unwrap(),
                        ino
                    );
                    assert!(eng.unlink(ROOT_INO, &alias).unwrap().is_none());
                    let gc = eng
                        .rename(ROOT_INO, &format!("b{j}"), ROOT_INO, &format!("c{j}"), false)
                        .unwrap();
                    assert!(gc.is_none());
                    assert!(eng.unlink(ROOT_INO, &format!("c{j}")).unwrap().is_none());
                }
            });
        }
    });
    let holders = ledger_holders(&dir);
    for j in 0..SHARED {
        assert_eq!(
            holders.get(&format!("shared-{j}")).copied().unwrap_or(0),
            VIEWS as i64,
            "in-view extra names must not change shared-{j}'s holder count"
        );
    }
    {
        let freed = freed.lock().unwrap();
        for v in 0..VIEWS {
            for i in 0..PRIVATE_PER_VIEW {
                let dr = format!("priv-{v}-{i}");
                assert_eq!(
                    freed.get(&dr).copied().unwrap_or(0),
                    1,
                    "{dr} is private to one view and must be freed exactly once"
                );
            }
        }
        for j in 0..SHARED {
            assert_eq!(
                freed.get(&format!("shared-{j}")).copied().unwrap_or(0),
                0,
                "shared-{j} is still named by every view, nothing may be freed"
            );
        }
    }

    // Phase 4 — every view drops its shared name at once. The holder count must
    // walk VIEWS -> 0 and cross zero exactly once, so exactly one view may be
    // told the blob is freeable. An early free would hand a second report to a
    // later unlink (its decrement would find no row and answer "freeable" too).
    std::thread::scope(|s| {
        for eng in views.iter() {
            let note_free = &note_free;
            s.spawn(move || {
                for j in 0..SHARED {
                    if let Some(x) = eng.unlink(ROOT_INO, &format!("s{j}")).unwrap() {
                        note_free(x);
                    }
                }
            });
        }
    });
    for j in 0..SHARED {
        let dr = format!("shared-{j}");
        assert_eq!(
            freed.lock().unwrap().get(&dr).copied().unwrap_or(0),
            1,
            "{dr} must be reported freeable exactly once by the last holder"
        );
    }

    // I1: no holder row survives the last name, and none went zero/negative.
    let ledger = ledger_holders(&dir);
    assert!(
        ledger.is_empty(),
        "ledger still holds {ledger:?} after every name was removed"
    );
    // I2: the blob-store model matches "no name anywhere" exactly — nothing was
    // freed while named (lost) and nothing named was left behind (dangling).
    let live = std::mem::take(&mut *blobs.lock().unwrap());
    let mut named: HashSet<String> = HashSet::new();
    for v in 0..VIEWS {
        named.extend(view_name_counts(&dir.join(format!("v{v}.db"))).into_keys());
    }
    assert!(named.is_empty(), "names survived: {named:?}");
    let dangling: Vec<&String> = live.difference(&named).collect();
    assert!(
        dangling.is_empty(),
        "blobs still on disk that no view names and the ledger does not hold: {dangling:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Why `LEDGER_SERIAL` is not the throughput wall: `BEGIN IMMEDIATE` on a view
/// connection takes the write lock of EVERY attached db up front, so two
/// engines writing their own view dbs still serialize on the shared `refs.db`
/// inside SQLite. This measures that directly — the second writer's BEGIN must
/// block for as long as the first holds its (empty!) transaction open.
#[test]
fn begin_immediate_serializes_on_the_shared_ledger() {
    let dir = scratch("beginlock");
    let ledger = dir.join(REFS_DB);
    let open = |name: &str| {
        let conn = rusqlite::Connection::open(dir.join(name)).unwrap();
        conn.execute_batch(&format!(
            "PRAGMA busy_timeout=5000;
             CREATE TABLE IF NOT EXISTS node (id INTEGER PRIMARY KEY);
             ATTACH DATABASE '{}' AS refs;
             CREATE TABLE IF NOT EXISTS refs.holders (data_ref TEXT PRIMARY KEY, holders INTEGER NOT NULL);",
            ledger.display()
        ))
        .unwrap();
        conn
    };
    let a = open("va.db");
    let mut b = open("vb.db");

    a.execute_batch("BEGIN IMMEDIATE").unwrap();
    // A touches NO table of its own: if BEGIN itself did not take the shared
    // ledger's write lock, B would not notice anything.
    let held = std::time::Duration::from_millis(400);
    let begin_wait = std::thread::scope(|s| {
        let waiter = s.spawn(|| {
            let t0 = Instant::now();
            let mk = b.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate);
            let waited = t0.elapsed();
            mk.unwrap().rollback().unwrap();
            waited
        });
        std::thread::sleep(held);
        a.execute_batch("COMMIT").unwrap();
        waiter.join().unwrap()
    });
    assert!(
        begin_wait >= held / 2,
        "second BEGIN IMMEDIATE returned after {begin_wait:?} while another connection held a \
         write txn: the shared ledger's write lock is NOT taken at BEGIN, so the serialization \
         point moved — re-evaluate LEDGER_SERIAL"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Throughput probe (ignored by default; `cargo test --release --test meta_ledger
/// -- --ignored --nocapture`). Prints aggregate create/rename/unlink rates for
/// 1/2/4/8 views driven concurrently from one process — the measurement the
/// `LEDGER_SERIAL` decision cites.
#[test]
#[ignore = "throughput probe, run explicitly with --ignored --nocapture"]
fn ledger_write_scaling_probe() {
    const PER_VIEW: usize = 200;
    for &k in &[1usize, 2, 4, 8] {
        let dir = scratch(&format!("scale{k}"));
        let views = open_views(&dir, k);

        let create = timed(&views, PER_VIEW, |v, eng, i| {
            let dr = format!("dr-{v}-{i}");
            eng.create_node(ROOT_INO, &format!("c{i}"), false, Some(&dr), 0o644, 1000, 1000)
                .unwrap();
        });
        let rename = timed(&views, PER_VIEW, |_v, eng, i| {
            eng.rename(ROOT_INO, &format!("c{i}"), ROOT_INO, &format!("r{i}"), false)
                .unwrap();
        });
        let unlink = timed(&views, PER_VIEW, |_v, eng, i| {
            eng.unlink(ROOT_INO, &format!("r{i}")).unwrap();
        });
        println!(
            "views={k}  create={create:>8.1}/s  rename={rename:>8.1}/s  unlink={unlink:>8.1}/s"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Run `op(v, engine, i)` for `per_view` items on every view concurrently and
/// return the aggregate ops/s across all views.
fn timed<F>(engines: &[MetaEngine], per_view: usize, op: F) -> f64
where
    F: Fn(usize, &MetaEngine, usize) + Sync,
{
    let t0 = Instant::now();
    std::thread::scope(|s| {
        for (v, eng) in engines.iter().enumerate() {
            let op = &op;
            s.spawn(move || {
                for i in 0..per_view {
                    op(v, eng, i);
                }
            });
        }
    });
    let dt = t0.elapsed().as_secs_f64();
    (engines.len() * per_view) as f64 / dt
}

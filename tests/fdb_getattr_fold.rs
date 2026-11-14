//! Live-FDB verification for the getattr nlink-count / pointer-read overlap.
//!
//! Exercises the changed `FdbStore::getattr` (see `src/fdb_store.rs`): for a
//! NON-directory it now rides `count_range(holder_prefix(ino))` on the same wave
//! as the `ino`→pointer read (the count key is the caller's own `ino`, so it
//! needs nothing from the row), cutting a file/symlink stat from three serial
//! round-trips to two. This test proves the overlapped count still equals the
//! serial one the old `info` path produced — a real cluster can catch a wrong
//! key, a paging bug, or a kind-branch mistake that pure tests can't.
//!
//! Self-skips when no FDB coordinator answers on the standard single-node port.
//! FDB futures have no deadline (module §5), so we TCP-probe BEFORE `open`: a
//! blind `open` against a dead coordinator would park this thread forever.
//! Point the test at another cluster with `GROVEFS_FDB_CLUSTER_FILE=/path`.

#![cfg(feature = "fdb")]

use grovefs::fdb_store::FdbStore;
use grovefs::meta::{NodeKind, ROOT_INO};
use std::net::TcpStream;
use std::time::Duration;

fn cluster() -> String {
    std::env::var("GROVEFS_FDB_CLUSTER_FILE")
        .unwrap_or_else(|_| "/data/grovefs/.fdbtest/test.cluster".to_string())
}

fn ready() -> bool {
    std::path::Path::new(&cluster()).exists()
        && TcpStream::connect_timeout(
            &"127.0.0.1:4500".parse().unwrap(),
            Duration::from_millis(400),
        )
        .is_ok()
}

/// Unique subspace per run so re-runs never collide; `clear_view` at the end
/// tears it down, keeping the scratch cluster uncluttered.
fn fresh_view() -> String {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("foldtest{pid}x{nanos}")
}

/// The two read paths — overlapped `getattr` and serial `lookup` — must agree on
/// every attribute of one node.
fn eq_getattr_lookup(store: &FdbStore, ino: i64, parent: i64, name: &str) {
    let g = store
        .getattr(ino)
        .unwrap_or_else(|| panic!("getattr({ino}) None"));
    let l = store
        .lookup(parent, name)
        .unwrap_or_else(|| panic!("lookup({parent},{name:?}) None"));
    assert_eq!(g.id, l.id);
    assert_eq!(g.parent_id, l.parent_id);
    assert_eq!(g.name, l.name);
    assert_eq!(g.kind, l.kind);
    assert_eq!(g.data_ref, l.data_ref);
    assert_eq!(g.link_target, l.link_target);
    assert_eq!(g.mode, l.mode);
    assert_eq!(g.uid, l.uid);
    assert_eq!(g.gid, l.gid);
    assert_eq!(g.nlink, l.nlink, "overlapped getattr != serial lookup nlink");
    assert_eq!(g.size, l.size);
}

#[test]
fn fdb_getattr_count_overlap_matches_serial() {
    if !ready() {
        eprintln!("SKIP fdb_getattr_fold: no FDB coordinator on 127.0.0.1:4500 (cluster {})", cluster());
        return;
    }
    let uri = format!("fdb://{}", cluster());
    let store = match FdbStore::open(&uri, &fresh_view(), false) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("SKIP fdb_getattr_fold: open failed: {e}");
            return;
        }
    };

    // 1) Fresh dir (overlapped read is NOT used for dirs — checks the fallback).
    let d = store
        .create_node_with_attr(ROOT_INO, "d", true, None, 0o755, 1000, 1000)
        .unwrap();
    assert_eq!(d.kind, NodeKind::Dir);
    assert_eq!((d.nlink, d.size), (2, 4096), "fresh dir");
    let did = d.id;

    // 2) Dir gains a child dir -> its (serial) nlink must read 3.
    let sub = store
        .create_node_with_attr(did, "sub", true, None, 0o755, 1000, 1000)
        .unwrap();
    assert_eq!(store.getattr(did).unwrap().nlink, 3, "d + 1 child dir");
    let subdir_id = sub.id;

    // 3) A child FILE under d must not count toward d.nlink (is_dir bound).
    store
        .create_node_with_attr(did, "cf", false, Some("blob-cf"), 0o644, 1000, 1000)
        .unwrap();
    assert_eq!(store.getattr(did).unwrap().nlink, 3, "child file counted as dir");

    // 4) Fresh regular file (the overlapped path): nlink 1, size 0.
    let f = store
        .create_node_with_attr(ROOT_INO, "f", false, Some("blob1"), 0o644, 1000, 1000)
        .unwrap();
    assert_eq!(f.kind, NodeKind::Regular);
    assert_eq!((f.nlink, f.size), (1, 0), "fresh file");
    let fino = f.id;

    // 5) Hard link -> the overlapped holder count sees both names.
    store.link_node(fino, ROOT_INO, "f_link").expect("link f");
    let gf = store.getattr(fino).unwrap();
    assert_eq!(gf.nlink, 2, "overlapped holder count after one hard link");
    assert_eq!(gf.size, 0, "hard link must not change size");
    eq_getattr_lookup(&store, fino, ROOT_INO, "f");
    let l2 = store.lookup(ROOT_INO, "f_link").expect("lookup f_link");
    assert_eq!(l2.id, fino, "hard link shares inode");
    assert_eq!(l2.nlink, 2, "second name sees same nlink");

    // 6) A third name -> 3.
    store.link_node(fino, ROOT_INO, "f_link2").unwrap();
    assert_eq!(store.getattr(fino).unwrap().nlink, 3, "holder count with 3 names");

    // 7) Fresh symlink (overlapped path): nlink 1, size = target len.
    let s = store
        .create_symlink_with_attr(ROOT_INO, "s", "/some/target", 1000, 1000)
        .unwrap();
    assert_eq!(s.kind, NodeKind::Symlink);
    assert_eq!(s.nlink, 1, "symlink nlink");
    assert_eq!(s.size, "/some/target".len() as u64, "symlink size");

    // 8) Overlapped getattr == serial lookup across the spread.
    eq_getattr_lookup(&store, did, ROOT_INO, "d");
    eq_getattr_lookup(&store, subdir_id, did, "sub");
    eq_getattr_lookup(&store, s.id, ROOT_INO, "s");

    // 9) unlink one name -> holder count drops, read via the overlap.
    store.unlink(ROOT_INO, "f_link").unwrap();
    assert_eq!(store.getattr(fino).unwrap().nlink, 2, "nlink after unlink one name");

    // Tear the scratch subspace down, then close the FDB network so the test
    // process exits cleanly (FDB keeps a network thread otherwise — module §5).
    let _ = store.clear_view();
    grovefs::fdb_store::shutdown_network();
    eprintln!("PASS fdb_getattr_count_overlap_matches_serial");
}

//! Live-PG verification for the getattr/lookup single-round-trip fold.
//!
//! Exercises the correlated-scalar-subquery SELECT in `PgEngine::getattr` /
//! `PgEngine::lookup` (see `src/pg_store.rs`) against a REAL PostgreSQL, which
//! the pure unit tests can't: they lock the `finalize_counts` arithmetic, but
//! only a live server proves the `subdir_count` / `holder_count` subqueries
//! return the right numbers for directories gaining child dirs / files and for
//! hard-linked inodes seen through two names.
//!
//! Self-skips when no PG is reachable, matching `tests/pg_ssi.rs`, so the
//! default host stays GREEN. Connect to a specific instance with `PGURI=...`;
//! defaults to the dev fixture on 127.0.0.1:5433.

use grovefs::meta::{MetaError, NodeKind, ROOT_INO};
use grovefs::pg_store::PgEngine;
use std::net::TcpStream;
use std::time::Duration;

fn uri() -> String {
    std::env::var("PGURI")
        .unwrap_or_else(|_| "postgres://grove:grovepw@127.0.0.1:5433/grovefs".to_string())
}

/// Extract "host:port" after the last '@' so the probe follows PGURI.
fn hostport(u: &str) -> String {
    let after = u.rsplit('@').next().unwrap_or(u);
    after.split('/').next().unwrap_or(after).to_string()
}

fn pg_ready() -> bool {
    let hp = hostport(&uri());
    match hp.parse() {
        Ok(sa) => TcpStream::connect_timeout(&sa, Duration::from_millis(300)).is_ok(),
        Err(_) => false,
    }
}

/// Unique schema-isolated view; its name is printed so the caller can
/// `DROP SCHEMA grovefs_v_<hex> CASCADE` afterwards (no client dep in-test).
fn fresh_view() -> String {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("fold{pid:07}{nanos:09}")
}

/// Assert the two folded read paths agree on every attribute for one node.
fn eq_getattr_lookup(eng: &PgEngine, ino: i64, parent: i64, name: &str) {
    let g = eng
        .getattr(ino)
        .unwrap_or_else(|| panic!("getattr({ino}) returned None"));
    let l = eng
        .lookup(parent, name)
        .unwrap_or_else(|| panic!("lookup({parent},{name:?}) returned None"));
    assert_eq!(g.id, l.id, "id");
    assert_eq!(g.parent_id, l.parent_id, "parent_id");
    assert_eq!(g.name, l.name, "name");
    assert_eq!(g.kind, l.kind, "kind");
    assert_eq!(g.data_ref, l.data_ref, "data_ref");
    assert_eq!(g.link_target, l.link_target, "link_target");
    assert_eq!(g.mode, l.mode, "mode");
    assert_eq!(g.uid, l.uid, "uid");
    assert_eq!(g.gid, l.gid, "gid");
    assert_eq!(g.nlink, l.nlink, "nlink must match between getattr/lookup");
    assert_eq!(g.size, l.size, "size");
}

#[test]
fn pg_getattr_lookup_fold_matches_counts() {
    if !pg_ready() {
        eprintln!("SKIP pg_getattr_fold: no PostgreSQL at {}", hostport(&uri()));
        return;
    }
    let view = fresh_view();
    eprintln!("PGVIEW {}", view);
    let eng = PgEngine::open(&uri(), &view, false).expect("open PG engine");

    // 1) Fresh dir via single-trip create: nlink=2, size=4096.
    let d = eng
        .create_node_with_attr(ROOT_INO, "d", true, None, 0o755, 1000, 1000)
        .map_err(|e| panic!("create dir: {e}"))
        .unwrap();
    assert_eq!(d.kind, NodeKind::Dir);
    assert_eq!((d.nlink, d.size), (2, 4096), "fresh dir");
    let did = d.id;

    // 2) One child DIR -> subdir_count must lift nlink to 3 (COUNT is_dir=1).
    let sub = eng
        .create_node_with_attr(did, "sub", true, None, 0o755, 1000, 1000)
        .unwrap();
    let g = eng.getattr(did).expect("getattr d after child dir");
    assert_eq!(g.nlink, 3, "d.nlink should be 2 + 1 child dir");
    let subdir_id = sub.id;

    // 3) A grandchild dir under `sub` must NOT touch d.nlink (parent_id corr).
    eng.create_node_with_attr(subdir_id, "ssub", true, None, 0o755, 1000, 1000)
        .unwrap();
    assert_eq!(eng.getattr(did).unwrap().nlink, 3, "grandchild dir leaks into d");
    assert_eq!(eng.getattr(subdir_id).unwrap().nlink, 3, "sub gains its child dir");

    // 4) A child FILE must NOT count toward a dir's nlink (is_dir=1 predicate).
    eng.create_node_with_attr(did, "cf", false, Some("blob-cf"), 0o644, 1000, 1000)
        .unwrap();
    assert_eq!(eng.getattr(did).unwrap().nlink, 3, "child file counted as dir");

    // 5) Fresh regular FILE: nlink=1, size=0.
    let f = eng
        .create_node_with_attr(ROOT_INO, "f", false, Some("blob1"), 0o644, 1000, 1000)
        .unwrap();
    assert_eq!(f.kind, NodeKind::Regular);
    assert_eq!((f.nlink, f.size), (1, 0), "fresh file");
    let fino = f.id;

    // 6) Hard link -> holder_count sees both names; nlink=2 via BOTH read paths.
    let _ = eng.link_node(fino, ROOT_INO, "f_link").expect("link f");
    let gf = eng.getattr(fino).expect("getattr f after link");
    assert_eq!(gf.nlink, 2, "holder count after one hard link");
    assert_eq!(gf.size, 0, "hard link must not change size");
    eq_getattr_lookup(&eng, fino, ROOT_INO, "f");
    // Both names resolve to the same inode with the same nlink.
    let l2 = eng.lookup(ROOT_INO, "f_link").expect("lookup f_link");
    assert_eq!(l2.id, fino, "hard link shares inode");
    assert_eq!(l2.nlink, 2, "second name sees same nlink");

    // 7) A third name -> nlink=3.
    eng.link_node(fino, ROOT_INO, "f_link2").unwrap();
    assert_eq!(eng.getattr(fino).unwrap().nlink, 3, "holder count with 3 names");

    // 8) Fresh SYMLINK via single-trip create: nlink=1, size=target len.
    let s = eng
        .create_symlink_with_attr(ROOT_INO, "s", "/some/target", 1000, 1000)
        .unwrap();
    assert_eq!(s.kind, NodeKind::Symlink);
    assert_eq!(s.nlink, 1, "symlink nlink");
    assert_eq!(s.size, "/some/target".len() as u64, "symlink size = target len");

    // 9) Cross-check fold agreement on the whole spread.
    eq_getattr_lookup(&eng, did, ROOT_INO, "d");
    eq_getattr_lookup(&eng, subdir_id, did, "sub");
    eq_getattr_lookup(&eng, s.id, ROOT_INO, "s");

    // 10) unlink one name -> holder count drops back, read via the fold.
    match eng.unlink(ROOT_INO, "f_link") {
        Ok(None) => {} // not the last name: no data_ref released (still 2 names)
        Ok(Some(_)) => panic!("released blob while other names remain"),
        Err(MetaError::NotFound) => panic!("unlink f_link: not found"),
        Err(e) => panic!("unlink: {e}"),
    }
    assert_eq!(eng.getattr(fino).unwrap().nlink, 2, "nlink after unlink one name");

    eprintln!("PASS pg_getattr_lookup_fold_matches_counts");
}

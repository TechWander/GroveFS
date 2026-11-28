//! Integration coverage for `grovefs import` against a LIVE treedb engine
//! (`--meta-store treedb://host:port,…`): the same `grovefs::import::ingest`
//! walk runs against `MetaStore::Treedb`, so metadata nodes go through the
//! engine's POSIX metadata ops into the view tree the deployment routes the
//! name to, while the BYTES still land in the local `--data-dir` under the
//! identical `blocks/<bucket>/<key>` layout a mounted write uses (req #2).
//!
//! Covers: fresh import, incremental append of new files/dirs, same-name skip
//! (idempotence, add-only), and permission/timestamp fidelity.
//!
//! Self-skips when no engine answers the store URI. Default endpoints are the
//! development cluster's two instances; override the whole URI with
//! `TREEDB_TEST_URI=treedb://host:port,…`. A reachable-but-unusable engine
//! (auth mismatch etc.) also skips with the reason, mirroring `import_pg`.
//!
//! Gated on `treedb` because every path below reaches the engine through
//! `grovefs::treedb_store` / `MetaStore::open_treedb`, which the default
//! profile does not compile at all (found 2026-11-28: a board without
//! `FEATURES_TREEDB=1` failed this target's build, so `cargo test --release`
//! alone could never pass in the shipped feature face).

#![cfg(feature = "treedb")]

use grovefs::backend::local::LocalBackend;
use grovefs::backend::StorageBackend;
use grovefs::import::ImportStats;
use grovefs::meta::{MetaProvider, MetaStore, ROOT_INO};
use std::collections::HashMap;
use std::fs::{self, FileTimes};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::time::{Duration, SystemTime};

const DEFAULT_URI: &str = "treedb://127.0.0.1:7777,127.0.0.1:7778";

fn test_uri() -> String {
    std::env::var("TREEDB_TEST_URI").unwrap_or_else(|_| DEFAULT_URI.to_string())
}

/// Short unique view name for the engine's tree.
fn fresh_view(tag: &str) -> String {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{tag}{pid:07}{nanos:06}")
}

/// Open a fresh BASE view through the SAME entry points `grovefs import`
/// uses: the shared placement helpers first (so the name lands on the engine
/// instance the meta-server would route it to), then the store itself.
fn open_treedb_store(view: &str) -> Option<(MetaStore, String)> {
    let uri = test_uri();
    if !grovefs::treedb_store::is_treedb_store(&uri) {
        eprintln!("SKIP import_treedb: TREEDB_TEST_URI is not a treedb:// URI");
        return None;
    }
    let addrs = match grovefs::treedb_store::parse_addrs(&uri) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("SKIP import_treedb: bad store uri: {e}");
            return None;
        }
    };
    let home = match grovefs::treedb_store::find_tree_home(&addrs, view) {
        Ok(h) => h,
        Err(e) => {
            // Every instance unreachable (or none answering) is an ENVIRONMENT
            // statement on dev boxes without the cluster — skip, don't fail.
            eprintln!("SKIP import_treedb: no reachable engine at {uri}: {e}");
            return None;
        }
    };
    let addr = home.unwrap_or_else(|| grovefs::treedb_store::plan_placement(&addrs, view).addr);
    match MetaStore::open_treedb(&addr, view, false) {
        Ok(m) => Some((m, addr)),
        Err(e) => {
            eprintln!("SKIP import_treedb: cannot open store on {addr}: {e}");
            None
        }
    }
}

fn seed_dirs(meta: &dyn MetaProvider, prefix: &str) -> HashMap<String, i64> {
    let mut dirs = HashMap::new();
    dirs.insert("/".to_string(), ROOT_INO);
    grovefs::import::ensure_dir_soft(meta, prefix, &mut dirs).unwrap();
    dirs
}

#[test]
fn treedb_import_full_pipeline() {
    let view = fresh_view("itd");
    let Some((meta, addr)) = open_treedb_store(&view) else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let src = dir.path().join("legacy");
    fs::create_dir_all(src.join("sub")).unwrap();

    let f = src.join("a.txt");
    fs::write(&f, b"alpha").unwrap();
    fs::set_permissions(&f, fs::Permissions::from_mode(0o640)).unwrap();
    let mt = SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
    let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_500_000_000);
    fs::File::open(&f)
        .unwrap()
        .set_times(FileTimes::new().set_modified(mt).set_accessed(at))
        .unwrap();
    fs::write(src.join("sub/b.bin"), b"bravo").unwrap();
    std::os::unix::fs::symlink(&f, src.join("l.txt")).unwrap();

    let be = LocalBackend::new(&home).unwrap();
    let mut dirs = seed_dirs(&meta, "/legacy");
    let mut st = ImportStats::default();
    grovefs::import::ingest(&meta, &be, &src, "/legacy", &mut dirs, &mut st, None).unwrap();
    assert_eq!(st.dirs, 1, "only the real dir mirrors: {st:?}");
    assert_eq!(st.linked + st.copied, 2, "two files registered: {st:?}");
    assert_eq!(st.skipped, 1, "the symlink is counted, not imported");
    assert_eq!(st.failed, 0);

    // The tree lives INSIDE the engine (POSIX metadata ops); the store
    // interface reads it back with the fidelity guarantees intact.
    let l = meta.lookup(ROOT_INO, "legacy").expect("treedb view tree root");
    let a = meta.lookup(l.id, "a.txt").expect("treedb node a.txt");
    assert_eq!(a.mode & 0o7777, 0o640, "perm bits survive into treedb");
    let fm = fs::metadata(&f).unwrap();
    assert_eq!(a.uid, fm.uid());
    assert_eq!(a.gid, fm.gid());
    assert_eq!(a.mtime, 1_600_000_000, "treedb keeps the file's own clock");
    assert_eq!(a.atime, 1_500_000_000);
    let ka = a.data_ref.clone().expect("treedb node carries a data_ref");
    assert_eq!(be.read(&ka, 0, 10).unwrap(), b"alpha");

    // BYTES keep the one blocks/<bucket>/<key> convention (req #2): same paths
    // and same hardlink zero-copy as every other backend.
    let obj = be.object_path(&ka);
    assert!(obj.starts_with(home.join("blocks")), "{obj:?} outside blocks/");
    assert_eq!(obj.parent().unwrap().file_name().unwrap().len(), 2);
    assert_eq!(fs::metadata(&obj).unwrap().ino(), fm.ino(), "zero-copy import");

    // IDEMPOTENCE: the same re-run registers nothing and changes nothing.
    let before: Vec<String> = meta.readdir(l.id).into_iter().map(|n| n.name).collect();
    let mut st2 = ImportStats::default();
    grovefs::import::ingest(&meta, &be, &src, "/legacy", &mut dirs, &mut st2, None).unwrap();
    assert_eq!(st2.linked + st2.copied, 0, "re-run must not churn: {st2:?}");
    let after: Vec<String> = meta.readdir(l.id).into_iter().map(|n| n.name).collect();
    assert_eq!(before, after);

    // INCREMENTAL APPEND above the watermark; DELETE never syncs back.
    std::thread::sleep(Duration::from_millis(1100));
    let wm = st.watermark.expect("a real run records a watermark");
    let now = grovefs::import::epoch_secs(SystemTime::now());
    fs::write(src.join("sub/new.txt"), b"delta").unwrap();
    fs::create_dir_all(src.join("brand")).unwrap();
    fs::write(src.join("brand/e.txt"), b"epsilon").unwrap();
    let mut st3 = ImportStats::default();
    grovefs::import::ingest(
        &meta,
        &be,
        &src,
        "/legacy",
        &mut dirs,
        &mut st3,
        Some(wm.max(now - 1)),
    )
    .unwrap();
    let sub = meta.lookup(l.id, "sub").unwrap();
    let n = meta.lookup(sub.id, "new.txt").expect("treedb sees the appended file");
    let kn = n.data_ref.clone().unwrap();
    assert_eq!(be.read(&kn, 0, 10).unwrap(), b"delta");
    let brand = meta.lookup(l.id, "brand").expect("treedb sees the appended dir");
    let e = meta.lookup(brand.id, "e.txt").unwrap();
    let ke = e.data_ref.clone().unwrap();
    assert_eq!(be.read(&ke, 0, 10).unwrap(), b"epsilon");

    fs::remove_file(src.join("a.txt")).unwrap();
    let mut st4 = ImportStats::default();
    grovefs::import::ingest(&meta, &be, &src, "/legacy", &mut dirs, &mut st4, None).unwrap();
    assert!(meta.lookup(l.id, "a.txt").is_some(), "treedb import is add-only too");

    // Teardown: drop the TEST view's tree from the engine it was placed on.
    if let Err(e) = grovefs::treedb_store::TreedbStore::drop_view(&addr, &view) {
        eprintln!("note: could not drop test view '{view}' on {addr}: {e}");
    }
}

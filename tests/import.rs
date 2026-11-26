//! Coverage for `grovefs import`: the DATA plane (registering an EXISTING file
//! as an object must be zero-copy — hard link — when source and store share a
//! filesystem, and the shared inode must stay live through every path of the
//! object interface) and the full INGEST pipeline of `grovefs::import::ingest`
//! against a real SQLite store (fresh import, incremental re-runs, watermark
//! prune, permission/timestamp fidelity).

use grovefs::backend::local::LocalBackend;
use grovefs::backend::{Imported, StorageBackend};
use grovefs::import::ImportStats;
use grovefs::meta::{MetaProvider, MetaStore, ROOT_INO};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::time::{Duration, SystemTime};

use std::fs::FileTimes;

#[test]
fn import_at_hardlinks_zero_copy_and_shares_the_inode() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("store");
    fs::create_dir_all(&root).unwrap();
    let src_path = dir.path().join("dataset.bin");
    fs::write(&src_path, b"legacy bytes").unwrap();
    let src_ino = fs::metadata(&src_path).unwrap().ino();

    let be = LocalBackend::new(&root).unwrap();
    let got = be.import_at("k1", &src_path).unwrap();
    assert_eq!(got, Imported::Hardlinked, "same-fs import must not copy");

    // One inode behind both paths: size reads match, and a write through the
    // object interface is visible in the source file (the passthrough half
    // of the import promise), and vice versa.
    assert_eq!(be.len("k1").unwrap(), 12);
    be.write("k1", 7, b"BYTES!").unwrap();
    assert_eq!(fs::read_to_string(&src_path).unwrap(), "legacy BYTES!");
    let mut f = fs::OpenOptions::new().append(true).open(&src_path).unwrap();
    f.write_all(b"!!").unwrap();
    drop(f);
    assert_eq!(be.read("k1", 13, 8).unwrap(), b"!!".to_vec());

    // The blocks path is a second link on the SAME inode (nlink == 2). It
    // lives under the fan-out bucket, resolved through the backend itself.
    let obj = fs::metadata(be.object_path("k1")).unwrap();
    assert_eq!(obj.nlink(), 2);
    assert_eq!(obj.ino(), src_ino, "both paths name one inode");
}

#[test]
fn gc_remove_drops_only_the_blocks_link_not_the_source() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("store");
    fs::create_dir_all(&root).unwrap();
    let src_path = dir.path().join("keepme.txt");
    fs::write(&src_path, b"precious").unwrap();

    let be = LocalBackend::new(&root).unwrap();
    be.import_at("k2", &src_path).unwrap();
    // Unlinking the object (what view-level GC does) must leave the user's
    // original dataset path fully intact — importing can never destroy data.
    be.remove("k2").unwrap();
    assert!(!be.object_path("k2").exists());
    assert_eq!(fs::read_to_string(&src_path).unwrap(), "precious");
}

#[test]
fn default_import_at_streams_a_copy_for_other_backends() {
    // A backend that does not override import_at still ends up with a correct,
    // independent object (exercised through LocalBackend's own copy fallback
    // path is hard to force on one filesystem, so assert the trait default
    // via a minimal newtype wrapper that inherits it).
    struct Plain(LocalBackend);
    impl StorageBackend for Plain {
        fn create(&self, key: &str) -> std::io::Result<()> {
            self.0.create(key)
        }
        fn read(&self, k: &str, o: u64, l: usize) -> std::io::Result<Vec<u8>> {
            self.0.read(k, o, l)
        }
        fn write(&self, k: &str, o: u64, d: &[u8]) -> std::io::Result<usize> {
            self.0.write(k, o, d)
        }
        fn len(&self, k: &str) -> std::io::Result<u64> {
            self.0.len(k)
        }
        fn fsync(&self, k: &str) -> std::io::Result<()> {
            self.0.fsync(k)
        }
        fn truncate(&self, k: &str, n: u64) -> std::io::Result<()> {
            self.0.truncate(k, n)
        }
        fn remove(&self, k: &str) -> std::io::Result<()> {
            self.0.remove(k)
        }
        fn statvfs(&self) -> std::io::Result<grovefs::backend::FsStat> {
            self.0.statvfs()
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("store");
    fs::create_dir_all(&root).unwrap();
    let src_path = dir.path().join("file.bin");
    fs::write(&src_path, b"0123456789").unwrap();

    let be = Plain(LocalBackend::new(&root).unwrap());
    // Plain does NOT override import_at... but it delegates nothing, so the
    // trait default runs: a true independent copy.
    let got = be.import_at("k3", &src_path).unwrap();
    assert_eq!(got, Imported::Copied);
    assert_eq!(be.len("k3").unwrap(), 10);
    // Independent: mutating the source no longer shows in the object.
    fs::write(&src_path, b"changed").unwrap();
    assert_eq!(be.read("k3", 0, 20).unwrap(), b"0123456789".to_vec());
}

// ---- Full ingest pipeline against a real SQLite store ----------------------

/// Open a fresh BASE view in an (otherwise empty) local home — exactly what
/// `grovefs import` does when the target name owns no tree yet.
fn sqlite_store(home: &Path, view: &str) -> MetaStore {
    MetaStore::open_sqlite(home, view, false).unwrap()
}

/// Seed `dirs` the way the CLI does: root plus the resolved prefix.
fn seed_dirs(meta: &dyn MetaProvider, prefix: &str) -> HashMap<String, i64> {
    let mut dirs = HashMap::new();
    dirs.insert("/".to_string(), ROOT_INO);
    grovefs::import::ensure_dir_soft(meta, prefix, &mut dirs).unwrap();
    dirs
}

#[test]
fn ingest_fresh_import_mirrors_tree_and_lands_objects_in_blocks_buckets() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let src = dir.path().join("legacy");
    fs::create_dir_all(src.join("sub/deep")).unwrap();
    fs::write(src.join("a.txt"), b"alpha").unwrap();
    fs::write(src.join("sub/b.bin"), b"0123456789").unwrap();
    fs::write(src.join("sub/deep/c.txt"), b"gamma").unwrap();
    // A symlink and a top-level empty dir pin the documented skip/mirror rules.
    std::os::unix::fs::symlink(src.join("a.txt"), src.join("link.txt")).unwrap();
    fs::create_dir_all(src.join("emptydir")).unwrap();

    let meta = sqlite_store(&home, "alice");
    let be = LocalBackend::new(&home).unwrap();
    let mut dirs = seed_dirs(&meta, "/legacy");
    let mut st = ImportStats::default();
    grovefs::import::ingest(&meta, &be, &src, "/legacy", &mut dirs, &mut st, None).unwrap();

    // Three real dirs mirrored (sub, deep, emptydir); symlink skipped.
    assert_eq!(st.dirs, 3, "dirs: {st:?}");
    assert_eq!(st.linked + st.copied, 3, "objects: {st:?}");
    assert_eq!(st.skipped, 1, "the symlink is counted, never followed");
    assert_eq!(st.failed, 0);

    // Tree shape through the metadata interface.
    let l = meta.lookup(ROOT_INO, "legacy").unwrap();
    let a = meta.lookup(l.id, "a.txt").unwrap();
    let ka = a.data_ref.clone().unwrap();
    assert_eq!(be.read(&ka, 0, 100).unwrap(), b"alpha");
    let sb = meta.lookup(l.id, "sub").unwrap();
    assert!(sb.is_dir());
    let deep = meta.lookup(sb.id, "deep").unwrap();
    assert!(!meta.lookup(deep.id, "c.txt").unwrap().is_dir());
    assert!(meta.lookup(l.id, "emptydir").unwrap().is_dir());
    assert!(meta.lookup(l.id, "link.txt").is_none(), "symlinks are not imported");

    // ONE storage convention: the data_ref keys resolve through the same
    // blocks/<bucket>/<key> fan-out a mounted write uses (req #2).
    let key = a.data_ref.clone().unwrap();
    let obj = be.object_path(&key);
    assert!(obj.starts_with(home.join("blocks")), "{obj:?} outside blocks/");
    let bucket = obj.parent().unwrap().file_name().unwrap().to_string_lossy().into_owned();
    assert_eq!(bucket.len(), 2, "objects live in a fan-out bucket");
    // Same-fs hardlink: the blocks path shares the SOURCE file's inode.
    let src_ino = fs::metadata(src.join("a.txt")).unwrap().ino();
    assert_eq!(fs::metadata(&obj).unwrap().ino(), src_ino, "zero-copy import");
    assert_eq!(fs::metadata(&obj).unwrap().nlink(), 2);
    assert_eq!(st.linked, 3, "all three landed zero-copy on one filesystem");
}

#[test]
fn ingest_is_incremental_add_only_and_idempotent_with_watermark_prune() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let src = dir.path().join("legacy");
    fs::create_dir_all(src.join("sub")).unwrap();
    fs::write(src.join("a.txt"), b"alpha").unwrap();
    fs::write(src.join("sub/b.bin"), b"bravo").unwrap();

    let meta = sqlite_store(&home, "alice");
    let be = LocalBackend::new(&home).unwrap();
    let mut dirs = seed_dirs(&meta, "/legacy");
    let mut st = ImportStats::default();
    grovefs::import::ingest(&meta, &be, &src, "/legacy", &mut dirs, &mut st, None).unwrap();
    assert_eq!(st.linked + st.copied, 2);
    let wm = st.watermark.expect("a real run always records a watermark");

    // Post-dated dirs make "now" a strict prune bound for the legacy tree.
    std::thread::sleep(Duration::from_millis(1100));
    let now = grovefs::import::epoch_secs(SystemTime::now());
    let past = SystemTime::UNIX_EPOCH + Duration::from_secs(1_550_000_000);
    fs::File::open(&src)
        .unwrap()
        .set_times(FileTimes::new().set_modified(past))
        .unwrap();
    fs::File::open(src.join("sub"))
        .unwrap()
        .set_times(FileTimes::new().set_modified(past))
        .unwrap();

    // Additions: one NEW file under sub/, one brand-new top-level dir. sub
    // keeps its old mtime (a pure content-side view of the pathological case:
    // the watermark prune cannot see an addition that did not bump a walked
    // dir — that is exactly what --since trades for a cheap walk).
    fs::write(src.join("sub/new.txt"), b"delta").unwrap();
    fs::File::open(src.join("sub"))
        .unwrap()
        .set_times(FileTimes::new().set_modified(past))
        .unwrap();
    fs::create_dir_all(src.join("brand")).unwrap();
    fs::write(src.join("brand/e.txt"), b"epsilon").unwrap();

    let mut st2 = ImportStats::default();
    grovefs::import::ingest(&meta, &be, &src, "/legacy", &mut dirs, &mut st2, Some(now)).unwrap();
    // `legacy` and `brand` gained children after `now` → walked; the untouched
    // `sub` (mtime <= wm <= now) is PRUNED whole — its new file is therefore
    // NOT seen. That is exactly the prune contract: only additions that bump a
    // walked dir's own mtime are picked up cheaply; --full rescans them.
    let l = meta.lookup(ROOT_INO, "legacy").unwrap();
    let brand = meta.lookup(l.id, "brand").unwrap();
    let ke = meta.lookup(brand.id, "e.txt").unwrap().data_ref.clone().unwrap();
    assert_eq!(be.read(&ke, 0, 10).unwrap(), b"epsilon", "new dir+file registered");
    let sub = meta.lookup(l.id, "sub").unwrap();
    assert!(meta.lookup(sub.id, "new.txt").is_none(), "pruned subtree is not rescanned");
    assert!(st2.skipped >= 1, "the pruned dir counts as skipped: {st2:?}");

    // The SAME re-run at watermark `wm` (both dirs' real mtimes > wm) picks the
    // new file up — the name-skip fallback alone already guarantees no churn.
    fs::File::open(src.join("sub"))
        .unwrap()
        .set_times(FileTimes::new().set_modified(SystemTime::now()))
        .unwrap();
    let mut st3 = ImportStats::default();
    grovefs::import::ingest(&meta, &be, &src, "/legacy", &mut dirs, &mut st3, Some(wm)).unwrap();
    let sub = meta.lookup(l.id, "sub").unwrap();
    let kn = meta.lookup(sub.id, "new.txt").unwrap();
    assert!(!kn.is_dir(), "additions above the watermark land");
    let kn = kn.data_ref.clone().unwrap();
    assert_eq!(be.read(&kn, 0, 10).unwrap(), b"delta");

    // The watermark round-trips through the persisted per-stream ledger, so a
    // later CLI run prunes without an explicit --since.
    let abs = fs::canonicalize(&src).unwrap();
    let wkey = grovefs::import::watermark_key("alice", &abs, "/legacy");
    grovefs::import::save_watermark(&home, &wkey, wm).unwrap();
    assert_eq!(grovefs::import::load_watermark(&home, &wkey), Some(wm));
    // Distinct streams never share a watermark.
    let other = grovefs::import::watermark_key("alice", &abs, "/elsewhere");
    assert_ne!(wkey, other);

    // IDEMPOTENCE: a full rescan (since=None) over the converged tree skips
    // every name, registers nothing, and deletes nothing.
    let before: Vec<String> = meta.readdir(l.id).into_iter().map(|n| n.name).collect();
    let mut st4 = ImportStats::default();
    grovefs::import::ingest(&meta, &be, &src, "/legacy", &mut dirs, &mut st4, None).unwrap();
    assert_eq!(st4.linked + st4.copied, 0, "re-run registers nothing: {st4:?}");
    assert_eq!(st4.failed, 0);
    let after: Vec<String> = meta.readdir(l.id).into_iter().map(|n| n.name).collect();
    assert_eq!(before, after, "the view tree is unchanged by an idempotent re-run");

    // NO DELETE SYNC: removing source entries leaves the view intact. The file
    // is back-dated so the name-skip (not a prune) is what protects it here.
    fs::File::open(src.join("a.txt"))
        .unwrap()
        .set_times(FileTimes::new().set_modified(past))
        .unwrap();
    fs::remove_file(src.join("a.txt")).unwrap();
    let mut st5 = ImportStats::default();
    grovefs::import::ingest(&meta, &be, &src, "/legacy", &mut dirs, &mut st5, None).unwrap();
    assert!(meta.lookup(l.id, "a.txt").is_some(), "imports never delete from the view");
    let a = meta.lookup(l.id, "a.txt").unwrap();
    let ka = a.data_ref.clone().unwrap();
    assert_eq!(be.read(&ka, 0, 10).unwrap(), b"alpha");
}

#[test]
fn ingest_keeps_permissions_owner_and_timestamps() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let src = dir.path().join("legacy");
    fs::create_dir_all(&src).unwrap();
    let f = src.join("script.sh");
    fs::write(&f, b"#!/bin/sh\n").unwrap();
    fs::set_permissions(&f, fs::Permissions::from_mode(0o750)).unwrap();
    let mt = SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
    let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_500_000_000);
    fs::File::open(&f)
        .unwrap()
        .set_times(FileTimes::new().set_modified(mt).set_accessed(at))
        .unwrap();

    let meta = sqlite_store(&home, "alice");
    let be = LocalBackend::new(&home).unwrap();
    let mut dirs = seed_dirs(&meta, "/legacy");
    let mut st = ImportStats::default();
    grovefs::import::ingest(&meta, &be, &src, "/legacy", &mut dirs, &mut st, None).unwrap();

    let l = meta.lookup(ROOT_INO, "legacy").unwrap();
    let n = meta.lookup(l.id, "script.sh").unwrap();
    assert_eq!(n.mode & 0o7777, 0o750, "perm bits survive the ingest");
    // The source's owner is a hardlink property, so it is preserved by
    // construction; assert it only where it is guaranteed (running as root
    // could chown, non-root always links its own files).
    let fm = fs::metadata(&f).unwrap();
    assert_eq!(n.uid, fm.uid());
    assert_eq!(n.gid, fm.gid());
    // The file keeps its OWN clock, not "import happened now".
    assert_eq!(n.mtime, 1_600_000_000);
    assert_eq!(n.atime, 1_500_000_000);
}

#[test]
fn count_walk_tallies_without_touching_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("legacy");
    fs::create_dir_all(src.join("sub")).unwrap();
    fs::write(src.join("a"), b"a").unwrap();
    fs::write(src.join("sub/b"), b"b").unwrap();
    std::os::unix::fs::symlink(src.join("a"), src.join("l")).unwrap();
    let mut st = ImportStats::default();
    grovefs::import::count_walk(&src, &mut st).unwrap();
    assert_eq!(st.dirs, 1);
    assert_eq!(st.files, 2);
    assert_eq!(st.skipped, 1);
    // A dry run opens NO store: no home, no watermark.
    assert!(st.watermark.is_none());
    assert!(!dir.path().join("home").exists());
}

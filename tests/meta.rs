//! Unit tests for the metadata / view-tree engine. No mounting required.

use grovefs::meta::{MetaError, MetaEngine, NodeKind, ROOT_INO};

fn tmpdb(tag: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("grovefs_meta_{tag}_{}.db", std::process::id()));
    let _ = std::fs::remove_file(&p);
    let _ = std::fs::remove_file(format!("{}-wal", p.display()));
    let _ = std::fs::remove_file(format!("{}-shm", p.display()));
    p
}

#[test]
fn root_starts_empty_and_is_a_dir() {
    let m = MetaEngine::open(&tmpdb("root")).unwrap();
    assert!(m.readdir(ROOT_INO).is_empty());
    let root = m.getattr(ROOT_INO).unwrap();
    assert_eq!(root.kind, NodeKind::Dir);
    assert_eq!(root.nlink, 2); // "." + its own entry, no subdirs yet
}

#[test]
fn create_lookup_nesting() {
    let m = MetaEngine::open(&tmpdb("nest")).unwrap();
    let docs = m.create_node(ROOT_INO, "docs", true, None, 0o755, 1000, 1000).unwrap();
    let a = m.create_node(docs, "a.txt", false, Some("key-a"), 0o644, 1000, 1000).unwrap();

    let d = m.lookup(ROOT_INO, "docs").unwrap();
    assert_eq!(d.id, docs);
    assert!(m.lookup(ROOT_INO, "missing").is_none());

    let f = m.getattr(a).unwrap();
    assert_eq!(f.kind, NodeKind::Regular);
    assert_eq!(f.data_ref.as_deref(), Some("key-a"));
    // nlink for a singly-linked file is 1
    assert_eq!(f.nlink, 1);

    assert!(m.readdir(ROOT_INO).iter().any(|n| n.name == "docs"));
    assert!(m.readdir(docs).iter().any(|n| n.name == "a.txt"));
}

#[test]
fn duplicate_create_is_exists() {
    let m = MetaEngine::open(&tmpdb("dup")).unwrap();
    m.create_node(ROOT_INO, "x", true, None, 0o755, 0, 0).unwrap();
    let again = m.create_node(ROOT_INO, "x", true, None, 0o755, 0, 0);
    assert!(matches!(again, Err(MetaError::Exists)));
}

#[test]
fn hardlink_shares_object_and_gc_waits_for_last_ref() {
    let m = MetaEngine::open(&tmpdb("hard")).unwrap();
    let f = m.create_node(ROOT_INO, "orig", false, Some("key-h"), 0o644, 0, 0).unwrap();
    let link = m.link_node(f, ROOT_INO, "alias").unwrap();

    // Both names now report nlink 2 and point at the same object.
    assert_eq!(m.getattr(f).unwrap().nlink, 2);
    assert_eq!(m.getattr(link).unwrap().data_ref.as_deref(), Some("key-h"));

    // Removing one name must NOT free the object (still one reference left).
    assert_eq!(m.unlink(ROOT_INO, "orig").unwrap(), None);
    // Removing the last reference frees it.
    assert_eq!(m.unlink(ROOT_INO, "alias").unwrap().as_deref(), Some("key-h"));
}

#[test]
fn link_rejects_dirs() {
    let m = MetaEngine::open(&tmpdb("linkdir")).unwrap();
    let d = m.create_node(ROOT_INO, "dir", true, None, 0o755, 0, 0).unwrap();
    assert!(matches!(m.link_node(d, ROOT_INO, "d2"), Err(MetaError::OpNotPermitted)));
}

#[test]
fn symlink_roundtrip() {
    let m = MetaEngine::open(&tmpdb("sym")).unwrap();
    let s = m.create_symlink(ROOT_INO, "link", "/some/target", 0, 0).unwrap();
    let n = m.getattr(s).unwrap();
    assert_eq!(n.kind, NodeKind::Symlink);
    assert_eq!(n.link_target.as_deref(), Some("/some/target"));
    // st_size of a symlink is the length of its target.
    assert_eq!(n.size, "/some/target".len() as u64);
}

#[test]
fn rename_moves_and_rejects_cycle() {
    let m = MetaEngine::open(&tmpdb("ren")).unwrap();
    let a = m.create_node(ROOT_INO, "a", true, None, 0o755, 0, 0).unwrap();
    let b = m.create_node(a, "b", true, None, 0o755, 0, 0).unwrap();

    // Normal move: a/b -> a/c
    m.rename(a, "b", a, "c", false).unwrap();
    assert!(m.lookup(a, "c").is_some());
    assert!(m.lookup(a, "b").is_none());

    // Cycle: cannot move a into its own descendant b.
    assert!(matches!(
        m.rename(ROOT_INO, "a", b, "a", false),
        Err(MetaError::BadParentCycle)
    ));
}

#[test]
fn rename_between_hardlinks_is_noop() {
    // POSIX (man 2 rename): oldpath and newpath as two hard link names of the
    // SAME file -> rename() does nothing and succeeds. A row-id-only no-op
    // guard used to miss this and delete the target name, silently shrinking
    // nlink — the guard must compare inodes, not rows.
    let m = MetaEngine::open(&tmpdb("renhl")).unwrap();
    let f = m.create_node(ROOT_INO, "a", false, Some("key-r"), 0o644, 0, 0).unwrap();
    m.link_node(f, ROOT_INO, "b").unwrap();
    assert_eq!(m.getattr(f).unwrap().nlink, 2);

    // Both directions between the two links are no-ops that free no object.
    assert_eq!(m.rename(ROOT_INO, "a", ROOT_INO, "b", false).unwrap(), None);
    assert_eq!(m.rename(ROOT_INO, "b", ROOT_INO, "a", false).unwrap(), None);
    assert_eq!(m.getattr(f).unwrap().nlink, 2, "nlink must stay 2");
    let names: Vec<String> = m.readdir(ROOT_INO).into_iter().map(|n| n.name).collect();
    assert!(
        names.contains(&"a".to_string()) && names.contains(&"b".to_string()),
        "both links survive, got {names:?}"
    );

    // A rename over a DIFFERENT file still replaces normally: the old "a"
    // link of the hardlinked pair loses one name ("b" survives), and its
    // object is NOT GC'd because that last remaining name still refs it.
    let other = m.create_node(ROOT_INO, "c", false, Some("key-c"), 0o644, 0, 0).unwrap();
    assert_eq!(m.rename(ROOT_INO, "c", ROOT_INO, "a", false).unwrap(), None);
    assert!(m.lookup(ROOT_INO, "c").is_none());
    assert_eq!(m.getattr(other).unwrap().nlink, 1);
    assert_eq!(m.getattr(f).unwrap().nlink, 1);
}

#[test]
fn unlink_nonempty_dir_rejected() {
    let m = MetaEngine::open(&tmpdb("notempty")).unwrap();
    let d = m.create_node(ROOT_INO, "d", true, None, 0o755, 0, 0).unwrap();
    m.create_node(d, "kid", false, Some("k"), 0o644, 0, 0).unwrap();
    assert!(matches!(m.unlink(ROOT_INO, "d"), Err(MetaError::NotEmpty)));
}

#[test]
fn set_times_updates() {
    let m = MetaEngine::open(&tmpdb("times")).unwrap();
    let f = m.create_node(ROOT_INO, "f", false, Some("k"), 0o644, 0, 0).unwrap();
    m.set_times(f, Some(111), Some(222));
    let n = m.getattr(f).unwrap();
    assert_eq!(n.atime, 111);
    assert_eq!(n.mtime, 222);
}

#[test]
fn backup_to_bytes_is_a_complete_readable_snapshot() {
    // HA replication ships `backup_to_bytes` to the standby and installs it as a
    // standalone file; the snapshot MUST be openable on its own (all pages,
    // schema included) or failover serves a corrupt view.
    use std::io::Write;
    let m = MetaEngine::open(&tmpdb("backup")).unwrap();
    let docs = m.create_node(ROOT_INO, "docs", true, None, 0o755, 7, 7).unwrap();
    m.create_node(docs, "a.txt", false, Some("key-a"), 0o644, 7, 7).unwrap();

    let snap = m.backup_to_bytes().unwrap();
    assert!(!snap.is_empty());

    let mut p = std::env::temp_dir();
    p.push(format!("grovefs_backup_copy_{}.db", std::process::id()));
    let _ = std::fs::remove_file(&p);
    std::fs::File::create(&p).unwrap().write_all(&snap).unwrap();

    // Open the shipped copy read-only exactly as a standby would.
    let c = MetaEngine::open_ex(&p, true).unwrap();
    let d = c.lookup(ROOT_INO, "docs").expect("docs missing from snapshot");
    let a = c.lookup(d.id, "a.txt").expect("a.txt missing from snapshot");
    assert_eq!(a.data_ref.as_deref(), Some("key-a"));
    let _ = std::fs::remove_file(&p);
}

#[test]
fn epoch_defaults_zero_and_persists_across_reopen() {
    // The fencing epoch lives in the DB ctrl table and must survive a restart so
    // a promoted node never moves its epoch backwards.
    let p = tmpdb("epoch");
    {
        let m = MetaEngine::open(&p).unwrap();
        assert_eq!(m.get_epoch(), 0);
        m.set_epoch(5).unwrap();
        assert_eq!(m.get_epoch(), 5);
    }
    let m2 = MetaEngine::open(&p).unwrap();
    assert_eq!(m2.get_epoch(), 5);
}

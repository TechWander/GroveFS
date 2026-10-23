//! Integration tests for the per-mount presentation overlay ([`LayoutMeta`]).
//! No FUSE mount required: the lens is driven directly against a real
//! [`MetaEngine`] source tree, so we exercise lookup/readdir filtering, path
//! relocation, aliasing, write-through and the scaffolding-creation guard at
//! the exact metadata interface the FUSE layer uses.

use grovefs::layout::{LayoutMeta, LayoutRules};
use grovefs::meta::{MetaEngine, MetaProvider, NodeKind, ROOT_INO};

fn tmpdb(tag: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("grovefs_layout_{tag}_{}.db", std::process::id()));
    let _ = std::fs::remove_file(&p);
    let _ = std::fs::remove_file(format!("{}-wal", p.display()));
    let _ = std::fs::remove_file(format!("{}-shm", p.display()));
    p
}

/// Convenience: build a lens over a shared engine instance.
fn lens_over(eng: std::sync::Arc<MetaEngine>, rules: &str) -> LayoutMeta {
    LayoutMeta::new(eng, LayoutRules::parse(rules).unwrap())
}

fn names(v: &[grovefs::meta::NodeInfo]) -> Vec<String> {
    let mut n: Vec<_> = v.iter().map(|x| x.name.clone()).collect();
    n.sort();
    n
}

/// Seed the standard tree used by most cases into a fresh engine:
///   /docs/2026/a.md   /docs/2025/old.md
///   /photos/private/x.jpg   /photos/public/y.jpg
///   /data/raw/big.bin
///   /shared/lib
fn seed_tree(tag: &str) -> std::sync::Arc<MetaEngine> {
    let path = tmpdb(tag);
    let eng = std::sync::Arc::new(MetaEngine::open(&path).unwrap());
    let docs = eng.create_node(ROOT_INO, "docs", true, None, 0o0755, 1000, 1000).unwrap();
    let d26 = eng.create_node(docs, "2026", true, None, 0o0755, 1000, 1000).unwrap();
    eng.create_node(d26, "a.md", false, Some("ka"), 0o0644, 1000, 1000).unwrap();
    let d25 = eng.create_node(docs, "2025", true, None, 0o0755, 1000, 1000).unwrap();
    eng.create_node(d25, "old.md", false, Some("ko"), 0o0644, 1000, 1000).unwrap();

    let photos = eng.create_node(ROOT_INO, "photos", true, None, 0o0755, 1000, 1000).unwrap();
    let priv_ = eng.create_node(photos, "private", true, None, 0o0755, 1000, 1000).unwrap();
    eng.create_node(priv_, "x.jpg", false, Some("kx"), 0o0644, 1000, 1000).unwrap();
    let pub_ = eng.create_node(photos, "public", true, None, 0o0755, 1000, 1000).unwrap();
    eng.create_node(pub_, "y.jpg", false, Some("ky"), 0o0644, 1000, 1000).unwrap();

    let data = eng.create_node(ROOT_INO, "data", true, None, 0o0755, 1000, 1000).unwrap();
    let raw = eng.create_node(data, "raw", true, None, 0o0755, 1000, 1000).unwrap();
    eng.create_node(raw, "big.bin", false, Some("kb"), 0o0644, 1000, 1000).unwrap();

    let shared = eng.create_node(ROOT_INO, "shared", true, None, 0o0755, 1000, 1000).unwrap();
    eng.create_node(shared, "lib", true, None, 0o0755, 1000, 1000).unwrap();
    eng
}

#[test]
fn hide_removes_subtree_from_its_spot() {
    let eng = seed_tree("hide");
    let l = lens_over(eng.clone(), "hide /photos/private\n");
    let photos = eng.lookup(ROOT_INO, "photos").unwrap().id;
    // private gone from listing and direct lookup...
    assert_eq!(names(&l.readdir(photos)), vec!["public".to_string()]);
    assert!(l.lookup(photos, "private").is_none());
    // ...while the rest of the tree is untouched.
    assert!(l.lookup(photos, "public").is_some());
    assert!(l.lookup(ROOT_INO, "data").is_some());
}

#[test]
fn only_shows_whitelist_plus_ancestors() {
    let eng = seed_tree("only");
    let l = lens_over(eng.clone(), "only /docs/2026\n");
    // root now only reveals the ancestor chain toward the shown subtree.
    assert_eq!(names(&l.readdir(ROOT_INO)), vec!["docs".to_string()]);
    let docs = eng.lookup(ROOT_INO, "docs").unwrap().id;
    assert_eq!(names(&l.readdir(docs)), vec!["2026".to_string()]);
    // a sibling of the shown subtree is invisible; the shown one is readable.
    assert!(l.lookup(docs, "2025").is_none());
    let d26 = eng.lookup(docs, "2026").unwrap().id;
    assert_eq!(names(&l.readdir(d26)), vec!["a.md".to_string()]);
}

#[test]
fn only_mode_refuses_writes_into_pass_through_ancestor() {
    let eng = seed_tree("onlyw");
    let l = lens_over(eng.clone(), "only /docs/2026\n");
    let docs = eng.lookup(ROOT_INO, "docs").unwrap().id;
    // /docs is shown only as a path to /docs/2026; a new sibling could never
    // appear in the lens, so creation there is refused (EPERM).
    let e = l.create_node(docs, "sneak", true, None, 0o0755, 1000, 1000);
    assert!(matches!(e, Err(grovefs::meta::MetaError::OpNotPermitted)), "{e:?}");
    // but inside the shown subtree it succeeds.
    let d26 = eng.lookup(docs, "2026").unwrap().id;
    assert!(l.create_node(d26, "b.md", false, Some("kbb"), 0o0644, 1000, 1000).is_ok());
    assert_eq!(names(&l.readdir(d26)), vec!["a.md".to_string(), "b.md".to_string()]);
}

#[test]
fn map_relocates_and_governs_write_through() {
    let eng = seed_tree("map");
    let l = lens_over(eng.clone(), "map /data/raw /team/raw\n");
    // A synthesized 'team' dir surfaces at root; the source 'data' stays but
    // its 'raw' child has moved out of its original spot.
    let mut root_names = names(&l.readdir(ROOT_INO));
    assert!(root_names.contains(&"team".to_string()), "{root_names:?}");
    assert!(root_names.contains(&"data".to_string()), "{root_names:?}");
    let data = eng.lookup(ROOT_INO, "data").unwrap().id;
    assert_eq!(names(&l.readdir(data)), Vec::<String>::new(), "raw vacated /data");
    assert!(l.lookup(data, "raw").is_none());

    // Enter the relocated dir under the lens.
    let team = l.lookup(ROOT_INO, "team").unwrap().id;
    assert_eq!(names(&l.readdir(team)), vec!["raw".to_string()]);
    let raw_disp = l.lookup(team, "raw").unwrap();
    let raw = eng.lookup(data, "raw").unwrap().id;
    assert_eq!(raw_disp.id, raw, "alias resolves to the one real inode");
    // `..` from the moved dir leads back onto the lens (team), not the source.
    assert_eq!(l.getattr(raw).unwrap().parent_id, team);

    // Writing through the lens lands on the real entry and is visible in BOTH
    // the relocated view and the raw engine.
    l.create_node(raw, "more.bin", false, Some("km"), 0o0644, 1000, 1000).unwrap();
    assert_eq!(names(&l.readdir(raw)), vec!["big.bin".to_string(), "more.bin".to_string()]);
    assert!(eng.lookup(raw, "more.bin").is_some());

    // Creating directly inside a lens-only (synthesized) dir is refused.
    let e = l.create_node(team, "oops", true, None, 0o0755, 1000, 1000);
    assert!(matches!(e, Err(grovefs::meta::MetaError::OpNotPermitted)), "{e:?}");
    // A name the lens already shows (the synthesized 'team' at root) is
    // taken: a plain mkdir there is EEXIST, not a hidden source entry.
    let e = l.create_node(ROOT_INO, "team", true, None, 0o0755, 1000, 1000);
    assert!(matches!(e, Err(grovefs::meta::MetaError::Exists)), "{e:?}");
    assert!(eng.lookup(ROOT_INO, "team").is_none(), "no hidden source twin");
    // An unknown name under scaffolding is ENOENT, not EPERM.
    let e = l.unlink(team, "ghost");
    assert!(matches!(e, Err(grovefs::meta::MetaError::NotFound)), "{e:?}");

    // Unlinking via the lens removes the real source entry — but the moved
    // dir still has children, so clear those first (POSIX rmdir semantics are
    // unchanged through the lens: ENOTEMPTY is refused, empties delete).
    assert!(matches!(l.unlink(team, "raw"), Err(grovefs::meta::MetaError::NotEmpty)));
    l.unlink(raw, "more.bin").unwrap();
    l.unlink(raw, "big.bin").unwrap();
    l.unlink(team, "raw").unwrap();
    assert!(eng.lookup(data, "raw").is_none());
    assert_eq!(names(&l.readdir(team)), Vec::<String>::new());
    root_names = names(&l.readdir(ROOT_INO));
    assert!(root_names.contains(&"data".to_string()));
}

#[test]
fn link_alias_shows_same_inode_in_two_places() {
    let eng = seed_tree("link");
    let l = lens_over(eng.clone(), "link /shared/lib /proj/mylib\n");
    let shared = eng.lookup(ROOT_INO, "shared").unwrap().id;
    let lib = eng.lookup(shared, "lib").unwrap().id;
    // source spot keeps 'lib' (alias, not a move).
    assert_eq!(names(&l.readdir(shared)), vec!["lib".to_string()]);
    // alias appears under a synthesized /proj named 'mylib', same inode.
    let proj = l.lookup(ROOT_INO, "proj").unwrap().id;
    let listed = l.readdir(proj);
    assert_eq!(names(&listed), vec!["mylib".to_string()]);
    assert_eq!(listed[0].id, lib, "alias shares the real inode");
    assert!(l.lookup(proj, "mylib").is_some());
}

#[test]
fn map_of_a_file_keeps_data_io_addressable() {
    let eng = seed_tree("mapfile");
    let l = lens_over(eng.clone(), "map /docs/2026/a.md /inbox/report.md\n");
    let inbox = l.lookup(ROOT_INO, "inbox").unwrap().id;
    let n = l.lookup(inbox, "report.md").unwrap();
    // Regular file kept its identity + data_ref so the FUSE layer can resolve
    // the object key by inode exactly as before.
    assert_eq!(n.kind, NodeKind::Regular);
    assert_eq!(n.data_ref.as_deref(), Some("ka"));
    // vacated its original spot.
    let d26 = eng.lookup(eng.lookup(ROOT_INO, "docs").unwrap().id, "2026").unwrap().id;
    assert!(l.lookup(d26, "a.md").is_none());
}

#[test]
fn no_rules_is_transparent_passthrough() {
    let eng = seed_tree("none");
    let l = lens_over(eng.clone(), "");
    // Empty lens must not disturb anything.
    assert_eq!(names(&l.readdir(ROOT_INO)), vec![
        "data".to_string(),
        "docs".to_string(),
        "photos".to_string(),
        "shared".to_string(),
    ]);
    let docs = eng.lookup(ROOT_INO, "docs").unwrap().id;
    assert_eq!(names(&l.readdir(docs)), vec!["2025".to_string(), "2026".to_string()]);
}

/// A `map` rule renders an entry under a SYNTHESIZED parent: `display_targets`
/// must return the display coordinate so the invalidation push can close the
/// staleness window that would otherwise last until the attribute TTL (~2s).
#[test]
fn display_targets_fires_for_relocated_entry() {
    let eng = seed_tree("dt1");
    let rules = "map /data/raw /team/raw";
    let l = lens_over(eng.clone(), rules);
    let data_ino = eng.lookup(ROOT_INO, "data").unwrap().id;
    // display_targets(data_ino, "raw") must return the synthesized /team coord.
    let targets = l.display_targets(data_ino, "raw");
    assert!(!targets.is_empty(), "expected at least one display coord");
    // The returned parent must match what the lens traversal allocates.
    let team_node = l.lookup(ROOT_INO, "team").expect("/team must exist");
    let (tp, tn) = &targets[0];
    assert_eq!(*tp, team_node.id, "display parent = traversal-allocated ino");
    assert_eq!(tn.as_str(), "raw", "display name = basename(dst)");
    // Non-relocated: empty.
    let docs_ino = eng.lookup(ROOT_INO, "docs").unwrap().id;
    assert!(
        l.display_targets(docs_ino, "2026").is_empty(),
        "no reloc for /docs/2026"
    );
}

/// A `link` alias renders under a synthesized parent too.
#[test]
fn display_targets_covers_link_alias() {
    let eng = seed_tree("dt2");
    let rules = "link /shared/lib /team/lib";
    let l = lens_over(eng.clone(), rules);
    let shared_ino = eng.lookup(ROOT_INO, "shared").unwrap().id;
    let targets = l.display_targets(shared_ino, "lib");
    assert!(!targets.is_empty(), "link alias must produce display coord");
    let team_node = l.lookup(ROOT_INO, "team").expect("/team dir");
    let (tp, tn) = &targets[0];
    assert_eq!(*tp, team_node.id);
    assert_eq!(tn.as_str(), "lib");
}

/// With only `hide`/`only` rules (no reloc), display_targets stays empty.
#[test]
fn display_targets_empty_without_reloc() {
    let eng = seed_tree("dt3");
    let rules = "hide /photos/private";
    let l = lens_over(eng.clone(), rules);
    let data_ino = eng.lookup(ROOT_INO, "data").unwrap().id;
    assert!(
        l.display_targets(data_ino, "raw").is_empty(),
        "hide doesn't relocate"
    );
}

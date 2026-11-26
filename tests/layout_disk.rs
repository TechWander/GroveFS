//! The on-disk METADATA LAYOUT of a store root, and the one verb that changes
//! it: `grovefs layout show|migrate`.
//!
//! Two properties matter here, and neither is visible from inside the library:
//! every `meta/` entry that a command reads has to be a tree directory (so a
//! stray file, an old build's flat db, or a half-created directory can never be
//! mistaken for a view), and a store that an old build left flat has to be
//! REFUSED with exactly one way out — because half-reading it is how a store
//! gets GC'd to zero bytes (`view list` shows nothing, `refs rebuild` books no
//! holders, `gc --apply` then collects every object as unreferenced).

use std::{fs, path::Path, process::Command};

/// Run the CLI in an isolated environment: no config file, no inherited server
/// addresses, store root always spelled out.
fn gv(args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_grovefs"))
        .args(args)
        .env("GROVEFS_CONFIG", "/nonexistent/grovefs.conf")
        .env_remove("GROVEFS_HOME")
        .env_remove("GROVEFS_META_SERVER")
        .env_remove("GROVEFS_DATA_SERVERS")
        .env_remove("GROVEFS_META_STORE")
        .env_remove("GROVEFS_TOKEN")
        .env_remove("GROVEFS_API_KEY")
        .output()
        .expect("grovefs binary");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A v2 store with one view and two objects under it.
fn populated(dir: &Path) -> std::path::PathBuf {
    let home = dir.join("store");
    let src = dir.join("src");
    fs::create_dir_all(src.join("sub")).unwrap();
    fs::write(src.join("a.txt"), vec![b'a'; 1000]).unwrap();
    fs::write(src.join("sub/b.txt"), vec![b'b'; 2500]).unwrap();
    let h = home.to_str().unwrap();
    let (ok, o, e) = gv(&["view", "--home", h, "create", "--view", "v"]);
    assert!(ok, "view create: {o}{e}");
    let (ok, o, e) = gv(&[
        "import",
        "--home",
        h,
        "--view",
        "v",
        "--source",
        src.to_str().unwrap(),
    ]);
    assert!(ok, "import: {o}{e}");
    home
}

/// What `du` reports as the object total, in bytes: the content side of the
/// store, which a layout move must not touch.
fn total(home: &Path) -> u64 {
    let (ok, o, e) = gv(&[
        "du",
        "--home",
        home.to_str().unwrap(),
        "--view",
        "v",
        "--bytes",
        "--total",
    ]);
    assert!(ok, "du: {o}{e}");
    o.lines()
        .next()
        .unwrap_or_else(|| panic!("no output: {o}"))
        .split('\t')
        .next()
        .unwrap()
        .parse()
        .unwrap()
}

/// Plant the pre-v2 shape: the db moves up to `meta/` and its WAL/SHM are left
/// behind the way a killed `meta-server` leaves them. The bytes inside are
/// untouched, so a successful migration is checkable by content.
fn flatten(home: &Path) {
    let meta = home.join("meta");
    let tree = meta.join("v");
    // Whatever sidecars a clean shutdown left (usually none) travel with the db.
    for sfx in ["", "-wal", "-shm"] {
        let p = tree.join(format!("v.db{sfx}"));
        if p.is_file() {
            fs::rename(&p, meta.join(format!("v.db{sfx}"))).unwrap();
        }
    }
    fs::remove_dir(&tree).ok();
    for sfx in ["-wal", "-shm"] {
        let p = meta.join(format!("v.db{sfx}"));
        if !p.is_file() {
            // Not a real WAL header: SQLite reads it as zero frames, checkpoints
            // past it and deletes it — which is exactly the state that has to
            // finish the move instead of aborting it.
            fs::write(&p, b"not a wal").unwrap();
        }
    }
}

#[test]
fn a_current_store_reports_itself_as_trees_not_as_files() {
    let dir = tempfile::tempdir().unwrap();
    let home = populated(dir.path());
    let (ok, o, e) = gv(&["layout", "--home", home.to_str().unwrap(), "show"]);
    assert!(ok, "layout show: {o}{e}");
    // Since layout v3 a per-tree store reads as v3 with a family mode; the
    // version label moved off the old `v2` when family placement landed.
    assert!(
        o.contains("layout:         v3"),
        "a current store must read as v3:\n{o}"
    );
    assert!(
        o.contains("family mode:"),
        "and name the family policy:\n{o}"
    );
    assert!(o.contains("view trees:     1"), "one physical tree:\n{o}");
    assert!(
        o.contains(&format!("{}", home.join("meta/v/v.db").display())),
        "the tree is named by its own path:\n{o}"
    );
    // `ls meta/` is the tree list: nothing that is not a directory lives there,
    // which is the whole point of the per-tree layout.
    let loose: Vec<String> = fs::read_dir(home.join("meta"))
        .unwrap()
        .filter_map(|en| en.ok())
        .filter(|en| !en.path().is_dir())
        .map(|en| en.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        loose.is_empty(),
        "stray files directly under meta/: {loose:?}"
    );
}

#[test]
fn a_flat_store_is_refused_by_every_reader_that_needs_the_trees() {
    let dir = tempfile::tempdir().unwrap();
    let home = populated(dir.path());
    flatten(&home);
    let h = home.to_str().unwrap();
    // The listing verb that would otherwise answer "no views" — the silence that
    // precedes a `gc --apply` collecting a healthy store.
    let (ok, _o, e) = gv(&["view", "--home", h, "list"]);
    assert!(!ok, "a flat store must not list as empty");
    assert!(
        e.contains("flat pre-v2 metadata layout") && e.contains("layout --meta-dir"),
        "the refusal must name the one way out: {e}"
    );
    // And `show` describes the shape it found, with a move per file.
    let (ok, o, e) = gv(&["layout", "--home", h, "show"]);
    assert!(ok, "show is read-only and safe on a live store: {o}{e}");
    assert!(o.contains("v1 FLAT"), "must name the layout:\n{o}");
    assert!(o.contains("migrate it"), "and the remedy:\n{o}");
    assert!(
        o.contains(&format!("{}  ->  ", home.join("meta/v.db").display()))
            && o.contains(&home.join("meta/v/v.db").display().to_string()),
        "the plan must show v.db travelling to its own directory:\n{o}"
    );
}

/// Every object file under a data root, by path: the thing a reclaim would
/// take away.
fn objects(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for ent in fs::read_dir(&d).unwrap().flatten() {
            let p = ent.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p.display().to_string());
            }
        }
    }
    out.sort();
    out
}

/// A path that is not a store must be REFUSED by the readers whose tree list
/// becomes a delete decision — never answered as "no trees".
///
/// The flat-layout sibling above refuses a store of the WRONG shape. This one
/// refuses a path of the right shape that does not exist at all (a typo in
/// `--meta-dir`). Under layout v2 the tree enumerator reports "no trees" for a
/// missing directory, and that empty list is exactly what `gc --apply` deletes
/// on: with no tree naming them, every object in the data root reads as an
/// orphan.
#[test]
fn a_missing_metadata_root_is_refused_before_it_can_decide_anything() {
    let dir = tempfile::tempdir().unwrap();
    let home = populated(dir.path());
    let h = home.to_str().unwrap();
    let before = objects(&home.join("blocks"));
    assert!(!before.is_empty(), "the store must really hold objects");

    // The typo: a sibling path that was never a store.
    let typo = home.join("typo");
    let t = typo.to_str().unwrap();

    // The audit refuses instead of reporting an empty store.
    let (ok, _o, e) = gv(&["gc", "--meta-dir", t, "--data-dir", h, "--older-than", "0"]);
    assert!(
        !ok,
        "a missing metadata root must not read as an empty store"
    );
    assert!(e.contains("no metadata directory"), "{e}");

    // And the apply that would have reclaimed the whole data root refuses too,
    // with the bytes still there.
    let (ok, _o, e) = gv(&[
        "gc",
        "--meta-dir",
        t,
        "--data-dir",
        h,
        "--older-than",
        "0",
        "--apply",
    ]);
    assert!(
        !ok,
        "a missing metadata root must not reclaim anything: {e}"
    );
    assert_eq!(
        before,
        objects(&home.join("blocks")),
        "the bytes must be untouched"
    );

    // The ledger recount refuses as well — and does not TURN the typo into a
    // store by creating the metadata root it just found missing.
    let (ok, _o, e) = gv(&["refs", "--meta-dir", t, "rebuild"]);
    assert!(
        !ok,
        "a recount over a path that is not a store must refuse: {e}"
    );
    assert!(e.contains("no metadata directory"), "{e}");
    assert!(
        !typo.join("meta").exists(),
        "the refusal must not leave a store behind"
    );
}

#[test]
fn migrate_completes_in_one_pass_even_with_sidecars_it_folds_away() {
    let dir = tempfile::tempdir().unwrap();
    let home = populated(dir.path());
    let h = home.to_str().unwrap();
    let before = total(&home);
    assert_eq!(before, 3500, "the fixture holds 1000 + 2500 bytes");
    flatten(&home);

    // A dry run must be exactly that: the same files, in the same places.
    let (ok, o, e) = gv(&["layout", "--home", h, "migrate", "--dry-run"]);
    assert!(ok, "dry run: {o}{e}");
    assert!(o.contains("nothing was moved"), "{o}");
    assert!(home.join("meta/v.db").is_file(), "dry run moved the db");
    assert!(!home.join("meta/v/v.db").exists(), "dry run created a tree");

    let (ok, o, e) = gv(&["layout", "--home", h, "migrate"]);
    assert!(ok, "one pass has to finish the job: {o}{e}");
    assert!(o.contains("migrated"), "{o}");
    assert!(
        home.join("meta/v/v.db").is_file(),
        "the db is in its tree dir"
    );
    assert!(!home.join("meta/v.db").exists(), "the flat db is gone");
    for sfx in ["-wal", "-shm"] {
        assert!(
            !home.join("meta").join(format!("v.db{sfx}")).exists(),
            "no sidecar may be left lying at the top level"
        );
        assert!(
            !home.join("meta/v").join(format!("v.db{sfx}")).exists(),
            "a sidecar the checkpoint folded away must not be re-planted"
        );
    }
    assert!(
        o.contains("sidecar") && o.contains("needed no move"),
        "the summary has to explain why it moved fewer files than the plan:\n{o}"
    );

    // After: the store reads as v3, the view is back, and the bytes are the same.
    let (ok, o, e) = gv(&["layout", "--home", h, "show"]);
    assert!(ok, "show after migrate: {o}{e}");
    assert!(
        o.contains("layout:         v3") && o.contains("view trees:     1"),
        "{o}"
    );
    let (ok, o, e) = gv(&["view", "--home", h, "list"]);
    assert!(ok, "the trees are readable again: {o}{e}");
    assert!(o.contains('v'), "the view is listed:\n{o}");
    assert_eq!(total(&home), before, "migration moved files, not content");

    // Idempotent: a second run finds nothing to do and still exits 0.
    let (ok, o, e) = gv(&["layout", "--home", h, "migrate"]);
    assert!(ok, "re-running migrate: {o}{e}");
    assert!(o.contains("already uses the v2 layout"), "{o}");
}

/// A grovefs server that has to be gone when the test ends, panic or not.
struct Servant(std::process::Child);

impl Drop for Servant {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The migrator must not run under a writer: a live holder keeps its handle on
/// the old inode, so the file that moved would be authoritative for nobody.
#[test]
fn migrate_refuses_a_store_that_a_grovefs_process_is_using() {
    let dir = tempfile::tempdir().unwrap();
    let home = populated(dir.path());
    let h = home.to_str().unwrap();
    flatten(&home);
    // Any live grovefs process naming this root is the witness: the gate reads
    // /proc, not a lockfile, so a `data-server` serving the same root is enough
    // to prove the store is not quiesced.
    let srv = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = srv.local_addr().unwrap();
    drop(srv);
    let child = Command::new(env!("CARGO_BIN_EXE_grovefs"))
        .args([
            "data-server",
            "--data-dir",
            h,
            "--listen",
            &addr.to_string(),
        ])
        .env("GROVEFS_CONFIG", "/nonexistent/grovefs.conf")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("data-server");
    let _srv = Servant(child);
    // The port, not a sleep: the witness only counts once it is really up.
    let mut up = std::net::TcpStream::connect(addr);
    for _ in 0..50 {
        if up.is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
        up = std::net::TcpStream::connect(addr);
    }
    assert!(up.is_ok(), "data-server never came up on {addr}");
    let (ok, o, e) = gv(&["layout", "--home", h, "migrate"]);
    assert!(!ok, "a busy store must not be migrated: {o}");
    assert!(
        e.contains("in use by local grovefs process"),
        "and say why in the direction of a fix: {e}"
    );
    // Nothing was moved by the refusal.
    assert!(
        home.join("meta/v.db").is_file(),
        "the refusal touched the store"
    );
}

/* ───────────────────────── layout v3: family placement ─────────────────────────

A fork, a derived lens and a pure alias all belong to ONE base view, so since
v3 they land INSIDE that base's directory instead of scattered at the top
level — and each definition file NAMES ITS KIND (`bias_*`, `lens_*`), while a
fork's def travels inside the fork's own tree directory. The policy (`nested`
default, `grouped` separators, `flat` legacy) only decides where NEW members
go — every read resolves by scanning, so flipping it never orphans an existing
tree. These three tests pin the placement the feature is FOR, and the
compatibility that makes it safe to adopt. */

/// One `define` needs a rules file; a lone `only` line is the smallest valid one.
fn rules(dir: &Path, name: &str, body: &str) -> String {
    let p = dir.join(name);
    fs::write(&p, body).unwrap();
    p.to_str().unwrap().to_string()
}

/// v3 default (`nested`): a fork's tree db, a lens's definition and an alias's
/// definition all land inside `meta/<base>/`, not at the top level beside the
/// base — which is the whole point of the归属 change.
#[test]
fn family_members_land_inside_their_base_view() {
    let dir = tempfile::tempdir().unwrap();
    let home = populated(dir.path());
    let h = home.to_str().unwrap();
    let meta = home.join("meta");

    let (ok, o, e) = gv(&["view", "--home", h, "fork", "--name", "lab", "--base", "v"]);
    assert!(ok, "fork: {o}{e}");
    assert!(
        meta.join("v/lab/lab.db").is_file(),
        "the fork tree nests inside its base"
    );
    assert!(
        meta.join("v/lab/lab.layout").is_file(),
        "the fork definition travels with the fork's own tree dir"
    );
    assert!(
        !meta.join("v/lab.layout").exists(),
        "not beside the base any more"
    );
    assert!(!meta.join("lab").exists(), "no top-level fork dir");

    let r = rules(dir.path(), "lens.rules", "only /sub\n");
    let (ok, o, e) = gv(&[
        "view", "--home", h, "define", "--name", "lens_v", "--base", "v", "--rules", &r,
    ]);
    assert!(ok, "define: {o}{e}");
    assert!(
        meta.join("v/lens_v.layout").is_file(),
        "the lens definition lands inside the base dir, its name stating its kind"
    );
    assert!(
        !meta.join("views/lens_v.layout").exists(),
        "not at the top-level defs slot"
    );

    let (ok, o, e) = gv(&["view", "--home", h, "alias", "--name", "bias_pub", "--real", "v"]);
    assert!(ok, "alias: {o}{e}");
    assert!(
        meta.join("v/bias_pub.layout").is_file(),
        "the alias definition lands inside the base dir, its name stating its kind"
    );

    // Kind prefixes are governance: an unprefixed alias/lens is refused.
    let (ok, _o, e) = gv(&["view", "--home", h, "alias", "--name", "pub", "--real", "v"]);
    assert!(!ok, "unprefixed alias must be refused");
    assert!(e.contains("bias_<alias>"), "refusal names the convention: {e}");
    let (ok, _o, e) = gv(&[
        "view", "--home", h, "define", "--name", "docs", "--base", "v", "--rules", &r,
    ]);
    assert!(!ok, "unprefixed lens must be refused");
    assert!(e.contains("lens_<name>"), "refusal names the convention: {e}");

    // The enumerator descends the family layer, so every member is still listed.
    let (ok, o, e) = gv(&["view", "--home", h, "list", "--json"]);
    assert!(ok, "list --json: {o}{e}");
    for name in ["v", "lab", "lens_v", "bias_pub"] {
        assert!(o.contains(name), "`view list` must carry {name}:\n{o}");
    }

    // `layout show` reads v3 + nested and reports the fork by its REAL home.
    let (ok, o, e) = gv(&["layout", "--home", h, "show"]);
    assert!(ok, "show: {o}{e}");
    assert!(
        o.contains("v3") && o.contains("nested"),
        "v3 + nested:\n{o}"
    );
    assert!(
        o.contains(&meta.join("v/lab/lab.db").display().to_string()),
        "the tree list shows the nested home:\n{o}"
    );
}

/// `grouped` keeps the归属 but adds a separator so a base with many members
/// still lists cleanly: trees under `<base>/forks/`, defs under `<base>/views/`.
#[test]
fn grouped_policy_separates_family_members() {
    let dir = tempfile::tempdir().unwrap();
    let home = populated(dir.path());
    let h = home.to_str().unwrap();
    let meta = home.join("meta");
    let (ok, o, e) = gv(&["layout", "--home", h, "set-family", "grouped"]);
    assert!(ok, "set-family grouped: {o}{e}");

    let (ok, o, e) = gv(&["view", "--home", h, "fork", "--name", "lab", "--base", "v"]);
    assert!(ok, "fork: {o}{e}");
    assert!(
        meta.join("v/forks/lab/lab.db").is_file(),
        "grouped fork tree lands behind the base's forks/ separator"
    );

    let r = rules(dir.path(), "lens.rules", "only /sub\n");
    let (ok, o, e) = gv(&[
        "view", "--home", h, "define", "--name", "lens_v", "--base", "v", "--rules", &r,
    ]);
    assert!(ok, "define: {o}{e}");
    assert!(
        meta.join("v/views/lens_v.layout").is_file(),
        "grouped lens def lands behind the base's views/ separator"
    );

    // The separators are reserved names, so `forks`/`views` never read as trees.
    let (ok, o, _e) = gv(&["view", "--home", h, "list", "--json"]);
    assert!(ok, "list: {o}");
    assert!(
        o.contains("lab") && o.contains("lens_v"),
        "members still listed:\n{o}"
    );
}

/// The compatibility promise: a store whose members are at the TOP level (the
/// pre-v3 `flat` shape) keeps reading under a `nested` policy, because every
/// read resolves by scanning; and after the flip only NEW members nest. Flipping
/// never moves — and never orphans — what already exists.
#[test]
fn a_flat_store_still_reads_after_the_policy_flips_to_nested() {
    let dir = tempfile::tempdir().unwrap();
    let home = populated(dir.path());
    let h = home.to_str().unwrap();
    let meta = home.join("meta");

    // Build the family the pre-v3 way: top-level fork dir + top-level def slot.
    let (ok, o, e) = gv(&["layout", "--home", h, "set-family", "flat"]);
    assert!(ok, "set-family flat: {o}{e}");
    let (ok, o, e) = gv(&["view", "--home", h, "fork", "--name", "old", "--base", "v"]);
    assert!(ok, "fork: {o}{e}");
    assert!(
        meta.join("old/old.db").is_file(),
        "a flat fork lands at the top level"
    );
    let r = rules(dir.path(), "oldlens.rules", "only /sub\n");
    let (ok, o, e) = gv(&[
        "view", "--home", h, "define", "--name", "lens_old", "--base", "v", "--rules", &r,
    ]);
    assert!(ok, "define: {o}{e}");
    assert!(
        meta.join("views/lens_old.layout").is_file(),
        "a flat def lands at the top-level defs slot"
    );

    // Flip to the v3 default. Nothing about the existing members moves or is
    // re-homed — but the enumerator must still find them, and a fresh member
    // must nest.
    let (ok, o, e) = gv(&["layout", "--home", h, "set-family", "nested"]);
    assert!(ok, "set-family nested: {o}{e}");
    assert!(
        meta.join("old/old.db").is_file(),
        "the flip must not disturb the existing flat fork"
    );
    assert!(
        meta.join("views/lens_old.layout").is_file(),
        "nor re-home the existing flat def"
    );

    let (ok, o, e) = gv(&["view", "--home", h, "list", "--json"]);
    assert!(ok, "list after flip: {o}{e}");
    for name in ["v", "old", "lens_old"] {
        assert!(
            o.contains(name),
            "a top-level member resolves under the nested policy: {name}\n{o}"
        );
    }

    let (ok, o, e) = gv(&["view", "--home", h, "fork", "--name", "new", "--base", "v"]);
    assert!(ok, "second fork: {o}{e}");
    assert!(
        meta.join("v/new/new.db").is_file(),
        "a member created AFTER the flip nests, proving the policy is live"
    );
}

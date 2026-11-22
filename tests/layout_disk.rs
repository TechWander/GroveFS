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
fn a_v2_store_reports_itself_as_trees_not_as_files() {
    let dir = tempfile::tempdir().unwrap();
    let home = populated(dir.path());
    let (ok, o, e) = gv(&["layout", "--home", home.to_str().unwrap(), "show"]);
    assert!(ok, "layout show: {o}{e}");
    assert!(
        o.contains("layout:         v2"),
        "a current store must read as v2:\n{o}"
    );
    assert!(
        o.contains("view trees:     1"),
        "one physical tree:\n{o}"
    );
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
    assert!(loose.is_empty(), "stray files directly under meta/: {loose:?}");
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
    assert!(home.join("meta/v/v.db").is_file(), "the db is in its tree dir");
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
        o.contains("sidecar(s) needed no move"),
        "the summary has to explain why it moved fewer files than the plan:\n{o}"
    );

    // After: the store reads as v2, the view is back, and the bytes are the same.
    let (ok, o, e) = gv(&["layout", "--home", h, "show"]);
    assert!(ok, "show after migrate: {o}{e}");
    assert!(o.contains("layout:         v2") && o.contains("view trees:     1"), "{o}");
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
        .args(["data-server", "--data-dir", h, "--listen", &addr.to_string()])
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
    assert!(home.join("meta/v.db").is_file(), "the refusal touched the store");
}

//! `grovefs du` against a local store. The store root holds BOTH halves of a
//! deployment (`meta/<view>.db` and `blocks/`), so naming it once has to be
//! enough — and where the bytes demonstrably are not, the command has to say
//! so instead of handing back a tree of zeros.

use std::{fs, path::Path, process::Command};

/// Run the CLI in an isolated environment: no config file, no inherited server
/// addresses, store root always spelled out.
fn gv(args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_grovefs"))
        .args(args)
        .env("GROVEFS_CONFIG", "/nonexistent/grovefs.conf")
        .env_remove("GROVEFS_HOME")
        .env_remove("GROVEFS_META")
        .env_remove("GROVEFS_DATA")
        .env_remove("GROVEFS_STORE")
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

/// A store holding two files (1000 + 2500 bytes) imported from a directory.
fn populated(dir: &Path) -> std::path::PathBuf {
    let home = dir.join("store");
    let src = dir.join("src");
    fs::create_dir_all(src.join("sub")).unwrap();
    fs::write(src.join("a.txt"), vec![b'a'; 1000]).unwrap();
    fs::write(src.join("sub/b.txt"), vec![b'b'; 2500]).unwrap();
    let h = home.to_str().unwrap();
    let (ok, o, e) = gv(&["init", "--home", h, "--view", "v"]);
    assert!(ok, "init: {o}{e}");
    let (ok, o, e) = gv(&["import", "--home", h, "--view", "v", "--source", src.to_str().unwrap()]);
    assert!(ok, "import: {o}{e}");
    home
}

/// The single `--total` line, in bytes.
fn total(home: &Path, extra: &[&str]) -> u64 {
    let mut args = vec![
        "du",
        "--home",
        home.to_str().unwrap(),
        "--view",
        "v",
        "--bytes",
        "--total",
    ];
    args.extend_from_slice(extra);
    let (ok, o, e) = gv(&args);
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

/// Every object file under `home/blocks/`, across all buckets. Taking one per
/// bucket would under-count whenever two random uuids land in the same bucket
/// (~1/256 for a pair), which is a flake rather than a property of the store.
fn objects(home: &Path) -> Vec<std::path::PathBuf> {
    let mut v = Vec::new();
    for bucket in fs::read_dir(home.join("blocks")).unwrap() {
        let bucket = bucket.unwrap().path();
        for entry in fs::read_dir(&bucket).unwrap() {
            let p = entry.unwrap().path();
            if p.is_file() {
                v.push(p);
            }
        }
    }
    v.sort();
    v
}

/// The reported symptom: `--home` resolved the metadata tree, nothing resolved
/// the objects, and every subtree reported 0 while the command exited 0.
#[test]
fn the_store_root_alone_is_enough_to_count_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let home = populated(dir.path());
    assert_eq!(total(&home, &[]), 3500, "du --home --view, no --data-root");
    // Naming the data root explicitly measures the same colocated layout.
    assert_eq!(
        total(&home, &["--data-root", home.to_str().unwrap()]),
        3500,
        "du --home --data-root"
    );
}

/// Objects that are demonstrably not in the data root are counted out loud:
/// their bytes stay out of the total, and stderr says how many.
#[test]
fn an_object_that_is_not_found_is_announced_not_silently_zero() {
    let dir = tempfile::tempdir().unwrap();
    let home = populated(dir.path());
    let objs = objects(&home);
    assert_eq!(objs.len(), 2, "two imported files, two objects");
    // Unlinking the blocks-side hard link leaves the user's source intact and
    // makes the object unresolvable for this store.
    let gone = fs::metadata(&objs[0]).unwrap().len();
    fs::remove_file(&objs[0]).unwrap();

    let args = vec!["du", "--home", home.to_str().unwrap(), "--view", "v", "--bytes"];
    let (ok, o, e) = gv(&args);
    assert!(ok, "still a usable report: {o}{e}");
    assert!(e.contains("warning:"), "gap announced on stderr: {e}");
    assert!(e.contains("1 of 2"), "counts the lost object: {e}");
    let reported: u64 = o
        .lines()
        .next()
        .unwrap()
        .split('\t')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(reported, 3500 - gone, "the rest is still measured");
}

/// No `blocks/` at all under the resolved root means the objects live
/// elsewhere, so the honest answer is a refusal that names what it looked for.
#[test]
fn a_root_without_blocks_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let home = populated(dir.path());
    fs::remove_dir_all(home.join("blocks")).unwrap();
    let (ok, o, e) = gv(&["du", "--home", home.to_str().unwrap(), "--view", "v", "--bytes"]);
    assert!(!ok, "refuses instead of printing zeros; stdout was: {o}");
    assert!(e.contains("no blocks/"), "says which root failed: {e}");
    assert!(e.contains("blocks/"), "and what to pass: {e}");
}

//! P3's acceptance test for the FoundationDB backend: the **same operation
//! sequence** run against the embedded SQLite engine and against a live FDB
//! cluster must produce the same transcript and the same final tree.
//!
//! This is the differential half of `docs/fdb-keyspace.md`'s P3 exit criterion
//! (`fdb_tree_ops_match_sqlite_semantics`). It is deliberately an *equality* test
//! rather than a list of expected values: the SQLite engine is the behaviour
//! specification — same trait, same ops, the backend the whole board has always
//! been run against — so the FDB arm gains nothing by matching a number I wrote
//! down and everything by matching it.
//!
//! `all_reachable_backends_agree_on_one_op_sequence` (below) runs the SAME script
//! through PostgreSQL too, which is what makes this P6's three-way harness
//! ("Sqlite vs Pg vs Fdb, one op sequence, compare `dump_tree()`") instead of a
//! two-way one: every arm that the host can reach is compared against SQLite.
//!
//! The sequence covers what the design doc calls the hard shapes: `link_node`
//! (§3.4 holder keys, §3.5 reverse keys, §4's id/ino split), `rename` (§3.9's
//! cycle check, same-inode no-op, overwrite teardown and ledger effect),
//! `unlink`'s last-name release, and the POSIX error surface (`EEXIST`,
//! `ENOTEMPTY`, `EISDIR`-class type mismatch, `EPERM`, `ECYCLE`).
//!
//! What is NOT compared is the row number. `docs/fdb-keyspace.md` §12.4 records
//! id holes as an attributable difference between a block allocator and a SQLite
//! rowid (a failed `create` costs FDB one number it will never reuse, and an
//! unlink hands a rowid straight back), so "same tree" is tested as a
//! STRUCTURAL isomorphism — [`canonical`] renumbers rows by walking the tree from
//! the root in name order. The numeric claims that ARE obligations get their own
//! checks: the root keeps id 1 (a re-issued id 1 would overwrite the root's own
//! `'d`/`'i` keys — corruption, not a difference), and no two rows share a row id.
//!
//! Like `tests/pg_ssi.rs` and `.build-bin/test_pg.sh`, this self-skips when the
//! dependency is unreachable, so `run_all` stays GREEN on hosts with no FDB
//! cluster: a missing cluster file is an environment fact, not a code failure.

#![cfg(all(feature = "fdb", feature = "native"))]

use grovefs::meta::{MetaError, MetaProvider, MetaStore, ROOT_INO};
use std::collections::{HashMap, HashSet};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::time::Duration;

/// Where the cluster file lives: the env `.build-bin/fdb_env.sh` exports, else
/// the path the P0 spike wrote.
fn cluster_file() -> Option<PathBuf> {
    let p = std::env::var("GROVEFS_FDB_CLUSTER_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/data/grovefs/.fdbtest/test.cluster"));
    p.is_file().then_some(p)
}

/// Cluster files are `token:secret@host:port[,host:port]`. Probe every
/// coordinator: one dead address among live ones is normal (a restarted
/// process), and the client would try them in turn — so the skip condition is
/// "none reachable", not "the first is dead".
fn cluster_reachable(file: &PathBuf) -> bool {
    let Ok(content) = std::fs::read_to_string(file) else {
        return false;
    };
    let Some(addrs) = content.rsplit('@').next() else {
        return false;
    };
    addrs.split(',').any(|a| {
        a.trim()
            .parse::<std::net::SocketAddr>()
            .ok()
            .and_then(|sa| TcpStream::connect_timeout(&sa, Duration::from_millis(300)).ok())
            .is_some()
    })
}

// ---- transcript helpers ----------------------------------------------------
//
// Every helper renders what the TRAIT reports, in a form whose text is
// backend-independent: outcomes become `ok`/the error's Display, ids never appear
// as numbers, and anything numeric (nlink, mode, size, a freed data_ref) is a
// POSIX-visible value that the two backends are obliged to agree on.

fn done(r: &Result<i64, MetaError>) -> String {
    match r {
        Ok(_) => "ok".to_string(),
        Err(e) => format!("err({e})"),
    }
}

/// Rename/unlink report the content key they freed, if any — that `Option` IS
/// the garbage-collection contract, so it belongs in the transcript rather than
/// being checked only at the end.
fn gc(r: &Result<Option<String>, MetaError>) -> String {
    match r {
        Ok(None) => "ok".to_string(),
        Ok(Some(dr)) => format!("ok(gc {dr})"),
        Err(e) => format!("err({e})"),
    }
}

/// The inode number of `name` under `parent`, i.e. the value the FUSE layer would
/// hand back as `attr.st_ino`. Panicking here is not a lost comparison: the first
/// backend to fail it is the one with a broken tree, and the message names the
/// path.
fn ino(s: &dyn MetaProvider, parent: i64, name: &str) -> i64 {
    s.lookup(parent, name)
        .unwrap_or_else(|| panic!("lookup({parent}, {name}): entry missing"))
        .id
}

fn seen(s: &dyn MetaProvider, parent: i64, name: &str) -> Option<grovefs::meta::NodeInfo> {
    s.lookup(parent, name)
}

/// One directory's contents, name-sorted, with a kind suffix — the POSIX-visible
/// shape of the tree at that moment, and independent of any numbering.
fn ls(s: &dyn MetaProvider, parent: i64) -> String {
    let mut items: Vec<String> = s
        .readdir(parent)
        .iter()
        .map(|n| {
            let mark = if n.is_dir() {
                "/"
            } else if n.link_target.is_some() {
                "@"
            } else {
                ""
            };
            format!("{}{}", n.name, mark)
        })
        .collect();
    items.sort();
    format!("[{}]", items.join(" "))
}

/// One dump row, as [`MetaProvider::dump_tree`] defines it.
type Row = (i64, i64, String, bool, Option<String>);

/// The dump, renumbered by STRUCTURE rather than by whichever allocator wrote the
/// numbers: breadth-first from the root row, children in name order. Two backends
/// produce equal output iff their trees agree on shape, names, kinds and content
/// keys, which is exactly the claim P3 makes.
///
/// The renumbering cannot paper over the two numbering bugs that matter, because
/// both are asserted here before any mapping happens: a shared row id (the
/// allocator handing out a live number), and a root that is not id 1.
fn canonical(s: &dyn MetaProvider) -> Vec<(u64, u64, String, bool, Option<String>)> {
    let rows: Vec<Row> = s.dump_tree();
    let mut out = Vec::with_capacity(rows.len());

    // No two rows may claim one id, whatever the ids look like per backend.
    let mut uniq: HashSet<i64> = HashSet::with_capacity(rows.len());
    for r in &rows {
        assert!(uniq.insert(r.0), "duplicate row id {} in the dump", r.0);
    }

    let mut kids: HashMap<i64, Vec<&Row>> = HashMap::new();
    for r in &rows {
        kids.entry(r.1).or_default().push(r);
    }
    for v in kids.values_mut() {
        v.sort_by(|a, b| a.2.cmp(&b.2));
    }
    let none: &[&Row] = &[];

    // The root: parent 0, id ROOT_INO, a directory. Both SQL backends seed it that
    // way (`engine.rs`'s open-time INSERT with NULL parent, `pg_store.rs:456`), and
    // `ensure_root` writes the same row into the subspace.
    let top = kids.get(&0).map(|v| v.as_slice()).unwrap_or(none);
    let root = top
        .iter()
        .copied()
        .find(|r| r.0 == ROOT_INO && r.3)
        .unwrap_or_else(|| panic!("dump has no root directory with id {ROOT_INO}: {rows:?}"));
    assert_eq!(top.len(), 1, "more than one row hangs off parent 0 (a second root?): {top:?}");

    let mut next: u64 = 0;
    let mut label: HashMap<i64, u64> = HashMap::new();
    let mut queue = std::collections::VecDeque::new();
    label.insert(root.0, next);
    out.push((next, u64::MAX, root.2.clone(), root.3, root.4.clone()));
    next += 1;
    queue.push_back(root);
    while let Some(row) = queue.pop_front() {
        for c in kids.get(&row.0).map(|v| v.as_slice()).unwrap_or(none) {
            let here = next;
            next += 1;
            label.insert(c.0, here);
            out.push((here, *label.get(&row.0).unwrap(), c.2.clone(), c.3, c.4.clone()));
            queue.push_back(c);
        }
    }
    assert_eq!(
        out.len(),
        rows.len(),
        "unreachable rows in the dump (a name whose parent no longer exists): {:?}",
        rows.iter()
            .filter(|r| !label.contains_key(&r.0))
            .collect::<Vec<_>>()
    );
    out
}

/// The scripted sequence. Each line is one observable outcome, so a divergence
/// names the operation instead of only the diff.
///
/// `tag` names this RUN's content keys — see [`release_ledger`] for why a fixed
/// `K0` is not safe to share with the bookings of earlier runs.
fn script(s: &dyn MetaProvider, tag: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mkdir = |parent: i64, name: &str| s.create_node(parent, name, true, None, 0o755, 1000, 1000);
    let file = |parent: i64, name: &str, dr: &str| {
        s.create_node(parent, name, false, Some(dr), 0o644, 1000, 1000)
    };
    // A content key is a row in the SHARED, cluster-wide ledger (§3.6, decision
    // D5), not part of the view's own keyspace. Hard-coded `K0`/`K2`/`KX` keys
    // therefore collide with the leftover bookings of every earlier run of this
    // harness, and one leftover booking is enough to turn a later run's
    // `ok(gc K0)` into a plain `ok`. Tagging with the run's view name keeps each
    // run's counts to itself; all arms of one run share the tag, so the
    // transcript still compares line for line.
    let ck = |k: &str| format!("{k}.{tag}");

    // ---- the cast ----------------------------------------------------------
    out.push(format!("mkdir d1 = {}", done(&mkdir(ROOT_INO, "d1"))));
    out.push(format!("mkdir d2 = {}", done(&mkdir(ROOT_INO, "d2"))));
    out.push(format!("mkdir d3 = {}", done(&mkdir(ROOT_INO, "d3"))));
    // Same parent + same name, twice: the uniqueness rule is a KEY property here
    // (§3.1), so its error is part of the transcript, not an aside.
    out.push(format!("mkdir d1 again = {}", done(&mkdir(ROOT_INO, "d1"))));
    let d1 = ino(s, ROOT_INO, "d1");
    out.push(format!("mkdir d1/sub = {}", done(&mkdir(d1, "sub"))));
    out.push(format!("file d1/f1 = {}", done(&file(d1, "f1", &ck("K1")))));
    out.push(format!("file d1/f2 = {}", done(&file(d1, "f2", &ck("K2")))));
    out.push(format!("file /f0 = {}", done(&file(ROOT_INO, "f0", &ck("K0")))));
    out.push(format!("file /x = {}", done(&file(ROOT_INO, "x", &ck("KX")))));
    let d3 = ino(s, ROOT_INO, "d3");
    out.push(format!("file d3/inner = {}", done(&file(d3, "inner", &ck("K9")))));
    let d2 = ino(s, ROOT_INO, "d2");
    out.push(format!("mkdir d2/blank = {}", done(&mkdir(d2, "blank"))));
    out.push(format!("ls / = {}", ls(s, ROOT_INO)));
    out.push(format!("ls d1 = {}", ls(s, d1)));

    // ---- hard links (§3.4 holder keys, §4's id/ino split) ------------------
    let f1 = seen(s, d1, "f1").expect("f1");
    out.push(format!(
        "link d1/f1 -> d1/f1h = {}",
        done(&s.link_node(f1.id, d1, "f1h"))
    ));
    let f1h = seen(s, d1, "f1h").expect("f1h");
    // Identity, stated the way POSIX states it: `NodeInfo.id` IS `st_ino` in all
    // three backends (`build_info` maps it from the `ino` column), so a hard link
    // is two names reporting ONE id while the dump shows two rows. If FDB had kept
    // a second issuing counter for inos, or given the new name its own inode, this
    // is the line that would say so.
    out.push(format!(
        "two names one inode = {} (distinct rows: {})",
        f1h.id == f1.id,
        f1h.name != f1.name
    ));
    out.push(format!("data_ref shared = {}", f1h.data_ref == f1.data_ref));
    // BOTH names must now report 2, so re-read `f1` rather than quoting the
    // pre-link snapshot: SQLite counts `WHERE ino=?` and FDB counts the `'h`
    // prefix, and the two must rise together or one backend is counting names
    // instead of inodes.
    out.push(format!(
        "nlink(f1) = {}",
        seen(s, d1, "f1").expect("f1 after link").nlink
    ));
    out.push(format!("nlink(f1h) = {}", f1h.nlink));
    // An attribute write targets the INODE, so it must land on the other name too
    // — the property that makes `set_mode` an inode write and not a row write,
    // which for FDB means fanning out over the `'h` prefix (§3.4).
    s.set_mode(f1h.id, 0o600);
    out.push(format!(
        "chmod through f1h lands on f1 = {:o}",
        seen(s, d1, "f1").expect("f1 after chmod").mode
    ));
    s.set_uid_gid(f1.id, 0, 0);
    let n = seen(s, d1, "f1h").expect("f1h after chown");
    out.push(format!("chown through f1 lands on f1h = {}/{}", n.uid, n.gid));
    out.push(format!("link a dir = {}", done(&s.link_node(d1, ROOT_INO, "d1l"))));
    out.push(format!("d1l exists = {}", seen(s, ROOT_INO, "d1l").is_some()));
    let sl = done(&s.create_symlink(d1, "sl", "f0", 1000, 1000));
    out.push(format!("symlink d1/sl = {sl}"));
    let sln = seen(s, d1, "sl").expect("sl");
    out.push(format!("nlink(symlink) = {}", sln.nlink));
    out.push(format!("size(symlink) = {}", sln.size));
    out.push(format!(
        "link a symlink = {}",
        done(&s.link_node(sln.id, ROOT_INO, "sl2"))
    ));

    // ---- rename (§3.9): every POSIX branch, plus the no-ops ---------------
    out.push(format!(
        "rename d1/f2 -> d1/f3 = {}",
        gc(&s.rename(d1, "f2", d1, "f3", false))
    ));
    // Overwrite: the displaced name's content key must come back as freed.
    out.push(format!(
        "rename d1/f3 -> /f0 (overwrite) = {}",
        gc(&s.rename(d1, "f3", ROOT_INO, "f0", false))
    ));
    out.push(format!("ls / = {}", ls(s, ROOT_INO)));
    out.push(format!("ls d1 = {}", ls(s, d1)));
    // I1's two no-op shapes, both BEFORE `noreplace` is consulted: the same name,
    // and a sibling hard-link name of the same inode. Getting the order wrong
    // would drop a link and shrink nlink under an EEXIST that never applies.
    out.push(format!(
        "rename /f0 -> /f0 noreplace = {}",
        gc(&s.rename(ROOT_INO, "f0", ROOT_INO, "f0", true))
    ));
    out.push(format!(
        "rename /x -> /f0 noreplace = {}",
        gc(&s.rename(ROOT_INO, "x", ROOT_INO, "f0", true))
    ));
    out.push(format!(
        "rename dir -> file = {}",
        gc(&s.rename(d1, "sub", ROOT_INO, "x", false))
    ));
    out.push(format!(
        "rename dir -> non-empty dir = {}",
        gc(&s.rename(d1, "sub", ROOT_INO, "d3", false))
    ));
    out.push(format!(
        "rename d1 into d1/sub = {}",
        gc(&s.rename(ROOT_INO, "d1", d1, "here", false))
    ));
    out.push(format!(
        "rename d1/sub -> d2/blank (dir onto empty dir) = {}",
        gc(&s.rename(d1, "sub", d2, "blank", false))
    ));
    out.push(format!("ls d2 = {}", ls(s, d2)));
    out.push(format!("ls d1 = {}", ls(s, d1)));
    // A directory's nlink is `2 + directory children`, so a move that reparents a
    // dir must move BOTH endpoints' nlink. This is where a stale `'k`
    // (dir-entry) key would show up as a wrong count rather than a wrong name.
    out.push(format!("nlink(d2) after move-in = {}", seen(s, ROOT_INO, "d2").expect("d2").nlink));

    // ---- unlink: the last name releases, the penultimate must not ----------
    let d1 = ino(s, ROOT_INO, "d1");
    out.push(format!("unlink d1/f1 = {}", gc(&s.unlink(d1, "f1"))));
    // The penultimate name must NOT release the bytes, and must leave its sibling
    // reporting one link: `drop_name_tx`'s count-before-delete rule (§3.5).
    out.push(format!(
        "nlink(f1h) after sibling unlink = {}",
        seen(s, d1, "f1h").expect("f1h").nlink
    ));
    out.push(format!("unlink d1/f1h = {}", gc(&s.unlink(d1, "f1h"))));
    out.push(format!("unlink d1/gone = {}", gc(&s.unlink(d1, "gone"))));
    out.push(format!(
        "unlink non-empty dir = {}",
        gc(&s.unlink(ROOT_INO, "d3"))
    ));
    let d3 = ino(s, ROOT_INO, "d3");
    out.push(format!("unlink d3/inner = {}", gc(&s.unlink(d3, "inner"))));
    out.push(format!("unlink empty d3 = {}", gc(&s.unlink(ROOT_INO, "d3"))));
    out.push(format!("ls / = {}", ls(s, ROOT_INO)));

    // ---- attribute writes land on the surviving inode -----------------------
    let f0 = seen(s, ROOT_INO, "f0").expect("f0 (the moved d1/f3)");
    s.set_mode(f0.id, 0o640);
    s.set_uid_gid(f0.id, 0, 0);
    let f0 = seen(s, ROOT_INO, "f0").expect("f0 after chmod");
    out.push(format!("chmod+chown /f0 = {:o} {}/{}", f0.mode, f0.uid, f0.gid));
    out.push(format!("data_ref of /f0 = {:?}", f0.data_ref));
    out
}

/// Teardown that hands the SHARED ledger back exactly what it borrowed.
///
/// The script ends with two live names (`/f0` and `/x`), so dropping the scratch
/// view with `clear_view` / `DROP SCHEMA` deletes those names WITHOUT unbooking
/// their content keys: the ledger (§3.6 — one cluster-wide row per data_ref, by
/// design outside every view prefix) then keeps a hold on bytes no tree names
/// anymore. Measured on this board: each run of the two-arm test left +1 on `K2`
/// and on `KX` (25 → 27 after two runs), and a single such leftover row for a key
/// is enough to make the next run report `ok` where the SQLite baseline reports
/// `ok(gc …)` — the reproduced shape of the `op #28` divergence. Deleting the
/// names through the trait first is the only path that unbooks, so this runs
/// before the view goes. (SQLite is exempt by construction: its ledger is
/// `refs.db` inside the tempdir that the harness drops.)
fn release_ledger(s: &dyn MetaProvider) {
    // Files and symlinks first — a directory unlinks only once it is empty.
    for (id, parent, name, is_dir, _) in s.dump_tree() {
        if !is_dir && parent != 0 && id != ROOT_INO {
            let _ = s.unlink(parent, &name);
        }
    }
    // Then one level per pass; the script's tree is three deep, so six passes are
    // generous and the loop stops as soon as only the root is left.
    for _ in 0..6 {
        let rows = s.dump_tree();
        if rows.len() <= 1 {
            break;
        }
        for row in rows {
            if row.3 && row.1 != 0 {
                let _ = s.unlink(row.1, &row.2);
            }
        }
    }
    let left = s.dump_tree().len();
    assert!(
        left <= 1,
        "release_ledger left {left} rows behind — their ledger bookings would survive the run"
    );
}

#[test]
fn fdb_tree_ops_match_sqlite_semantics() {
    let Some(file) = cluster_file() else {
        eprintln!("SKIP fdb_tree: no FDB cluster file (set GROVEFS_FDB_CLUSTER_FILE)");
        return;
    };
    if !cluster_reachable(&file) {
        eprintln!("SKIP fdb_tree: cluster {} unreachable", file.display());
        return;
    }
    let uri = format!("fdb://{}", file.display());
    // A distinct subspace per run, so concurrent or repeated runs never share
    // keys. Truncated to well under the 25-byte cap all three backends agree on.
    let nanos = std::time::UNIX_EPOCH.elapsed().map(|d| d.as_nanos()).unwrap_or(0);
    let view: String = format!("dt{:x}.{:x}", std::process::id(), nanos)
        .chars()
        .take(20)
        .collect();

    let dir = tempfile::tempdir().expect("sqlite base");
    let sqlite = MetaStore::open_sqlite(dir.path(), &view, false).expect("open sqlite store");
    let fdb = MetaStore::open_fdb(&uri, &view, false)
        .unwrap_or_else(|e| panic!("open fdb view {view} against {}: {e}", file.display()));

    let t_sqlite = script(&sqlite, &view);
    let t_fdb = script(&fdb, &view);
    // Transcript first: it localises the failure to one operation, which a bare
    // tree diff does not.
    assert_eq!(t_sqlite.len(), t_fdb.len(), "one backend aborted mid-script");
    for (i, (a, b)) in t_sqlite.iter().zip(t_fdb.iter()).enumerate() {
        assert_eq!(a, b, "op #{} diverged (sqlite vs fdb)", i + 1);
    }

    let c_sqlite = canonical(&sqlite);
    let c_fdb = canonical(&fdb);
    assert_eq!(
        c_sqlite, c_fdb,
        "final trees differ after an identical transcript:\nsqlite={c_sqlite:?}\nfdb   ={c_fdb:?}"
    );

    // Leave the cluster as found, in two steps with different jobs: first the
    // names go through `unlink`, which is what releases this run's bookings in the
    // shared ledger (see [`release_ledger`]); then the scratch subspace goes with
    // one range clear. Only after both is "nothing left pointing at bytes" true —
    // the `(gc Kx)` transcript lines prove the mid-script releases, not these last
    // two names. `clear_view` is also P5's `view rm` primitive, so its one range
    // clear is exercised here against a live cluster rather than only in a unit
    // test.
    if let MetaStore::Fdb(f) = &fdb {
        release_ledger(&fdb);
        // …then the subspace itself, through the very operation an operator uses:
        // `view rm --base` is `remove_view`. `unbooked == 0` here is the point —
        // `release_ledger` already gave the bookings back through `unlink`, so an
        // unwind that decremented a second time would be caught on this line
        // (under-counting the ledger is the direction that frees live bytes).
        let rep = f
            .remove_view()
            .unwrap_or_else(|e| panic!("remove scratch view {view}: {e}"));
        assert!(
            rep.keys > 0 && rep.entries > 0,
            "remove_view found nothing in {view}: {rep:?}"
        );
        assert_eq!(
            (rep.unbooked, rep.still_held),
            (0, 0),
            "remove_view re-released bookings that unlink had already released: {rep:?}"
        );
        assert!(
            !grovefs::fdb_store::FdbStore::subspace_exists(&uri, &view).unwrap_or(true),
            "remove_view left the subspace in {view}"
        );
        assert!(
            fdb.dump_tree().is_empty(),
            "remove_view left rows behind in {view}"
        );
    }
    println!(
        "fdb_tree_ops_match_sqlite_semantics: {} ops, {} rows, identical",
        t_sqlite.len(),
        c_sqlite.len()
    );
    // The harness (.build-bin/test_fdb.sh) records the transcript itself, because
    // "48 ops identical" proves agreement but not WHAT was agreed: the POSIX error
    // surface and the `(gc Kx)` contract are the interesting part, and an operator
    // reading a GREEN run should be able to see them without editing code.
    if std::env::var("GROVEFS_FDB_DIFF_VERBOSE").is_ok() {
        for line in &t_sqlite {
            println!("  sqlite|fdb  {line}");
        }
        for r in &c_sqlite {
            println!("  tree      {r:?}");
        }
    }
}

// ---- readdir paging: the contract the mount's cookies rest on ---------------
//
// The differential above compares `readdir` — one whole listing — and therefore
// cannot see a paging-cursor bug: every directory it builds fits in a page, so a
// resume that skips or repeats a row is invisible to it. This test drives the
// SAME resume offsets the FUSE layer derives (arbitrary, from the kernel's 4 KiB
// reply boundary, NOT from our stride) and holds the FDB arm to the SQLite
// listing at every one.
//
// This is not hypothetical coverage. Measured through a mount, a 6,000-name
// directory listed 851 names twice and dropped 851 others while the total count
// stayed exactly 6,000 — a count-only assertion (and every `ls` in the suites)
// passes that. The bug was the checkpoint key's frame of reference: FDB stored
// `(parent, block index)` -> "last name returned", so a resume landed
// `offset % stride` rows late. The fix keys checkpoints by ROW COUNT, the same
// contract `MetaEngine::readdir_page` states, and this test is what keeps the two
// engines from drifting apart again.
#[test]
fn fdb_readdir_pages_stitch_into_the_sqlite_listing() {
    let Some(file) = cluster_file() else {
        eprintln!("SKIP fdb_paging: no FDB cluster file (set GROVEFS_FDB_CLUSTER_FILE)");
        return;
    };
    if !cluster_reachable(&file) {
        eprintln!("SKIP fdb_paging: cluster {} unreachable", file.display());
        return;
    }
    let uri = format!("fdb://{}", file.display());
    let nanos = std::time::UNIX_EPOCH.elapsed().map(|d| d.as_nanos()).unwrap_or(0);
    let view: String = format!("pg{:x}.{:x}", std::process::id(), nanos)
        .chars()
        .take(20)
        .collect();
    // One content key for every file: the ledger is cluster-wide and shared with
    // the other tests in this file (which run in parallel), so a bookkeeping
    // number that is unique to this run is the only one safe to assert on.
    let dr = format!("PG{:x}", nanos);

    let dir = tempfile::tempdir().expect("sqlite base");
    let sqlite = MetaStore::open_sqlite(dir.path(), &view, false).expect("open sqlite store");
    let fdb = MetaStore::open_fdb(&uri, &view, false)
        .unwrap_or_else(|e| panic!("open fdb view {view} against {}: {e}", file.display()));

    // 700 names = 21.875 strides: the tail page is short and every resume past
    // 672 crosses a boundary that is not a page boundary of any test chunk.
    for i in 0..700 {
        let nm = format!("f{i:04}");
        for s in [&sqlite, &fdb] {
            s.create_node(ROOT_INO, &nm, false, Some(&dr), 0o644, 1000, 1000)
                .unwrap_or_else(|e| panic!("create {nm}: {e}"));
        }
    }
    let listing = |s: &MetaStore| -> Vec<String> {
        s.readdir(ROOT_INO).into_iter().map(|n| n.name).collect()
    };
    let names = |s: &MetaStore, off: usize, lim: usize| -> Vec<String> {
        s.readdir_page(ROOT_INO, off, lim)
            .into_iter()
            .map(|n| n.name)
            .collect()
    };
    let full = listing(&sqlite);
    assert_eq!(full.len(), 700, "700 files and nothing else");

    // A sequential walk at chunk sizes that do and do not divide the stride (32).
    // `off` advances by what came back, exactly as the mount's `child_i` does.
    for chunk in [1usize, 7, 31, 32, 33, 100, 257] {
        let mut paged: Vec<String> = Vec::new();
        let mut off = 0usize;
        loop {
            let page = names(&fdb, off, chunk);
            if page.is_empty() {
                break;
            }
            off += page.len();
            paged.extend(page);
            assert!(off <= 700, "chunk {chunk} walked past the end of the directory");
        }
        assert_eq!(paged, full, "chunk size {chunk} must reassemble the listing");
    }

    // Lay the checkpoints one big page leaves behind, then resume where the KERNEL
    // would: every offset here is mid-stride, and several sit inside the window
    // the checkpoints cover (the fast path) rather than outside it (the fallback
    // that scanned from row 0 and therefore never had the bug).
    assert_eq!(names(&fdb, 0, 300).len(), 300);
    for off in [1usize, 2, 31, 33, 47, 64, 96, 97, 150, 299, 300, 301, 511, 640, 699] {
        let want = &full[off..(off + 9).min(full.len())];
        assert_eq!(
            names(&fdb, off, 9),
            want,
            "resume at offset {off} must serve the same window"
        );
    }

    // A deletion shifts every later index: the checkpoint that covered it must be
    // gone, or the resume silently skips the row after the hole.
    for s in [&sqlite, &fdb] {
        s.unlink(ROOT_INO, "f0100").unwrap();
    }
    let shifted = listing(&sqlite);
    assert_eq!(shifted.len(), 699);
    assert_eq!(names(&fdb, 0, 300).len(), 300, "re-lay checkpoints after the delete");
    for off in [99usize, 100, 101, 350, 698] {
        let want = &shifted[off..(off + 9).min(shifted.len())];
        assert_eq!(
            names(&fdb, off, 9),
            want,
            "resume at offset {off} after a delete must follow the new order"
        );
    }

    if let MetaStore::Fdb(f) = &fdb {
        release_ledger(&fdb);
        let rep = f
            .remove_view()
            .unwrap_or_else(|e| panic!("remove scratch view {view}: {e}"));
        assert_eq!(
            (rep.unbooked, rep.still_held),
            (0, 0),
            "paging test left bookings behind: {rep:?}"
        );
        assert!(
            !grovefs::fdb_store::FdbStore::subspace_exists(&uri, &view).unwrap_or(true),
            "paging test left the subspace in {view}"
        );
    }
    println!("fdb_readdir_pages_stitch_into_the_sqlite_listing: 700 names, 7 chunk sizes + 20 resumes");
}

// ---- the checkpoint is a CACHE, not state ---------------------------------
//
// `readdir_page` keeps its `(parent, row)` -> name hints in the SERVER's memory.
// A meta-server restart (or an idle-engine eviction) throws that map away while a
// client is mid-enumeration, and the next page therefore arrives at a store that
// has never seen the earlier ones. The contract that makes this safe is that a
// hint only ever SHORTENS the scan: with no hint the page is produced by scanning
// from row 0 and skipping `offset`, which is slower and exactly as correct. That
// is a property no amount of same-process paging tests can pin — they all reuse
// the store that laid the checkpoints.
//
// So this test walks the directory through a FRESH store per page: every page
// after the first sees a cold cache, which is the restart case without the
// flakiness of actually killing a process mid-`getdents`.
#[test]
fn fdb_readdir_pages_survive_a_cold_checkpoint_cache() {
    let Some(file) = cluster_file() else {
        eprintln!("SKIP fdb_cold_cache: no FDB cluster file (set GROVEFS_FDB_CLUSTER_FILE)");
        return;
    };
    if !cluster_reachable(&file) {
        eprintln!("SKIP fdb_cold_cache: cluster {} unreachable", file.display());
        return;
    }
    let uri = format!("fdb://{}", file.display());
    let nanos = std::time::UNIX_EPOCH.elapsed().map(|d| d.as_nanos()).unwrap_or(0);
    let view: String = format!("cc{:x}.{:x}", std::process::id(), nanos)
        .chars()
        .take(20)
        .collect();
    let dr = format!("CC{:x}", nanos);

    let dir = tempfile::tempdir().expect("sqlite base");
    let sqlite = MetaStore::open_sqlite(dir.path(), &view, false).expect("open sqlite store");
    let fdb = MetaStore::open_fdb(&uri, &view, false)
        .unwrap_or_else(|e| panic!("open fdb view {view} against {}: {e}", file.display()));

    // 311 names: not a multiple of the 32-row stride, so the last full page ends
    // mid-stride and the resume that follows has to skip a partial stride.
    for i in 0..311 {
        let nm = format!("c{i:04}");
        for s in [&sqlite, &fdb] {
            s.create_node(ROOT_INO, &nm, false, Some(&dr), 0o644, 1000, 1000)
                .unwrap_or_else(|e| panic!("create {nm}: {e}"));
        }
    }
    let full: Vec<String> = sqlite.readdir(ROOT_INO).into_iter().map(|n| n.name).collect();
    assert_eq!(full.len(), 311);

    // (a) every page through a NEW store: 7 does not divide 32, so page starts
    // land inside the stride window on most iterations.
    let mut walked: Vec<String> = Vec::new();
    let mut off = 0usize;
    let mut opens = 0usize;
    loop {
        let fresh = MetaStore::open_fdb(&uri, &view, false)
            .unwrap_or_else(|e| panic!("re-open view {view} at offset {off}: {e}"));
        opens += 1;
        let page: Vec<String> = fresh
            .readdir_page(ROOT_INO, off, 7)
            .into_iter()
            .map(|n| n.name)
            .collect();
        if page.is_empty() {
            break;
        }
        off += page.len();
        walked.extend(page);
        assert!(off <= 311, "cold-cache walk ran past the end at offset {off}");
    }
    assert_eq!(walked, full, "a cold cache must reassemble the listing");
    assert!(opens >= 44, "one open per page, but only {opens} happened");

    // (b) hints laid by one store, read by another: the resume must not trust a
    // hint it did not lay, and whether it finds one or not the window is the same.
    assert_eq!(fdb.readdir_page(ROOT_INO, 0, 300).len(), 300);
    let other = MetaStore::open_fdb(&uri, &view, false).expect("second store");
    for off in [3usize, 32, 33, 96, 160, 299, 310] {
        let want = &full[off..(off + 5).min(full.len())];
        let got: Vec<String> = other
            .readdir_page(ROOT_INO, off, 5)
            .into_iter()
            .map(|n| n.name)
            .collect();
        assert_eq!(got, want, "resume at {off} through a second store");
    }

    if let MetaStore::Fdb(f) = &fdb {
        release_ledger(&fdb);
        let rep = f
            .remove_view()
            .unwrap_or_else(|e| panic!("remove scratch view {view}: {e}"));
        assert_eq!((rep.unbooked, rep.still_held), (0, 0), "cold-cache test leaked bookings: {rep:?}");
        assert!(
            !grovefs::fdb_store::FdbStore::subspace_exists(&uri, &view).unwrap_or(true),
            "cold-cache test left the subspace in {view}"
        );
    }
    println!("fdb_readdir_pages_survive_a_cold_checkpoint_cache: 311 names, {opens} fresh stores");
}

// ---- the third arm --------------------------------------------------------
//
// `docs/fdb-keyspace.md` §14 registers the harness P6 asks for as a THREE-way
// differential, and the test above only pins FDB against SQLite. PostgreSQL is the
// backend that has shipped longest, so it belongs in the same comparison: one
// `script()`, one `canonical()`, and every arm the host can reach measured against
// the SQLite specification. A PG-vs-FDB divergence therefore names itself too
// (both are compared to the same baseline, and to each other transitively).

/// Probe PostgreSQL the way `.build-bin/test_pg.sh` does — `PGHOST`/`PGPORT`
/// (default 127.0.0.1:5432) — and give up (skip) rather than fail when nothing
/// answers. `ToSocketAddrs` so a host name in `PGHOST` works too.
fn pg_endpoint() -> Option<(String, u16)> {
    let host = std::env::var("PGHOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let port: u16 = std::env::var("PGPORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(5432);
    let live = (host.as_str(), port)
        .to_socket_addrs()
        .map(|mut addrs| {
            addrs
                .any(|sa| TcpStream::connect_timeout(&sa, Duration::from_millis(300)).is_ok())
        })
        .unwrap_or(false);
    live.then_some((host, port))
}

/// `pg_store::schema_for` is private and its scheme is `grovefs_v_` + lowercase
/// hex of the view name; the harness recomputes it for one purpose — dropping the
/// scratch schema it created, which no library call does (`view rm` refuses a PG
/// deployment by design). If that scheme ever changes this becomes a no-op
/// (`IF EXISTS`), leaking one schema per run instead of failing a differential for
/// a housekeeping detail.
fn pg_schema_of(view: &str) -> String {
    let mut hex = String::with_capacity(view.as_bytes().len() * 2);
    for b in view.as_bytes() {
        hex.push_str(&format!("{b:02x}"));
    }
    format!("grovefs_v_{hex}")
}

#[test]
fn all_reachable_backends_agree_on_one_op_sequence() {
    let nanos = std::time::UNIX_EPOCH
        .elapsed()
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // One name for all three arms: 20 bytes clears PG's 25-byte view cap and
    // FDB's own limit, and the SQLite engine takes it as a file stem.
    let view: String = format!("dw{:x}.{:x}", std::process::id(), nanos)
        .chars()
        .take(20)
        .collect();
    let dir = tempfile::tempdir().expect("sqlite base");
    let sqlite = MetaStore::open_sqlite(dir.path(), &view, false).expect("open sqlite store");
    let mut arms: Vec<(&str, Vec<String>, Vec<(u64, u64, String, bool, Option<String>)>)> =
        vec![("sqlite", script(&sqlite, &view), canonical(&sqlite))];

    if let Some((host, port)) = pg_endpoint() {
        let uri = format!("postgres://grove:grovepw@{host}:{port}/grovefs");
        let pg = MetaStore::open_pg(&uri, &view, false)
            .unwrap_or_else(|e| panic!("open pg view {view} at {host}:{port}: {e}"));
        arms.push(("postgres", script(&pg, &view), canonical(&pg)));
        // `DROP SCHEMA CASCADE` takes the tree but not this view's rows in the
        // shared `grovefs_refs.holders` table, so the bookings go first.
        release_ledger(&pg);
        drop(pg);
        if let Ok(mut c) = postgres::Client::connect(uri.as_str(), postgres::NoTls) {
            let _ = c.batch_execute(&format!(
                "DROP SCHEMA IF EXISTS {} CASCADE",
                pg_schema_of(&view)
            ));
        }
    } else {
        eprintln!("  (no PostgreSQL reachable: this run compares the arms that exist)");
    }

    match cluster_file() {
        Some(file) if cluster_reachable(&file) => {
            let uri = format!("fdb://{}", file.display());
            let fdb = MetaStore::open_fdb(&uri, &view, false)
                .unwrap_or_else(|e| panic!("open fdb view {view} against {}: {e}", file.display()));
            arms.push(("foundationdb", script(&fdb, &view), canonical(&fdb)));
            if let MetaStore::Fdb(f) = &fdb {
                release_ledger(&fdb);
                f.remove_view()
                    .unwrap_or_else(|e| panic!("remove scratch view {view}: {e}"));
                assert!(
                    !grovefs::fdb_store::FdbStore::subspace_exists(&uri, &view).unwrap_or(true),
                    "remove_view left the subspace in {view}"
                );
            }
        }
        _ => eprintln!("  (no FDB cluster reachable: this run compares the arms that exist)"),
    }

    if arms.len() < 2 {
        eprintln!(
            "SKIP three-way: only the sqlite arm is up, so there is nothing to diff against \
             (start a cluster with .build-bin/fdb_env.sh up, or PG on $PGHOST:$PGPORT)"
        );
        return;
    }
    let (base, bt, bc) = &arms[0];
    for (label, t, c) in &arms[1..] {
        assert_eq!(
            bt.len(),
            t.len(),
            "{label} ran a different number of ops than {base} — one backend aborted mid-script"
        );
        for (i, (a, b)) in bt.iter().zip(t.iter()).enumerate() {
            assert_eq!(a, b, "op #{} diverged ({base} vs {label})", i + 1);
        }
        assert_eq!(
            bc, c,
            "final trees differ between {base} and {label} after an identical transcript"
        );
    }
    let names: Vec<&str> = arms.iter().map(|a| a.0).collect();
    println!(
        "three-way differential: {} arms [{}] agree on {} ops and {} rows",
        arms.len(),
        names.join(", "),
        bt.len(),
        bc.len()
    );
}

// ---- teardown: `view rm --base` = `remove_view` ----------------------------
//
// The two differential tests above end with `release_ledger` + `remove_view`,
// which proves the idempotent door. They cannot prove the UNWIND itself: their
// names are unlinked through the trait first, so by the time the view is removed
// there is nothing left to unbook. That path — a tree deleted WITHOUT walking its
// names, which is what an operator does — is what this test measures.

#[test]
fn fdb_remove_view_unwinds_the_shared_ledger_exactly_once() {
    use grovefs::fdb_store::{list_subspaces, FdbStore};

    let Some(file) = cluster_file() else {
        eprintln!("SKIP fdb_remove_view: no FDB cluster file (set GROVEFS_FDB_CLUSTER_FILE)");
        return;
    };
    if !cluster_reachable(&file) {
        eprintln!("SKIP fdb_remove_view: cluster {} unreachable", file.display());
        return;
    }
    let uri = format!("fdb://{}", file.display());
    let nanos = std::time::UNIX_EPOCH.elapsed().map(|d| d.as_nanos()).unwrap_or(0);
    let base: String = format!("rv{:x}{:x}", std::process::id(), nanos)
        .chars()
        .take(18)
        .collect();
    let (va, vb, vc) = (format!("{base}a"), format!("{base}b"), format!("{base}c"));
    // A content key unique to this run. The ledger is cluster-wide, so a key some
    // other view also names would make every count below depend on leftover state
    // — and this file's other two tests run in PARALLEL with this one, releasing
    // their own bookings on the same cluster.
    let dr = format!("RV{:x}", nanos);

    // Three views naming ONE content key. The row must count VIEWS, not names and
    // not bytes.
    let stores: Vec<MetaStore> = [&va, &vb, &vc]
        .iter()
        .map(|v| {
            MetaStore::open_fdb(&uri, v, false)
                .unwrap_or_else(|e| panic!("open fdb view {v} against {}: {e}", file.display()))
        })
        .collect();
    for (i, s) in stores.iter().enumerate() {
        s.create_node(ROOT_INO, "shared", false, Some(&dr), 0o644, 1000, 1000)
            .unwrap_or_else(|e| panic!("create in arm {i}: {e}"));
    }
    // Asked PER CONTENT KEY, not by counting the whole `refs` subspace: a global
    // key count cannot tell a correct unwind from a sibling test's concurrent
    // teardown (that is exactly how the first version of this test failed — 538
    // expected, 528 observed, nothing wrong with `remove_view`).
    let holders = |i: usize| -> i64 {
        match &stores[i] {
            MetaStore::Fdb(f) => f
                .ledger_holders(&dr)
                .unwrap_or_else(|e| panic!("read the ledger for {dr}: {e}")),
            _ => unreachable!("open_fdb yields MetaStore::Fdb"),
        }
    };
    assert_eq!(holders(0), 3, "one booking per VIEW naming the content key");

    // The inventory sees all three, and counts the root row plus the file.
    let inv = list_subspaces(&uri).unwrap_or_else(|e| panic!("inventory: {e}"));
    for v in [&va, &vb, &vc] {
        let row = inv
            .iter()
            .find(|s| &s.name == v)
            .unwrap_or_else(|| panic!("{v} is missing from the cluster inventory"));
        assert!(!row.ledger, "{v} reported as a ledger");
        assert_eq!(row.entries, 2, "{v}: root row + the one file");
    }

    let remove = |i: usize, why: &str| {
        match &stores[i] {
            MetaStore::Fdb(f) => f
                .remove_view()
                .unwrap_or_else(|e| panic!("remove view {i} ({why}): {e}")),
            _ => unreachable!("open_fdb yields MetaStore::Fdb"),
        }
    };

    // A goes: two holders are left, so nothing may be released and the row stays.
    let r1 = remove(0, "first");
    assert_eq!(
        r1.entries, 2,
        "the report describes the tree it found: {r1:?}"
    );
    assert_eq!(
        (r1.unbooked, r1.still_held),
        (0, 1),
        "A's removal must not free bytes B and C still name: {r1:?}"
    );
    assert_eq!(holders(1), 2, "one view left, so two bookings are left");
    assert!(
        !FdbStore::subspace_exists(&uri, &va).unwrap_or(true),
        "A's subspace survived its own removal"
    );

    // A AGAIN: this is the idempotency contract. A second decrement here would
    // leave the row reading 1 while two views still name those bytes, and the
    // next removal (B) would then wrongly report "unbooked" — the unsafe
    // direction, because a later GC would free bytes C is still reading.
    let r2 = remove(0, "again");
    assert_eq!(
        (r2.unbooked, r2.still_held, r2.keys, r2.entries),
        (0, 0, 0, 0),
        "a re-run on an already-removed view must be a no-op: {r2:?}"
    );
    assert_eq!(
        holders(1),
        2,
        "a re-run moved the ledger — the chunk pair (delete 'r rows + decrement) is not atomic"
    );

    // B goes: C is still a holder, so B's removal must release nothing either.
    let r3 = remove(1, "only holder left is C");
    assert_eq!(
        (r3.unbooked, r3.still_held),
        (0, 1),
        "B's removal must not free bytes C still reads: {r3:?}"
    );
    assert_eq!(holders(2), 1, "one view left, one booking left");

    // C goes: the last holder. The row is DELETED rather than left at zero, and
    // the ref is named in the report so a caller that owns a data root can free
    // the bytes (that is what `view rm --base` does with it).
    let r4 = remove(2, "last holder");
    assert_eq!((r4.unbooked, r4.still_held), (1, 0), "{r4:?}");
    assert_eq!(r4.unbooked_refs, vec![dr.clone()], "{r4:?}");
    // Zero holders is the absence of a booking, not a count: the row must be
    // gone, because a ledger full of zero rows is a leak the next GC cannot
    // distinguish from a live hold.
    assert_eq!(holders(2), 0, "the ledger still books {dr}");

    // And the three subspaces are out of the cluster inventory. Counted as "these
    // names are not there" rather than "the total dropped", because the other two
    // tests in this file share this cluster.
    let after = list_subspaces(&uri).unwrap_or_else(|e| panic!("inventory: {e}"));
    for v in [&va, &vb, &vc] {
        assert!(
            after.iter().all(|s| &s.name != v),
            "{v} is still in the cluster inventory after its removal"
        );
    }
    assert!(
        !FdbStore::subspace_exists(&uri, &vc).unwrap_or(true),
        "C's subspace survived its own removal"
    );

    println!(
        "fdb_remove_view: 3 views sharing one content key — unwind counted 1 holder at a time, deleted the row at zero, re-run was a no-op"
    );
}

// ---- a replace-import must not leave dead rows in the SHARED ledger --------
//
// `refs` is the only subspace every view writes to, so a row left there outlives
// the tree that booked it: no view can release it any more, and
// `ledger_holders` cannot even report it — an absent key reads as 0 by design,
// so a leaked 0 and a clean absence are the same number through that door. What
// an operator sees instead is a `refs` subspace with keys and no tree beside it,
// which is exactly what `view scan` showed after a run of the parameterised
// suites. The path that produced it was `import --replace`: it released the old
// tree's units with a bare decrement, while `unlink` and `remove_view` delete the
// row at zero. This pins the reproduction end to end — book, replace-import over
// the only holder, book again, replace again — and asserts the ledger is EMPTY,
// not merely that it reads 0.
#[test]
fn fdb_replace_import_deletes_the_ledger_row_it_emptied() {
    let Some(file) = cluster_file() else {
        eprintln!("SKIP fdb_import_ledger: no FDB cluster file (set GROVEFS_FDB_CLUSTER_FILE)");
        return;
    };
    if !cluster_reachable(&file) {
        eprintln!("SKIP fdb_import_ledger: cluster {} unreachable", file.display());
        return;
    }
    let uri = format!("fdb://{}", file.display());
    let nanos = std::time::UNIX_EPOCH.elapsed().map(|d| d.as_nanos()).unwrap_or(0);
    let view: String = format!("ri{:x}.{:x}", std::process::id(), nanos)
        .chars()
        .take(20)
        .collect();
    // Unique refs: the ledger is cluster-wide and shared with the tests running
    // in parallel, so the assertions below can only name refs this test booked.
    let (dr_a, dr_b) = (format!("RPA{:x}", nanos), format!("RPB{:x}", nanos));

    let fdb = MetaStore::open_fdb(&uri, &view, false)
        .unwrap_or_else(|e| panic!("open fdb view {view} against {}: {e}", file.display()));

    // A dump carrying nothing but the root row: "the same tree, without the
    // file that held the bytes". 0755 = 493.
    let dir = tempfile::tempdir().expect("dump dir");
    let dump = dir.path().join("root.jsonl");
    std::fs::write(
        &dump,
        format!(
            "{{\"format\":\"{}\",\"version\":{},\"view\":\"{}\",\"kind\":\"physical\"}}\n\
             {{\"id\":1,\"ino\":1,\"parent_id\":null,\"name\":\"/\",\"is_dir\":1,\"mode\":493}}\n",
            grovefs::meta::VIEW_EXPORT_FORMAT,
            grovefs::meta::VIEW_EXPORT_VERSION,
            view
        ),
    )
    .expect("write dump");

    let f = match &fdb {
        MetaStore::Fdb(f) => f,
        _ => unreachable!("open_fdb returns the Fdb arm"),
    };
    let row_of = |dr: &str| -> Option<i64> {
        f.ledger_rows()
            .unwrap_or_else(|e| panic!("ledger inventory: {e}"))
            .into_iter()
            .find(|(d, _)| d == dr)
            .map(|(_, n)| n)
    };

    // (1) Book one unit: a file naming a content key.
    fdb.create_node(ROOT_INO, "a", false, Some(&dr_a), 0o644, 1000, 1000)
        .expect("create a");
    assert_eq!(row_of(&dr_a), Some(1), "create must book exactly one unit");

    // (2) Replace the tree with one that no longer names it. The old tree was
    // the ONLY holder, so the row must disappear rather than sit at 0.
    let st = f.import_jsonl(&dump, true).expect("replace-import over a");
    assert!(st.replaced && st.nodes == 1, "{st:?}");
    assert_eq!(
        row_of(&dr_a),
        None,
        "replace-import left the emptied ledger row behind — a row no tree can release"
    );

    // (3) A second book + replace, the shape that turned the leak into -1: with
    // the row already at 0 a further decrement could not be observed at all.
    fdb.create_node(ROOT_INO, "b", false, Some(&dr_b), 0o644, 1000, 1000)
        .expect("create b");
    assert_eq!(row_of(&dr_b), Some(1), "second create must book one unit");
    f.import_jsonl(&dump, true).expect("second replace-import");
    assert_eq!(row_of(&dr_b), None, "second replace leaked its ref");
    assert_eq!(row_of(&dr_a), None, "the first ref came back");

    // Teardown: the view holds only the root, so nothing is booked and the
    // subspace goes away whole.
    let rep = f
        .remove_view()
        .unwrap_or_else(|e| panic!("remove scratch view {view}: {e}"));
    assert_eq!(
        (rep.unbooked, rep.still_held),
        (0, 0),
        "the emptied view should have had nothing left to release: {rep:?}"
    );
    assert_eq!(row_of(&dr_a), None, "ref A back after the view went");
    assert_eq!(row_of(&dr_b), None, "ref B back after the view went");
    assert!(
        !grovefs::fdb_store::FdbStore::subspace_exists(&uri, &view).unwrap_or(true),
        "import test left the subspace in {view}"
    );
    println!(
        "fdb_replace_import: book -> replace -> book -> replace, ledger rows deleted at zero"
    );
}

// ---- an import must refuse a CORPSE (keys, but no root row) ----------------
//
// `view create` refuses that shape by name, because seeding a root on the
// leftovers resurrects a corpse as a mountable tree whose inode table predates
// the caller. `import` used to accept it: measured on the dev cluster, importing
// a one-node dump onto a 9-key corpse returned rc 0, the view went from 9 keys /
// 1 entry to 13 keys / 2 entries, and BOTH refs stayed in the shared ledger —
// i.e. the tree the dump described inherited the old tree's `'r` rows and their
// bookings. Nothing can mount a corpse, so no later teardown would ever look for
// those rows again: the old bytes stay booked to this view forever.
//
// The guard asks the cluster the SAME question `view scan` asks (`KIND=corpse`:
// keys AND no root row), inside the transaction that would do the import, and
// `--replace` does not bypass it — replacing presupposes a root row for the dump
// to replace. This pins both doors, the no-write property, and the exit the
// refusal names: `view rm --base` on a corpse still hands the old booking back.
#[test]
fn fdb_import_refuses_a_corpse() {
    let Some(file) = cluster_file() else {
        eprintln!("SKIP fdb_import_corpse: no FDB cluster file (set GROVEFS_FDB_CLUSTER_FILE)");
        return;
    };
    if !cluster_reachable(&file) {
        eprintln!("SKIP fdb_import_corpse: cluster {} unreachable", file.display());
        return;
    }
    let uri = format!("fdb://{}", file.display());
    let nanos = std::time::UNIX_EPOCH.elapsed().map(|d| d.as_nanos()).unwrap_or(0);
    let view: String = format!("cp{:x}.{:x}", std::process::id(), nanos)
        .chars()
        .take(20)
        .collect();
    // Unique refs: the ledger is shared with every other test on this cluster, so
    // the assertions can only name refs this test booked.
    let (dr_a, dr_b) = (format!("CPA{:x}", nanos), format!("CPB{:x}", nanos));

    let fdb = MetaStore::open_fdb(&uri, &view, false)
        .unwrap_or_else(|e| panic!("open fdb view {view} against {}: {e}", file.display()));
    fdb.create_node(ROOT_INO, "a", false, Some(&dr_a), 0o644, 1000, 1000)
        .expect("create a");

    // The dump names a different content key, so "the refused import wrote
    // nothing" is visible in the ledger too, not only in the key count.
    let dir = tempfile::tempdir().expect("dump dir");
    let dump = dir.path().join("b.jsonl");
    std::fs::write(
        &dump,
        format!(
            "{{\"format\":\"{}\",\"version\":{},\"view\":\"{}\",\"kind\":\"physical\"}}\n\
             {{\"id\":1,\"ino\":1,\"parent_id\":null,\"name\":\"/\",\"is_dir\":1,\"mode\":493}}\n\
             {{\"id\":2,\"ino\":2,\"parent_id\":1,\"name\":\"g\",\"is_dir\":0,\"data_ref\":\"{}\",\"mode\":420}}\n",
            grovefs::meta::VIEW_EXPORT_FORMAT,
            grovefs::meta::VIEW_EXPORT_VERSION,
            view,
            dr_b
        ),
    )
    .expect("write dump");

    let f = match &fdb {
        MetaStore::Fdb(f) => f,
        _ => unreachable!("open_fdb returns the Fdb arm"),
    };
    let row_of = |dr: &str| -> Option<i64> {
        f.ledger_rows()
            .unwrap_or_else(|e| panic!("ledger inventory: {e}"))
            .into_iter()
            .find(|(d, _)| d == dr)
            .map(|(_, n)| n)
    };
    let keys_of = |name: &str| -> u64 {
        grovefs::fdb_store::list_subspaces(&uri)
            .unwrap_or_else(|e| panic!("inventory: {e}"))
            .into_iter()
            .find(|s| s.name == name)
            .map(|s| s.keys)
            .unwrap_or(0)
    };

    assert_eq!(row_of(&dr_a), Some(1), "create must book exactly one unit");
    let keys_before = keys_of(&view);
    assert!(keys_before >= 4, "a one-file tree should hold more than a handful of keys: {keys_before}");

    // The corpse, built exactly the way a partial `clearrange` of the `'n` family
    // builds one: the root row is gone, every other family is untouched.
    {
        let trx = f.db().create_trx().expect("create trx");
        trx.clear(&f.keys().name(grovefs::fdb_store::ROOT_PARENT, ""));
        // The crate's transaction wrapper blocks on `commit()` the same way.
        futures::executor::block_on(trx.commit()).expect("commit the root-row clear");
    }
    assert!(
        !grovefs::fdb_store::FdbStore::subspace_exists(&uri, &view).unwrap_or(true),
        "clearing the root row must make the subspace unmountable"
    );
    assert_eq!(
        keys_of(&view),
        keys_before - 1,
        "the damage must drop the root row and nothing else"
    );
    assert!(
        grovefs::fdb_store::FdbStore::subspace_has_keys(&uri, &view).unwrap_or(false),
        "the corpse should still hold keys"
    );

    // Both doors: plain import and `--replace`.
    for replace in [false, true] {
        let msg = match f.import_jsonl(&dump, replace) {
            Ok(st) => panic!("import (replace={replace}) onto a corpse was ACCEPTED: {st:?}"),
            Err(e) => e.to_string(),
        };
        assert!(
            msg.contains("corpse"),
            "the refusal must name the shape it found: {msg}"
        );
        assert!(
            msg.contains(&format!("--name {view} --base")),
            "the refusal must point at the exit that clears it: {msg}"
        );
    }
    assert_eq!(
        keys_of(&view),
        keys_before - 1,
        "a refused import wrote keys into the corpse"
    );
    assert_eq!(row_of(&dr_a), Some(1), "a refused import moved the old booking");
    assert_eq!(row_of(&dr_b), None, "a refused import booked the dump's ref");

    // The exit the refusal names, exercised rather than assumed: `view rm --base`
    // still works on a corpse (its teardown predicate is the `'r` family, not the
    // root row), and it is the only path that hands the old booking back.
    let rep = f
        .remove_view()
        .unwrap_or_else(|e| panic!("remove corpse view {view}: {e}"));
    assert_eq!(
        (rep.unbooked, rep.still_held),
        (1, 0),
        "removing the corpse should have released its one booking: {rep:?}"
    );
    assert_eq!(row_of(&dr_a), None, "the corpse's booking survived its removal");
    assert!(
        !grovefs::fdb_store::FdbStore::subspace_exists(&uri, &view).unwrap_or(true),
        "the corpse's subspace survived its removal"
    );
    println!(
        "fdb_import_corpse: refused with and without --replace, wrote nothing, `view rm --base` still unbooked it"
    );
}

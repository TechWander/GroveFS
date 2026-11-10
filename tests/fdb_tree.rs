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
        f.clear_view()
            .unwrap_or_else(|e| panic!("clear scratch view {view}: {e}"));
        assert!(
            fdb.dump_tree().is_empty(),
            "clear_view left rows behind in {view}"
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
                f.clear_view()
                    .unwrap_or_else(|e| panic!("clear scratch view {view}: {e}"));
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

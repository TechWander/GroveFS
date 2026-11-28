//! End-to-end check of the multi-meta-server VIEW SHARDING deployment
//! (ADR-0006): several `meta-server` processes, ONE PostgreSQL control plane
//! holding the view registry and `grovefs_control.meta_placement`, each process
//! serving the trees the placement table routes to it.
//!
//! What is proven here, through the real listeners and the real wire protocol:
//!   * a mount aimed at the WRONG instance is redirected and lands on the home
//!     instance — the entry point routes, it never proxies (the tree appears on
//!     one node's disk and not the other's, and only one node holds an engine);
//!   * `view create` / `view rm` refuse to act on a peer's view instead of
//!     creating a second tree behind one name;
//!   * `view move --to-meta` rewrites the placement row on the TARGET instance
//!     and the tree follows: an already-open mount keeps working and ends up
//!     talking to the new home with no remount (placement sweep + `E_SHARD`
//!     mid-op + client re-handshake);
//!   * the placement table is keyed by the PHYSICAL BASE view and is written
//!     only by a SUCCESSFUL mount — a lens follows its base, and a refused
//!     `M_OPEN` (unknown name, a lens whose base was never created) leaves no
//!     row behind (ADR-0006 §2, §5.6);
//!   * `store.info` reports the ring, per-node placed-view counts and which
//!     instance is answering.
//!
//! Needs a reachable PostgreSQL, because a placement table only one process can
//! read is that process's private opinion. `GROVEFS_PG_TEST_URI` names the
//! endpoint; without it 127.0.0.1:5432 and :5433 are probed. No server means the
//! file SELF-SKIPS (a suite that fails because a laptop has no PostgreSQL teaches
//! nothing). Every view and key name carries this process's id and the counts
//! asserted are about THOSE names only, so a shared database's other content
//! cannot tip a check either way.

use grovefs::apikeys::{ApiKeyStore, Scope};
use grovefs::control_remote::ControlClient;
use grovefs::meta::{MetaProvider, NodeKind, ROOT_INO};
use grovefs::remote::meta::{shard::Shard, MetaServer, RemoteMeta};
use serde_json::json;
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::time::{Duration, Instant};

const UID: u32 = 1000;
const GID: u32 = 1000;

/// A PostgreSQL this machine can reach, or `None` (→ self-skip).
fn pg_uri() -> Option<String> {
    if let Ok(u) = std::env::var("GROVEFS_PG_TEST_URI") {
        if !u.trim().is_empty() {
            return Some(u);
        }
    }
    for port in [5432u16, 5433] {
        let addr = format!("127.0.0.1:{port}").parse().ok()?;
        if TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok() {
            return Some(format!("postgres://grove:grovepw@127.0.0.1:{port}/grovefs"));
        }
    }
    None
}

/// Ask the kernel for a port nobody holds, then release it (the listener is
/// bound again by the server a moment later).
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("free port")
        .local_addr()
        .unwrap()
        .port()
}

/// One `meta-server` process's worth of state: its tree directory, the two
/// listeners it serves, and the admin credential its control port accepts.
struct Node {
    dir: tempfile::TempDir,
    meta: String,
    ctl: String,
    key: String,
    _srv: std::thread::JoinHandle<()>,
}

impl Node {
    fn gov(&self) -> ControlClient {
        ControlClient::new(&self.ctl, &self.key)
    }

    /// `store.info` as answered by THIS instance.
    fn info(&self) -> serde_json::Value {
        self.gov()
            .call("store.info", json!({}))
            .expect("store.info")
    }

    fn wait_up(&self) {
        for _ in 0..200 {
            let ok = |a: &str| {
                a.parse()
                    .ok()
                    .map(|ip| TcpStream::connect_timeout(&ip, Duration::from_millis(100)).is_ok())
                    .unwrap_or(false)
            };
            if ok(&self.meta) && ok(&self.ctl) {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("meta-server did not come up on {}", self.meta);
    }
}

/// Boot a sharded instance: `name` is its `--shard-id`, `ring` the whole
/// `--shard-ring` text (whose entry for `name` MUST be `meta` — a ring that
/// points a redirect at a port nobody holds fails the mount), `trees = None` for
/// SQLite under its own directory and `Some(uri)` for trees in shared PostgreSQL.
fn start(
    name: &str,
    ring: &str,
    control: &str,
    trees: Option<&str>,
    key: &str,
    meta: String,
    ctl: String,
) -> Node {
    let dir = tempfile::Builder::new()
        .prefix("grovefs_shard_")
        .tempdir()
        .unwrap();
    let mut srv = MetaServer::new(&meta, dir.path())
        // `0` turns idle eviction off: the ONLY thing that may drop a tree in
        // these tests is the placement sweep, which is what is being measured.
        .with_engine_idle_secs(0)
        .with_control_listen(Some(ctl.clone()))
        .with_sharding(Some(ring.to_string()), Some(name.to_string()), None);
    srv = match trees {
        // A PostgreSQL tree store IS its own control plane, so it must not be
        // handed a second one (`serve()` refuses that combination outright).
        Some(uri) => srv.with_store(Some(uri.to_string())),
        None => srv.with_control_plane(Some(control.to_string())),
    };
    let _srv = std::thread::spawn(move || {
        let _ = srv.serve();
    });
    let node = Node {
        dir,
        meta,
        ctl,
        key: key.to_string(),
        _srv,
    };
    node.wait_up();
    node
}

/// Does `base`'s SQLite tree set contain `view`? (The PG store writes nothing
/// locally, so this is how a test proves WHICH instance materialized a tree.)
fn has_local_tree(base: &Path, view: &str) -> bool {
    let mut stack = vec![base.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if e.file_name().to_string_lossy().contains(view) {
                return true;
            }
            if p.is_dir() {
                stack.push(p);
            }
        }
    }
    false
}

/// `store.info`'s shard block, with the row a node name carries.
fn shard_of(info: &serde_json::Value, node: &str) -> serde_json::Value {
    info["shard"]["nodes"]
        .as_array()
        .expect("shard nodes")
        .iter()
        .find(|n| n["name"] == json!(node))
        .cloned()
        .unwrap_or_else(|| panic!("no shard row for '{node}' in {info}"))
}

/// Mint one admin credential in the shared control plane for this run. Returns
/// the presentation string (`create_for` hands back the full `<id>:<secret>`, not
/// a bare secret) and the id, so teardown can delete the row.
fn admin_key(control: &str, base: &Path, label: &str) -> (String, String) {
    let store = ApiKeyStore::open_for(base, Some(control)).expect("open key store");
    let (id, full) = store
        .create_for(label, Scope::Admin)
        .expect("mint admin key");
    (full, id)
}

/// The placement table as THIS run sees it: every `(view, meta)` row whose view
/// name carries this process's tag.
///
/// Read straight from the shared control plane rather than off `store.info`'s
/// counts, because the ADR-0006 §2 invariant is about which NAMES are keys — a
/// count cannot tell a base row from a derived one, and the bug this catches is
/// precisely a derived row sitting beside its base and naming another instance.
fn placement_rows(pg: &str, ring: &str, me: &str, tag: &str) -> Vec<(String, String)> {
    let s = Shard::new(
        ring,
        Some(me),
        None,
        pg.to_string(),
        "sqlite (local)".to_string(),
    )
    .expect("shard handle for the placement read");
    s.placement_rows()
        .expect("read meta_placement")
        .into_iter()
        .filter(|(v, _)| v.starts_with(tag))
        .collect()
}

/// Remove the key this run minted, so a shared database is left as it was found.
fn cleanup(control: &str, base: &Path, key_id: &str) {
    if let Ok(store) = ApiKeyStore::open_for(base, Some(control)) {
        let _ = store.delete(key_id);
    }
}

#[test]
fn sqlite_trees_shard_across_two_instances() {
    let Some(pg) = pg_uri() else {
        eprintln!("skipping: no PostgreSQL reachable (set GROVEFS_PG_TEST_URI)");
        return;
    };
    let tag = format!("s{}a", std::process::id());
    // Node names carry the pid. `meta_placement` is one table in a shared
    // database, and a row written by an earlier run that died before its
    // teardown would be counted against a node that merely happens to share the
    // name — so a run that owns its names is the only way the counts below mean
    // what they say.
    let n_a = format!("{tag}m1");
    let n_b = format!("{tag}m2");
    let scratch = tempfile::Builder::new()
        .prefix("grovefs_shardkeys_")
        .tempdir()
        .unwrap();
    let (key, key_id) = admin_key(&pg, scratch.path(), &format!("shardtest-{tag}"));

    let m1p = free_port();
    let m2p = free_port();
    let ring = format!("{n_a}=127.0.0.1:{m1p},{n_b}=127.0.0.1:{m2p}");
    let m1 = start(
        &n_a,
        &ring,
        &pg,
        None,
        &key,
        format!("127.0.0.1:{m1p}"),
        format!("127.0.0.1:{}", free_port()),
    );
    let m2 = start(
        &n_b,
        &ring,
        &pg,
        None,
        &key,
        format!("127.0.0.1:{m2p}"),
        format!("127.0.0.1:{}", free_port()),
    );

    let va = format!("{tag}va");
    let vb = format!("{tag}vb");

    // ---- create lands on the instance that was asked, and records it --------
    m1.gov()
        .call("view.create", json!({"view": va}))
        .expect("create va on m1");
    m2.gov()
        .call("view.create", json!({"view": vb}))
        .expect("create vb on m2");

    // A peer asked to create the SAME name refuses instead of building a second
    // tree behind it, and names the node whose disk holds it (the mount address,
    // NOT a governance port — the ring stores where clients dial, and inventing
    // `mount port + 1` here would be a pasteable command that cannot be reached).
    let e = m2
        .gov()
        .call("view.create", json!({"view": va}))
        .expect_err("m2 must not create a view m1 holds");
    assert!(
        e.contains(&format!("placed on meta-server '{n_a}'")) && e.contains(&m1.meta),
        "unhelpful refusal: {e}"
    );
    // The regression this guards: the ring stores the MOUNT address, and an
    // earlier wording pasted that one into `--control-server`, sending the
    // operator to a port that does not speak the governance protocol.
    assert!(
        !e.contains(&format!("--control-server {}", m1.meta)),
        "refusal dials the mount port as a control port: {e}"
    );

    // ---- the mount is routed, not proxied ----------------------------------
    // Two timings taken back to back on the SAME instance, so they compare: the
    // view this process owns, then the one it has to hand off.
    let t_home = Instant::now();
    let _mh = RemoteMeta::new(&m2.meta, &vb).expect("mount vb at its own home");
    let home_took = t_home.elapsed();
    // Snapshot m2's engine cache NOW, before the redirect: what changes from here
    // on is attributable to the redirect alone.
    let pre2 = m2.info();
    // Dialed at the WRONG instance, and it still opens: the redirect is answered
    // inside the connect walk.
    let t0 = Instant::now();
    let m = RemoteMeta::new(&m2.meta, &va).expect("mount va through m2");
    // Printed, not asserted: ADR-0006 quotes the real cost of a mount that has
    // to be redirected (one placement read + one extra handshake).
    eprintln!(
        "timings: mount at its own home {home_took:?}; mount dialed at the wrong instance {:?} \
         (placement read + one extra handshake)",
        t0.elapsed()
    );
    let ino = m
        .create_node(
            ROOT_INO,
            "from-elsewhere",
            false,
            Some("k"),
            0o0644,
            UID,
            GID,
        )
        .expect("write through the redirected mount");
    assert_eq!(
        m.getattr(ino).map(|n| n.kind),
        Some(NodeKind::Regular),
        "the mount that was redirected did not serve the tree"
    );
    // The tree lives on m1 alone: m2 never materialized one, and m2 holds no
    // engine for it either — routing is a per-connection decision, so the
    // steady state costs m2 nothing.
    assert!(has_local_tree(m1.dir.path(), &va), "m1 must hold va's tree");
    assert!(
        !has_local_tree(m2.dir.path(), &va),
        "m2 served a redirect it should only have ANSWERED"
    );
    let i1 = m1.info();
    let i2 = m2.info();
    assert_eq!(i1["shard"]["self"], json!(n_a));
    assert_eq!(i2["shard"]["self"], json!(n_b));
    assert_eq!(
        i1["shard"]["controlPlane"],
        json!("separate --control-plane")
    );
    assert_eq!(shard_of(&i1, &n_a)["views"], json!(1), "va on m1");
    assert_eq!(shard_of(&i1, &n_b)["views"], json!(1), "vb on m2");
    assert_eq!(shard_of(&i1, &n_a)["here"], json!(true));
    assert_eq!(i1["views"], json!(1), "m1 holds exactly va's tree");
    assert_eq!(i2["views"], json!(1), "m2 holds only its own vb");
    // The instance that only ANSWERED a redirect cached nothing: routing is a
    // per-connection decision, so a foreign view costs the peer no engine.
    assert_eq!(
        i2["residentEngines"], pre2["residentEngines"],
        "m2 cached a view it only redirected: {i2}"
    );
    assert!(
        i1["residentEngines"].as_u64().unwrap_or(0) >= 1,
        "the home instance must hold the engine the mount is using: {i1}"
    );

    // ---- destroy follows the same rule -------------------------------------
    let e = m2
        .gov()
        .call("view.rm_base", json!({"view": va}))
        .expect_err("m2 must not delete a view m1 holds");
    assert!(
        e.contains(&format!("placed on meta-server '{n_a}'")),
        "unhelpful refusal: {e}"
    );
    m1.gov()
        .call("view.rm_base", json!({"view": va}))
        .expect("the home instance deletes its own view");
    // The row went with the tree, so the counts move too — this is what makes
    // `store.info` a live picture instead of a list of what was once created.
    assert_eq!(shard_of(&m1.info(), &n_a)["views"], json!(0));
    assert_eq!(shard_of(&m2.info(), &n_b)["views"], json!(1));

    // ---- cross-instance move is refused unless the tree is reachable -------
    // vb's tree is m2's local SQLite file: m1 cannot adopt a name whose bytes it
    // does not have, and saying so beats creating an empty schema under the name.
    let e = m1
        .gov()
        .call("view.move_meta", json!({"view": vb, "to": n_a.clone()}))
        .expect_err("a SQLite tree needs export/import first");
    assert!(
        e.contains("view export") || e.contains("--meta-store"),
        "unhelpful refusal: {e}"
    );
    // Asking the instance that already holds it is a no-op, not an error.
    m2.gov()
        .call("view.move_meta", json!({"view": vb, "to": n_b.clone()}))
        .expect("moving to the current home is idempotent");
    // A `to` that names someone else is refused by the process that got it.
    let e = m2
        .gov()
        .call("view.move_meta", json!({"view": vb, "to": n_a.clone()}))
        .expect_err("m2 must not answer for m1");
    assert!(e.contains(&n_a), "unhelpful refusal: {e}");

    m2.gov()
        .call("view.rm_base", json!({"view": vb}))
        .expect("teardown vb");
    cleanup(&pg, scratch.path(), &key_id);
}

#[test]
fn a_moved_view_follows_its_open_mount() {
    let Some(pg) = pg_uri() else {
        eprintln!("skipping: no PostgreSQL reachable (set GROVEFS_PG_TEST_URI)");
        return;
    };
    let tag = format!("s{}b", std::process::id());
    // Unique node names, for the reason spelled out in the test above.
    let n_a = format!("{tag}p1");
    let n_b = format!("{tag}p2");
    let scratch = tempfile::Builder::new()
        .prefix("grovefs_shardkeys_")
        .tempdir()
        .unwrap();
    let (key, key_id) = admin_key(&pg, scratch.path(), &format!("shardtest-{tag}"));

    // Both instances put their TREES in the shared database, which is what makes
    // a move legal at all: the bytes are reachable from the target, so no export
    // step and no second copy is involved.
    let m1p = free_port();
    let m2p = free_port();
    let ring = format!("{n_a}=127.0.0.1:{m1p},{n_b}=127.0.0.1:{m2p}");
    let n1 = start(
        &n_a,
        &ring,
        &pg,
        Some(&pg),
        &key,
        format!("127.0.0.1:{m1p}"),
        format!("127.0.0.1:{}", free_port()),
    );
    let n2 = start(
        &n_b,
        &ring,
        &pg,
        Some(&pg),
        &key,
        format!("127.0.0.1:{m2p}"),
        format!("127.0.0.1:{}", free_port()),
    );
    let v = format!("{tag}mv");
    n1.gov()
        .call("view.create", json!({"view": v}))
        .expect("create on n1");
    assert_eq!(shard_of(&n1.info(), &n_a)["views"], json!(1));
    assert_eq!(n1.info()["shard"]["controlPlane"], json!("tree backend"));

    // An open mount on the home instance, with a directory in it.
    let m = RemoteMeta::new(&n1.meta, &v).expect("mount on the home instance");
    m.create_node(ROOT_INO, "before", true, None, 0o0755, UID, GID)
        .expect("mkdir before the move");

    // Move it, asked at the TARGET — the only process that can promise the tree
    // is reachable where it claims to be.
    let r = n2
        .gov()
        .call("view.move_meta", json!({"view": v, "to": n_b.clone()}))
        .expect("adopt the view on n2");
    // Measured from the moment the row flipped, not from the moment the test
    // notices: this is the window a client is talking to the wrong instance in.
    let t_move = Instant::now();
    assert!(
        r.to_string().contains(&n_b),
        "the move reply should name the new home: {r}"
    );
    assert_eq!(shard_of(&n2.info(), &n_b)["views"], json!(1));
    assert_eq!(shard_of(&n2.info(), &n_a)["views"], json!(0));

    // The old instance reads the table on its sweep tick and drops the tree.
    // Until it does, ops on the open mount are STILL served locally — so "did
    // the mount follow?" is only a meaningful question once the engine cache is
    // empty, and this instance holds nothing but `v`, so its count going to zero
    // is a per-view proof. Then the mount's next op gets `E_SHARD` + hang-up,
    // re-handshakes to the new home, and lands there. Nothing re-opens it by
    // hand.
    let deadline = Instant::now() + Duration::from_secs(20);
    while n1.info()["residentEngines"].as_u64() != Some(0) {
        if Instant::now() >= deadline {
            panic!("the old instance never released the view: {}", n1.info());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let t_release = t_move.elapsed();
    let t_op = Instant::now();
    let ino = m.create_node(ROOT_INO, "after", false, Some("k2"), 0o0644, UID, GID);
    assert_ne!(ino.expect("the mount never followed the view"), 0);
    eprintln!(
        "timings: sweep released the view {:?} after the row flipped; the first op on \
         the still-open mount followed it in {:?}",
        t_release,
        t_op.elapsed()
    );
    // The new home is the one holding the engine now, and the old one holds none.
    assert!(
        n2.info()["residentEngines"].as_u64().unwrap_or(0) >= 1,
        "the target never took the tree over"
    );
    assert_eq!(
        n1.info()["residentEngines"].as_u64(),
        Some(0),
        "the old instance served the view again after releasing it"
    );
    // One tree, seen from both instances: the file created BEFORE the move is
    // still there next to the one created AFTER it migrated.
    let names: Vec<String> = m
        .readdir(ROOT_INO)
        .into_iter()
        .map(|n| n.name)
        .filter(|n| n != ".")
        .collect();
    assert!(
        names.iter().any(|n| n == "before") && names.iter().any(|n| n == "after"),
        "listing after the move lost a directory: {names:?}"
    );
    // And the home instance is now the one holding the engine.
    assert_eq!(shard_of(&n1.info(), &n_b)["views"], json!(1));

    n2.gov()
        .call("view.rm_base", json!({"view": v}))
        .expect("teardown the moved view");
    cleanup(&pg, scratch.path(), &key_id);
}

/// A derived view is a PRESENTATION over one base tree, so it lives where that
/// tree lives (ADR-0006 §2: "the placement row's key is the physical base view
/// name, after alias resolution and before lens/fork expansion"; §5.6: "derived
/// views travel with their base").
///
/// Routing the VISIBLE name instead put a second row beside the base's, and the
/// two could name different instances — after which the lens mounts nowhere:
/// the wrong instance redirected it to the base's home, which refused it over a
/// tree it had never materialized, and no governance verb could remove the row,
/// because `def.remove` keys off the definition and `view rm_base` off the base.
/// A lens whose base was never created is the worst case: the failed mount wrote
/// the row itself, then every later attempt was routed by it.
#[test]
fn a_lens_routes_by_its_base_and_owns_no_placement_row() {
    let Some(pg) = pg_uri() else {
        eprintln!("skipping: no PostgreSQL reachable (set GROVEFS_PG_TEST_URI)");
        return;
    };
    let tag = format!("s{}c", std::process::id());
    let n_a = format!("{tag}r1");
    let n_b = format!("{tag}r2");
    let scratch = tempfile::Builder::new()
        .prefix("grovefs_shardkeys_")
        .tempdir()
        .unwrap();
    let (key, key_id) = admin_key(&pg, scratch.path(), &format!("shardtest-{tag}"));

    let m1p = free_port();
    let m2p = free_port();
    let ring = format!("{n_a}=127.0.0.1:{m1p},{n_b}=127.0.0.1:{m2p}");
    let m1 = start(
        &n_a,
        &ring,
        &pg,
        None,
        &key,
        format!("127.0.0.1:{m1p}"),
        format!("127.0.0.1:{}", free_port()),
    );
    let m2 = start(
        &n_b,
        &ring,
        &pg,
        None,
        &key,
        format!("127.0.0.1:{m2p}"),
        format!("127.0.0.1:{}", free_port()),
    );

    // A base on m1, and a lens over it whose NAME would hash to m2 if the
    // visible name were ever the sharding key.
    let base = format!("{tag}rbase");
    let lens = format!("lens_{tag}rlens");
    m1.gov()
        .call("view.create", json!({"view": base}))
        .expect("create the base on m1");
    let mb = RemoteMeta::new(&m1.meta, &base).expect("mount the base at home");
    mb.create_node(ROOT_INO, "kept", true, None, 0o0755, UID, GID)
        .expect("mkdir in the base");
    m1.gov()
        .call(
            "def.put",
            json!({"name": lens, "text": format!("base {base}\nhide /gone\n")}),
        )
        .expect("publish the lens");

    // The lens mounts at BOTH instances: at its base's home directly, at the
    // peer through the redirect the BASE's row answers (that is the whole point
    // of §5.6 — a derived name never needs a row of its own).
    let _l1 = RemoteMeta::new(&m1.meta, &lens).expect("the lens must mount at its base's home");
    let l2 = RemoteMeta::new(&m2.meta, &lens).expect("the lens must follow its base to m1");
    let names: Vec<String> = l2
        .readdir(ROOT_INO)
        .into_iter()
        .map(|n| n.name)
        .filter(|n| n != ".")
        .collect();
    assert!(
        names.iter().any(|n| n == "kept"),
        "the lens mounted through the peer did not show its base's tree: {names:?}"
    );

    // The table's shape: ONE row for this run, keyed by the base, pointing at
    // the instance that holds the tree. No derived key, no duplicate.
    let rows = placement_rows(&pg, &ring, &n_a, &tag);
    assert!(
        !rows.iter().any(|(v, _)| v == &lens),
        "a derived name took a placement row of its own: {rows:?}"
    );
    assert_eq!(
        rows.as_slice(),
        [(base.clone(), n_a.clone())].as_slice(),
        "the placement table must hold the base, once, on its home"
    );

    // A lens whose BASE WAS NEVER CREATED is the shape that used to poison the
    // name: the mount fails, and it must fail without leaving a row.
    let ghost = format!("{tag}rghost");
    let ghost_lens = format!("lens_{tag}rghost");
    m1.gov()
        .call(
            "def.put",
            json!({"name": ghost_lens, "text": format!("base {ghost}\nhide /gone\n")}),
        )
        .expect("publish a lens over a base that does not exist");
    for port in [m1p, m2p] {
        let e = RemoteMeta::new(&format!("127.0.0.1:{port}"), &ghost_lens)
            .err()
            .unwrap_or_else(|| panic!("a lens over a missing base mounted at {port}"));
        assert!(
            e.to_string().contains("has no physical tree"),
            "unhelpful refusal: {e}"
        );
    }
    let rows = placement_rows(&pg, &ring, &n_a, &tag);
    assert!(
        !rows.iter().any(|(v, _)| v.starts_with("lens_")),
        "a failed mount wrote a placement row: {rows:?}"
    );
    assert!(
        !rows.iter().any(|(v, _)| v == &ghost),
        "the missing base was placed by a mount it never served: {rows:?}"
    );

    // Teardown, and the run's rows go with it — nothing here outlives the tree.
    m1.gov()
        .call("def.remove", json!({"name": lens}))
        .expect("drop the lens");
    m1.gov()
        .call("def.remove", json!({"name": ghost_lens}))
        .expect("drop the ghost lens");
    m1.gov()
        .call("view.rm_base", json!({"view": base}))
        .expect("teardown the base");
    assert!(
        placement_rows(&pg, &ring, &n_a, &tag).is_empty(),
        "placement rows outlived the tree they named"
    );
    cleanup(&pg, scratch.path(), &key_id);
}

/// An `M_OPEN` that is REFUSED must leave nothing in the shared control plane.
///
/// The gate used to bootstrap the placement row BEFORE it knew the mount would
/// work, so a single unauthenticated socket probe with a junk name was enough
/// to grow `grovefs_control.meta_placement` without bound — and worse, to give
/// a name that does not exist a home, which every later mount then obeys.
#[test]
fn refused_mounts_leave_no_placement_row() {
    let Some(pg) = pg_uri() else {
        eprintln!("skipping: no PostgreSQL reachable (set GROVEFS_PG_TEST_URI)");
        return;
    };
    let tag = format!("s{}d", std::process::id());
    let n_a = format!("{tag}q1");
    let n_b = format!("{tag}q2");
    let scratch = tempfile::Builder::new()
        .prefix("grovefs_shardkeys_")
        .tempdir()
        .unwrap();
    let (key, key_id) = admin_key(&pg, scratch.path(), &format!("shardtest-{tag}"));

    let m1p = free_port();
    let m2p = free_port();
    let ring = format!("{n_a}=127.0.0.1:{m1p},{n_b}=127.0.0.1:{m2p}");
    let m1 = start(
        &n_a,
        &ring,
        &pg,
        None,
        &key,
        format!("127.0.0.1:{m1p}"),
        format!("127.0.0.1:{}", free_port()),
    );
    let _m2 = start(
        &n_b,
        &ring,
        &pg,
        None,
        &key,
        format!("127.0.0.1:{m2p}"),
        format!("127.0.0.1:{}", free_port()),
    );

    // Nothing created, nothing mounted: every one of these is refused by the
    // explicit-creation gate, which now runs BEFORE any row can be claimed.
    // `..` is not a legal view name at all, the rest are simply absent.
    for name in [format!("{tag}never"), format!("{tag}{name}", name = "x".repeat(40))] {
        let e = RemoteMeta::new(&format!("127.0.0.1:{m1p}"), &name)
            .err()
            .unwrap_or_else(|| panic!("'{name}' mounted although it does not exist"));
        assert!(
            e.to_string().contains("does not exist"),
            "unhelpful refusal for '{name}': {e}"
        );
    }
    assert!(
        placement_rows(&pg, &ring, &n_a, &tag).is_empty(),
        "refused mounts grew the placement table"
    );

    // The flip side, so the check above cannot be satisfied by a broken claim:
    // a mount that SUCCEEDS records exactly one row, keyed by the name it was
    // claimed under, on the instance that answered.
    let v = format!("{tag}ok");
    m1.gov()
        .call("view.create", json!({"view": v}))
        .expect("create");
    let _m = RemoteMeta::new(&format!("127.0.0.1:{m2p}"), &v).expect("mount through the peer");
    let rows = placement_rows(&pg, &ring, &n_a, &tag);
    assert_eq!(
        rows,
        vec![(v.clone(), n_a.clone())],
        "a successful mount must leave exactly its base's row"
    );

    m1.gov()
        .call("view.rm_base", json!({"view": v}))
        .expect("teardown");
    cleanup(&pg, scratch.path(), &key_id);
}

// TODO(severity=High): fork definitions (`ViewDef::Fork(base)`) take the same
// routing key as a lens via `shard_routing_key`, but no case here exercises one:
// `fork.create` needs a seeded base tree, and the SQLite/PG split changes what a
// fork's own metadata tree is — the lens case above covers the fold, this covers
// the second spelling of it.  TODO(severity=Medium): a PostgreSQL connection-pool
// exhaustion case (mount storm against `--pg-*-pool-max`, and the shard path's
// own `Shard::connect` which bypasses that pool) needs a PG fixture that can be
// driven to `max_connections` and a knob to assert on; nothing in this file can
// say "the control plane was too slow" apart from "the placement table was empty".
// TODO(severity=Medium): a network-partition case (peer reachable at the TCP
// layer but refusing, or half-open) belongs to the HA suite, not to sharding.

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
//!   * the invalidation subscription is a THIRD `M_OPEN` for the same view and
//!     follows the same redirect, so a mount whose coherence stream was dialed
//!     at the wrong instance still receives the home instance's `EV_ENTRY`
//!     frames for a peer's writes (ADR-0006 §5);
//!   * a THREE-node ring routes by the row, not by adjacency: a redirect lands
//!     on the instance the row names even when that is neither the entry point
//!     nor its neighbour, and a row naming a node the ring does not define
//!     fails the mount everywhere with that name instead of falling back to
//!     serving the tree locally;
//!   * `store.info` reports the ring, the physical base trees EACH node's
//!     placement rows fold to, the raw row tally next to them, the row's own
//!     `updated_at`, the configured sweep and the trees an instance still holds
//!     while the table names a peer — which is the one thing a per-node count
//!     could not say while a move was converging.
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
use grovefs::remote::proto;
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
    start_swept(name, ring, control, trees, key, meta, ctl, None)
}

/// `sweep_secs` overrides `--shard-sweep-secs` (`None` = the built-in 5s), so a
/// test can MEASURE the window the operator is configuring instead of waiting
/// out the default on every run.
#[allow(clippy::too_many_arguments)]
fn start_swept(
    name: &str,
    ring: &str,
    control: &str,
    trees: Option<&str>,
    key: &str,
    meta: String,
    ctl: String,
    sweep_secs: Option<u64>,
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
        .with_sharding(Some(ring.to_string()), Some(name.to_string()), None)
        .with_shard_sweep_secs(sweep_secs);
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

/// The sweep interval is a knob, not a constant. A deployment that would rather
/// close the window in which BOTH instances still hold a moved tree asks for a
/// shorter one (here through the same setter `--shard-sweep-secs` calls) and
/// gets a proportionally shorter window — measured against the default's ~4.7s
/// so the two cannot be confused, and with the SAME end state: the open mount
/// still follows the row.
#[test]
fn a_shorter_sweep_shortens_the_window_a_deposed_instance_holds_the_tree() {
    let Some(pg) = pg_uri() else {
        eprintln!("skipping: no PostgreSQL reachable (set GROVEFS_PG_TEST_URI)");
        return;
    };
    let tag = format!("s{}sw", std::process::id());
    let n_a = format!("{tag}p1");
    let n_b = format!("{tag}p2");
    let scratch = tempfile::Builder::new()
        .prefix("grovefs_shardkeys_")
        .tempdir()
        .unwrap();
    let (key, key_id) = admin_key(&pg, scratch.path(), &format!("shardtest-{tag}"));
    let m1p = free_port();
    let m2p = free_port();
    let ring = format!("{n_a}=127.0.0.1:{m1p},{n_b}=127.0.0.1:{m2p}");
    // ONE SECOND, set the way the flag sets it.
    let n1 = start_swept(
        &n_a,
        &ring,
        &pg,
        Some(&pg),
        &key,
        format!("127.0.0.1:{m1p}"),
        format!("127.0.0.1:{}", free_port()),
        Some(1),
    );
    let n2 = start_swept(
        &n_b,
        &ring,
        &pg,
        Some(&pg),
        &key,
        format!("127.0.0.1:{m2p}"),
        format!("127.0.0.1:{}", free_port()),
        Some(1),
    );
    let v = format!("{tag}sv");
    n1.gov()
        .call("view.create", json!({"view": v}))
        .expect("create on n1");
    let m = RemoteMeta::new(&n1.meta, &v).expect("mount on the home instance");
    m.create_node(ROOT_INO, "before", true, None, 0o0755, UID, GID)
        .expect("mkdir before the move");

    let r = n2
        .gov()
        .call("view.move_meta", json!({"view": v, "to": n_b.clone()}))
        .expect("adopt the view on n2");
    let t_move = Instant::now();
    // The reply quotes the CONFIGURED window, not a hard-coded one — this is the
    // text an operator reads before deciding whether to wait.
    assert!(
        r.to_string().contains("1s"),
        "the move reply did not name the configured sweep: {r}"
    );

    let deadline = Instant::now() + Duration::from_secs(20);
    while n1.info()["residentEngines"].as_u64() != Some(0) {
        if Instant::now() >= deadline {
            panic!("the old instance never released the view: {}", n1.info());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let t_release = t_move.elapsed();
    eprintln!(
        "timings: a 1s sweep released the view {t_release:?} after the row flipped (the 5s \
         default measures ~4.7s in the test above)"
    );
    assert!(
        t_release < Duration::from_secs(3),
        "a 1s sweep only released the tree after {t_release:?}: the configured interval was ignored"
    );
    // Same outcome as the default interval: the mount follows to the new home.
    let ino = m.create_node(ROOT_INO, "after", false, Some("k2"), 0o0644, UID, GID);
    assert_ne!(ino.expect("the mount never followed the view"), 0);
    assert_eq!(shard_of(&n1.info(), &n_b)["views"], json!(1));

    n2.gov()
        .call("view.rm_base", json!({"view": v}))
        .expect("teardown the moved view");
    cleanup(&pg, scratch.path(), &key_id);
}

/// The shard block's arithmetic, against a REAL placement table. `views` counts
/// physical base trees and `rows` counts the rows behind them, `lastUpdatedAt`
/// is the table's own text, `sweepMs` is the window that was configured and
/// `homeMismatch` is the split-write window made visible instead of being
/// something an operator reads out of a restarted process's log.
#[test]
fn store_info_reports_the_placement_rows_behind_its_view_count() {
    let Some(pg) = pg_uri() else {
        eprintln!("skipping: no PostgreSQL reachable (set GROVEFS_PG_TEST_URI)");
        return;
    };
    let tag = format!("s{}si", std::process::id());
    let n_a = format!("{tag}p1");
    let n_b = format!("{tag}p2");
    let scratch = tempfile::Builder::new()
        .prefix("grovefs_shardkeys_")
        .tempdir()
        .unwrap();
    let (key, key_id) = admin_key(&pg, scratch.path(), &format!("shardtest-{tag}"));
    let m1p = free_port();
    let m2p = free_port();
    let ring = format!("{n_a}=127.0.0.1:{m1p},{n_b}=127.0.0.1:{m2p}");
    // A sweep that CANNOT run inside the test: the window being described has to
    // still be open when the page is read, and `0` is refused at startup because
    // a table that is never re-read is a frozen placement.
    let n1 = start_swept(
        &n_a,
        &ring,
        &pg,
        Some(&pg),
        &key,
        format!("127.0.0.1:{m1p}"),
        format!("127.0.0.1:{}", free_port()),
        Some(600),
    );
    let n2 = start_swept(
        &n_b,
        &ring,
        &pg,
        Some(&pg),
        &key,
        format!("127.0.0.1:{m2p}"),
        format!("127.0.0.1:{}", free_port()),
        Some(600),
    );
    let v = format!("{tag}iv");
    n1.gov()
        .call("view.create", json!({"view": v}))
        .expect("create on n1");
    let _m = RemoteMeta::new(&n1.meta, &v).expect("mount on the home instance");

    let i = n1.info();
    assert_eq!(i["shard"]["self"], json!(n_a));
    assert_eq!(
        i["shard"]["sweepMs"],
        json!(600_000),
        "the reported window must be the configured one, not the built-in default"
    );
    let home = shard_of(&i, &n_a);
    assert_eq!(home["views"], json!(1));
    assert_eq!(home["rows"], json!(1), "one base tree, one row");
    assert!(home["here"].as_bool().unwrap());
    // The stamp travels as the table writes it, so it must parse as the RFC3339
    // text `Shard` puts there — an operator greps this against a peer's log.
    let at = home["lastUpdatedAt"]
        .as_str()
        .expect("a placed view carries an updated_at");
    assert!(
        chrono::DateTime::parse_from_rfc3339(at).is_ok(),
        "unparsable updated_at: {at}"
    );
    assert!(i["shard"]["homeMismatch"].as_array().unwrap().is_empty());

    // Move it, asked at the TARGET, and leave the row as the only message: with
    // a 600s sweep nothing else tells the old instance it was deposed.
    n2.gov()
        .call("view.move_meta", json!({"view": v, "to": n_b.clone()}))
        .expect("adopt the view on n2");
    let i = n1.info();
    assert_eq!(
        shard_of(&i, &n_a)["views"],
        json!(0),
        "the row no longer names this instance"
    );
    assert_eq!(shard_of(&i, &n_b)["views"], json!(1));
    assert_eq!(shard_of(&i, &n_b)["rows"], json!(1));
    // THE WINDOW, MADE VISIBLE: the table says `n_b`, this process still holds
    // the engine. A nonzero `residentEngines` beside a zero `views` is exactly
    // what `evict_misplaced` is waiting to close.
    assert!(
        i["shard"]["homeMismatch"]
            .as_array()
            .unwrap()
            .contains(&json!(v.clone())),
        "the deposed instance reported nothing about the tree it still holds: {i}"
    );
    assert!(
        n1.info()["residentEngines"].as_u64().unwrap_or(0) >= 1,
        "the tree was dropped although the sweep is set to 600s"
    );
    // The NEW home never reports its own tree as misplaced.
    assert!(n2.info()["shard"]["homeMismatch"]
        .as_array()
        .unwrap()
        .iter()
        .all(|k| k != &json!(v)));

    // The table is shared with every other run against this database, so an
    // absolute total means nothing here — but the block's own arithmetic holds
    // for ANY content: one query, and every row either belongs to a ring node
    // or is named as an orphan.
    let sh = n2.info()["shard"].clone();
    let summed: u64 = sh["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["rows"].as_u64().unwrap())
        .sum();
    assert_eq!(
        sh["placementRows"].as_u64().unwrap(),
        summed + sh["orphanViews"].as_array().unwrap().len() as u64,
        "a placement row counted in no node and named in no orphan list: {sh}"
    );

    n2.gov()
        .call("view.rm_base", json!({"view": v}))
        .expect("teardown the moved view");
    cleanup(&pg, scratch.path(), &key_id);
}

/// `0` is REFUSED, not honored. `--engine-idle-secs 0` can mean "never" because
/// an idle engine costs nothing to keep; a placement table that is never
/// re-read is a deposed instance going on writing a tree whose row now names a
/// peer — a split write, which is the one thing sharding exists to prevent. The
/// check sits where every other ring mistake is caught (control plane missing,
/// `--shard-id` not in the ring): before a single client is served, so the
/// process exits instead of running with placement frozen.
#[test]
fn a_zero_sweep_fails_the_start_instead_of_freezing_placement() {
    let Some(pg) = pg_uri() else {
        eprintln!("skipping: no PostgreSQL reachable (set GROVEFS_PG_TEST_URI)");
        return;
    };
    let tag = format!("s{}z0", std::process::id());
    let n_a = format!("{tag}p1");
    let m1p = free_port();
    let dir = tempfile::Builder::new()
        .prefix("grovefs_shard_")
        .tempdir()
        .unwrap();
    let err = MetaServer::new(&format!("127.0.0.1:{m1p}"), dir.path())
        .with_sharding(Some(format!("{n_a}=127.0.0.1:{m1p}")), Some(n_a.clone()), None)
        .with_control_plane(Some(pg))
        .with_shard_sweep_secs(Some(0))
        .serve()
        .expect_err("a 0 sweep must fail the start, not serve");
    let why = err.to_string();
    eprintln!("refused at start-up: {why}");
    assert!(why.contains("--shard-sweep-secs 0"), "wrong refusal: {why}");
    assert!(
        why.contains("split write"),
        "the refusal has to name what never sweeping costs: {why}"
    );
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

/// The invalidation subscription is a THIRD `M_OPEN` for the same view, and it
/// must follow the same redirect as the work socket.
///
/// The defect this pins: the mount's listener used to walk a comma-separated
/// failover list and take the first socket that answered, ignoring `E_SHARD`.
/// A deployment whose entry point is not the home instance then gets a healthy
/// work mount (redirected) and a coherence stream parked on the WRONG instance,
/// which serves the subscription from a tree it does not own — so peer writes
/// never invalidate, while every read still returns the right bytes. Nothing in
/// the log says so, and the suite a sharding test can reach is the only place
/// this is observable at all: the frames below only arrive if the subscription
/// was established on the instance that holds the engine the write went through.
#[test]
fn the_invalidation_stream_of_a_redirected_mount_comes_from_the_home_instance() {
    let Some(pg) = pg_uri() else {
        eprintln!("skipping: no PostgreSQL reachable (set GROVEFS_PG_TEST_URI)");
        return;
    };
    let tag = format!("s{}e", std::process::id());
    let n_a = format!("{tag}e1");
    let n_b = format!("{tag}e2");
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
    let v = format!("{tag}ev");
    m1.gov()
        .call("view.create", json!({"view": v}))
        .expect("create on m1");

    // The work socket, opened where the tree actually is.
    let work = RemoteMeta::new(&m1.meta, &v).expect("mount at home");

    // The subscription, dialed at the instance that does NOT serve it: the
    // address the mount's listener thread is handed by a deployment that uses
    // one bootstrap address for every client. Retried inside the budget only
    // because the two sockets race over this test's own process; the FIRST
    // successful dial is the one whose answer is asserted — once m2's row
    // points at m1, every later attempt takes the same redirect.
    let mut sub = None;
    let t_open = Instant::now();
    while t_open.elapsed() < Duration::from_secs(10) {
        match grovefs::remote::meta::open_event_stream(&m2.meta, &v, "", 0) {
            Ok(s) => {
                sub = Some(s);
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                panic!("the subscription was refused on credentials: {e}")
            }
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    let mut sub = sub.expect("the event stream never followed the redirect");
    let followed = t_open.elapsed();
    // The proof that the frames can only have come from m1: this instance never
    // took the tree over (no local SQLite file, no cached engine), so a
    // subscription that had stayed here would have been accepted against a view
    // it does not serve and then had nothing to push.
    assert!(
        !has_local_tree(m2.dir.path(), &v),
        "the subscription made the peer materialize the tree it redirected"
    );
    assert_eq!(
        m2.info()["residentEngines"].as_u64(),
        Some(0),
        "m2 holds an engine for a view it only routes: {}",
        m2.info()
    );
    assert!(
        m1.info()["residentEngines"].as_u64().unwrap_or(0) >= 1,
        "m1 must be the instance holding the tree both sockets talk to"
    );

    // A missing push must surface as an assertion, not as a stalled suite —
    // the listener's socket deliberately has no read timeout in production.
    sub.set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");

    // A peer write through the home instance: the subscriber must be told.
    let dir = work
        .create_node(ROOT_INO, "docs", true, None, 0o755, UID, GID)
        .expect("mkdir on the home instance");
    let (kind, val, data) = proto::recv_response(&mut sub).expect("EV_ENTRY after a peer mkdir");
    assert_eq!(kind, 0, "EV_ENTRY expected, got {kind}");
    assert_eq!(val as i64, ROOT_INO, "the event's parent is the root");
    let mut i = 0usize;
    assert_eq!(proto::get_key(&data, &mut i).unwrap(), "docs");

    // And the stream is not a one-shot: a second write on the same subscription
    // is delivered against the inode the first one created.
    work
        .create_node(dir, "a.txt", false, Some("x"), 0o0644, UID, GID)
        .expect("file under the new dir");
    let (kind2, _, data2) = proto::recv_response(&mut sub).expect("second EV_ENTRY");
    assert_eq!(kind2, 0);
    let mut i2 = 0usize;
    assert_eq!(proto::get_key(&data2, &mut i2).unwrap(), "a.txt");
    eprintln!("timings: the subscription took {followed:?} to land on the home instance");

    m1.gov()
        .call("view.rm_base", json!({"view": v}))
        .expect("teardown");
    cleanup(&pg, scratch.path(), &key_id);
}

/// A ring of THREE instances routes by the placement row, not by proximity —
/// and a row that names a node the ring does not define is refused instead of
/// quietly served by whoever got the mount.
///
/// Two instances can only ever answer "the row's node is the other one", so the
/// redirect target and the fallback neighbour are the same process and a bug
/// that resolved a row to "the peer" rather than "the named node" was invisible.
/// With three, the entry point m2 has to hand a mount to m1 (its row) for `va`
/// and to m3 for `vb`, and the retired-node case below is the one a real
/// deployment hits: the operator removes a host from `--shard-ring` while its
/// rows are still in the shared table.
#[test]
fn a_three_node_ring_routes_by_the_row_not_by_adjacency() {
    let Some(pg) = pg_uri() else {
        eprintln!("skipping: no PostgreSQL reachable (set GROVEFS_PG_TEST_URI)");
        return;
    };
    let tag = format!("s{}f", std::process::id());
    let n_a = format!("{tag}r1");
    let n_b = format!("{tag}r2");
    let n_c = format!("{tag}r3");
    let scratch = tempfile::Builder::new()
        .prefix("grovefs_shardkeys_")
        .tempdir()
        .unwrap();
    let (key, key_id) = admin_key(&pg, scratch.path(), &format!("shardtest-{tag}"));

    let p1 = free_port();
    let p2 = free_port();
    let p3 = free_port();
    let ring = format!("{n_a}=127.0.0.1:{p1},{n_b}=127.0.0.1:{p2},{n_c}=127.0.0.1:{p3}");
    let m1 = start(
        &n_a,
        &ring,
        &pg,
        None,
        &key,
        format!("127.0.0.1:{p1}"),
        format!("127.0.0.1:{}", free_port()),
    );
    let m2 = start(
        &n_b,
        &ring,
        &pg,
        None,
        &key,
        format!("127.0.0.1:{p2}"),
        format!("127.0.0.1:{}", free_port()),
    );
    let m3 = start(
        &n_c,
        &ring,
        &pg,
        None,
        &key,
        format!("127.0.0.1:{p3}"),
        format!("127.0.0.1:{}", free_port()),
    );
    let va = format!("{tag}va");
    let vb = format!("{tag}vb");
    m1.gov()
        .call("view.create", json!({"view": va}))
        .expect("va on m1");
    m3.gov()
        .call("view.create", json!({"view": vb}))
        .expect("vb on m3");

    // Neither mount is at its home, and neither home is the entry point's only
    // neighbour from the other's point of view: `va` lands on m1, `vb` on m3.
    let a = RemoteMeta::new(&m2.meta, &va).expect("va through m2");
    let b = RemoteMeta::new(&m2.meta, &vb).expect("vb through m2");
    a.create_node(ROOT_INO, "on-m1", true, None, 0o755, UID, GID)
        .expect("write follows va's row");
    b.create_node(ROOT_INO, "on-m3", true, None, 0o755, UID, GID)
        .expect("write follows vb's row");
    assert!(
        has_local_tree(m1.dir.path(), &va) && !has_local_tree(m3.dir.path(), &va),
        "va's tree must be m1's alone"
    );
    assert!(
        has_local_tree(m3.dir.path(), &vb) && !has_local_tree(m1.dir.path(), &vb),
        "vb's tree must be m3's alone"
    );
    assert!(
        !has_local_tree(m2.dir.path(), &va) && !has_local_tree(m2.dir.path(), &vb),
        "the entry point served two views it only had to redirect"
    );
    let rows = placement_rows(&pg, &ring, &n_a, &tag);
    // `placement_rows` sorts: the table read has no ORDER BY and a two-row
    // comparison on scan order would be a flake, not a finding.
    assert_eq!(
        rows,
        vec![(va.clone(), n_a.clone()), (vb.clone(), n_c.clone())],
        "one row per base, each naming the instance that created it"
    );
    // `store.info` on the entry point describes the whole ring, counting views
    // it serves itself (zero here) alongside the two it hands off.
    let i2 = m2.info();
    assert_eq!(i2["shard"]["self"], json!(n_b));
    assert_eq!(shard_of(&i2, &n_a)["views"], json!(1));
    assert_eq!(shard_of(&i2, &n_b)["views"], json!(0));
    assert_eq!(shard_of(&i2, &n_c)["views"], json!(1));
    assert_eq!(i2["views"], json!(0), "m2 materialized nothing");

    // ---- a row naming a node the ring does not define ----------------------
    // The retired-host case, spelled directly in the shared table because no
    // verb in the ring can produce it (that is the point: the ring changed
    // under the table). `drop` is the safe form — it costs no tree, only the
    // routing answer.
    let retire = |to: &str| {
        let mut c =
            postgres::Client::connect(&pg, postgres::NoTls).expect("pg for the edit");
        c.execute(
            "UPDATE grovefs_control.meta_placement SET meta = $1 WHERE view = $2",
            &[&to, &va],
        )
        .expect("retire m1 in the row");
    };
    let restore = || {
        let mut c =
            postgres::Client::connect(&pg, postgres::NoTls).expect("pg for the fix");
        let meta: &str = &n_a;
        c.execute(
            "UPDATE grovefs_control.meta_placement SET meta = $1 WHERE view = $2",
            &[&meta, &va],
        )
        .expect("put the row back");
    };
    retire(&format!("{tag}gone"));
    // Every instance now has a verdict it cannot honour. Each must REFUSE with
    // the name in the row — serving the tree locally would give two answers to
    // one name, which is the data-loss shape ADR-0006 §2 exists to prevent.
    for entry in [&m1.meta, &m2.meta, &m3.meta] {
        let e = RemoteMeta::new(entry, &va)
            .err()
            .unwrap_or_else(|| panic!("{entry} mounted a view whose row names a retired node"));
        let msg = e.to_string();
        assert!(
            msg.contains("does not define") && msg.contains(&format!("'{tag}gone")),
            "unhelpful refusal from {entry}: {msg}"
        );
        assert!(
            msg.contains(&n_c) || msg.contains(&n_a) || msg.contains(&n_b),
            "the refusal must list the ring it knows: {msg}"
        );
    }
    // The refusals must not have touched the table: an `M_OPEN` that is refused
    // reads the row and answers, it never re-places a name it cannot serve.
    let refused_rows = placement_rows(&pg, &ring, &n_a, &tag);
    assert_eq!(
        refused_rows,
        vec![
            (va.clone(), format!("{tag}gone")),
            (vb.clone(), n_c.clone()),
        ],
        "refused mounts rewrote the placement table"
    );
    // `store.info` must SHOW the dangling row instead of folding it away: it
    // lands in no node's tally, so a page that only had per-node counts would
    // report a ring that simply lost a view.
    let orphan = json!(format!("{va} → {tag}gone"));
    let iret = m2.info();
    assert!(
        iret["shard"]["orphanViews"]
            .as_array()
            .unwrap()
            .contains(&orphan),
        "the shard block hid the row that names no ring node: {}",
        iret["shard"]
    );
    assert_eq!(
        shard_of(&iret, &n_a)["views"],
        json!(0),
        "an orphan row counts for nobody"
    );
    assert_eq!(shard_of(&iret, &n_a)["rows"], json!(0));
    // `vb`, whose row is still valid, is untouched by m1's retirement: a
    // dangling row is one view's problem, not the ring's.
    let _b2 = RemoteMeta::new(&m3.meta, &vb).expect("vb still routes");
    restore();
    let _a2 = RemoteMeta::new(&m2.meta, &va).expect("a restored row routes again");
    assert!(
        !m2.info()["shard"]["orphanViews"]
            .as_array()
            .unwrap()
            .contains(&orphan),
        "the orphan list still names a row that points back into the ring"
    );

    let mut teardown = Vec::new();
    for node in [&m1, &m3] {
        for view in [&va, &vb] {
            if let Err(e) = node.gov().call("view.rm_base", json!({"view": view})) {
                teardown.push(format!("{}/{}: {e}", node.ctl, view));
            }
        }
    }
    // The regression this line catches is the one the DIAG walk found: `view rm`
    // asked at a PEER whose tree is already gone used to answer "no physical
    // base tree" — after `Shard::route`'s bootstrap arm had pinned the name to
    // that peer. A destroy verb must read the row, never write it.
    let rows = placement_rows(&pg, &ring, &n_a, &tag);
    assert!(
        rows.is_empty(),
        "placement rows outlived the ring they named: {rows:?} (refused teardowns: {teardown:?})"
    );
    cleanup(&pg, scratch.path(), &key_id);
}

// TODO(severity=High): a HOME NODE GOING DOWN mid-mount is not covered here,
// because nothing in this file can stop an instance: `MetaServer::serve()` has
// no shutdown control op and the accept loops outlive the test's `Node` struct,
// so a dead ring member can only be simulated by a port that was never bound
// (the redirect then fails with a transport error, which is a different answer
// from "the tree is unreachable because its host is gone").  Needs a stop
// handle on `MetaServer` (or a `node.shutdown` control op) plus the assertion
// that a mount dials the NEXT address in the failover list rather than the
// retired home.
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

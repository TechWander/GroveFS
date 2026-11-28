//! What a GroveFS deployment does when PostgreSQL refuses one more connection.
//!
//! The ceiling is a real operation event, not a hypothetical: every view an
//! instance serves holds its own writer/reader lanes (defaults 8 + 4 warm
//! backends per active view — see `src/pg_store.rs`'s `PG_WRITE_POOL_SIZE`), so
//! `max_connections` is a CLUSTER number that a modest number of busy views can
//! reach, and a `DATABASE`-scoped limit is the cheap knob an operator pulls to
//! make room for one tenant by starving the rest. Two properties are worth
//! having a test for, because they are the ones an incident report argues about:
//!
//!   * a mount that is ALREADY OPEN must keep working. Its lanes are warm; an
//!     op on it must not need a new backend. If it fails, the refusal is a
//!     self-inflicted outage of every tenant, not a limit on one;
//!   * the mount that CANNOT start must fail with the server's reason in its
//!     message, on one line, in bounded time. "EIO" and a hang are both answers
//!     an operator cannot act on, and both were reachable here: the pool
//!     checkout path dials on demand, so a refused connect is the ONLY thing
//!     that surfaces this event to the client, and what it says is the product.
//!
//! The ceiling is applied to a PRIVATE DATABASE created for this run
//! (`CREATE DATABASE` needs `CREATEDB`; without it the file SELF-SKIPS), so no
//! other test on this cluster sees a slot taken away — the shared `grovefs`
//! database keeps working while this one is squeezed. Teardown stops the
//! instance through its [`ServerStop`] handle first: a `DROP DATABASE` on PG 12
//! has no `FORCE`, and the only reliable way to empty a database is to make the
//! process holding its backends shut them, which is what the stop seam
//! guarantees (nothing of the instance outlives `stop()`).

use grovefs::apikeys::{ApiKeyStore, Scope};
use grovefs::control_remote::ControlClient;
use grovefs::meta::{MetaProvider, ROOT_INO};
use grovefs::remote::meta::{MetaServer, RemoteMeta, ServerStop};
use serde_json::json;
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

const UID: u32 = 1000;
const GID: u32 = 1000;

/// A PostgreSQL this machine can reach, or `None` (→ self-skip). Same probe as
/// the sharding suite's, and the same reason: no server means nothing to
/// squeeze, and a suite that fails because a laptop has no PostgreSQL teaches
/// nothing.
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

/// `postgres://user:pw@host:port/grovefs` → the same endpoint without a
/// database, and the database name this run must not touch.
fn split_uri(pg: &str) -> Option<(String, String)> {
    let i = pg.rfind('/')?;
    Some((pg[..i].to_string(), pg[i + 1..].to_string()))
}

/// A connection to the maintenance database (`postgres`), which is where
/// `CREATE DATABASE` / `ALTER DATABASE` / `DROP DATABASE` for another database
/// have to run.
fn admin(base: &str) -> Option<postgres::Client> {
    postgres::Client::connect(format!("{base}/postgres").as_str(), postgres::NoTls).ok()
}

/// The reply-shape rule the rest of the suite uses: a refusal that reaches an
/// operator as several indented lines cannot be grepped or pasted.
fn one_line(text: &str) {
    assert!(
        !text.contains('\n'),
        "a reply arrived as several lines: {text:?}"
    );
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("free port")
        .local_addr()
        .unwrap()
        .port()
}

/// One `meta-server` whose TREES and control plane are the private database.
///
/// `serve()` refuses a PG tree store plus a second control plane, which is
/// exactly the shape wanted here: ONE backend, so the connection limit being
/// squeezed is the same limit every lane dials through.
struct Node {
    /// The instance's `<base>` directory. Nothing here inspects it — the trees
    /// live in the private database — but HOLDING it is the point: the tempdir
    /// must outlive the server, and dropping it is the local half of teardown.
    #[allow(dead_code)]
    dir: tempfile::TempDir,
    meta: String,
    ctl: String,
    key: String,
    stop: ServerStop,
    _srv: std::thread::JoinHandle<()>,
}

impl Node {
    fn boot(uri: &str, key: &str) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("grovefs_connlimit_")
            .tempdir()
            .unwrap();
        let meta = format!("127.0.0.1:{}", free_port());
        let ctl = format!("127.0.0.1:{}", free_port());
        let srv = MetaServer::new(&meta, dir.path())
            .with_engine_idle_secs(0)
            .with_control_listen(Some(ctl.clone()))
            .with_store(Some(uri.to_string()));
        let stop = srv.stop_handle();
        let _srv = std::thread::spawn(move || {
            let _ = srv.serve();
        });
        let n = Self {
            dir,
            meta,
            ctl,
            key: key.to_string(),
            stop,
            _srv,
        };
        n.wait_up();
        n
    }

    fn gov(&self) -> ControlClient {
        ControlClient::new(&self.ctl, &self.key)
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

    /// Stop the instance and wait for both ports to go: the backends it dials
    /// belong to the database this case has to drop.
    fn down(&self) {
        self.stop.stop();
        for addr in [&self.meta, &self.ctl] {
            let Ok(ip) = addr.parse::<std::net::SocketAddr>() else {
                panic!("bad address {addr}");
            };
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if TcpStream::connect_timeout(&ip, Duration::from_millis(100)).is_err() {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "{addr} still accepts connections 10 s after stop()"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

/// How many backends the private database holds, as the server itself counts
/// them — the number the limit is set FROM, so the ceiling is measured instead
/// of assumed, and so a lane the implementation changed since this file was
/// written does not silently turn the test into a no-op.
fn backends(admin: &mut postgres::Client, db: &str) -> i64 {
    admin
        .query(
            "SELECT count(*) FROM pg_stat_activity WHERE datname = $1",
            &[&db],
        )
        .unwrap()
        .iter()
        .map(|r| r.get::<_, i64>(0))
        .next()
        .unwrap_or(0)
}

#[test]
fn a_database_at_its_connection_limit_still_serves_an_open_mount() {
    let Some(pg) = pg_uri() else {
        eprintln!("skipping: no PostgreSQL reachable (set GROVEFS_PG_TEST_URI)");
        return;
    };
    let Some((base, _shared_db)) = split_uri(&pg) else {
        eprintln!("skipping: cannot read a database name out of {pg}");
        return;
    };
    let Some(mut admin) = admin(&base) else {
        eprintln!("skipping: cannot reach the maintenance database at {base}/postgres");
        return;
    };
    let db = format!("grovefs_connlimit_{}", std::process::id());
    // The private database is the whole safety of this case: it must be ours to
    // create and ours to drop, and a role without CREATEDB means "cannot run
    // this here", not "the behaviour is correct".
    if let Err(e) = admin.batch_execute(&format!("CREATE DATABASE {db}")) {
        eprintln!("skipping: cannot CREATE DATABASE {db} on {base}: {e}");
        return;
    }
    let uri = format!("{base}/{db}");
    let scratch = tempfile::Builder::new()
        .prefix("grovefs_connkeys_")
        .tempdir()
        .unwrap();
    // Teardown is armed FIRST and runs on every later panic too: a leaked
    // private database is a surprise for the next run's operator, and PG 12 has
    // no `DROP DATABASE ... WITH (FORCE)` to lean on.
    let drop_db = |admin: &mut postgres::Client| {
        for _ in 0..40 {
            let pids = admin
                .query(
                    "SELECT pid FROM pg_stat_activity WHERE datname = $1",
                    &[&db],
                )
                .unwrap_or_default();
            for pid in pids {
                let pid: i32 = pid.get(0);
                let _ = admin
                    .query("SELECT pg_terminate_backend($1)", &[&pid])
                    .map(|_| ());
            }
            match admin.batch_execute(&format!("DROP DATABASE IF EXISTS {db}")) {
                Ok(()) => return,
                Err(_) => std::thread::sleep(Duration::from_millis(250)),
            }
        }
        eprintln!("could not drop the private database {db}; it is left for an operator to remove");
    };

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_case(&uri, &db, &mut admin, &scratch)
    }));

    drop_db(&mut admin);
    if let Err(p) = result {
        std::panic::resume_unwind(p);
    }
}

fn run_case(
    uri: &str,
    db: &str,
    admin: &mut postgres::Client,
    scratch: &tempfile::TempDir,
) {
    let tag = format!("{}cl", std::process::id());
    let store = ApiKeyStore::open_for(scratch.path(), Some(uri)).expect("open key store");
    let (_id, key) = store
        .create_for(&format!("connlimit-{tag}"), Scope::Admin)
        .expect("mint admin key");
    let node = Node::boot(uri, &key);

    let v1 = format!("{tag}v1");
    let v2 = format!("{tag}v2");
    for v in [&v1, &v2] {
        node.gov()
            .call("view.create", json!({"view": v}))
            .expect("create a view while the database has headroom");
    }

    // Warm v1's lanes with a real mount and one write, so "the open mount keeps
    // working" is measured against a view that has connections to reuse.
    let m1 = RemoteMeta::new(&node.meta, &v1).expect("mount v1 at the fresh instance");
    m1.create_node(ROOT_INO, "before-the-limit", true, None, 0o0755, UID, GID)
        .expect("mkdir through the first mount");
    let warm = backends(admin, db);
    assert!(
        warm >= 2,
        "the instance is supposed to hold a lane of backends per active view; the \
         private database only has {warm}, so this case would be proving nothing"
    );

    // Pin the ceiling to exactly what is connected. From here the database can
    // hand out NOTHING new, and every existing backend keeps its session.
    admin
        .batch_execute(&format!("ALTER DATABASE {db} WITH CONNECTION LIMIT {warm}"))
        .expect("set the per-database connection limit");

    // (1) THE OPEN MOUNT IS UNAFFECTED. A write on a warm lane must not need a
    // new connection; if this fails, the limit is a global outage switch and no
    // operator can use it to shape one tenant.
    m1.create_node(ROOT_INO, "written-at-the-limit", true, None, 0o0755, UID, GID)
        .expect("an already-open mount failed once the database hit its connection limit");
    let names: Vec<String> = m1
        .readdir(ROOT_INO)
        .into_iter()
        .map(|n| n.name)
        .collect();
    assert!(
        names.iter().any(|n| n == "written-at-the-limit"),
        "the write did not land in the open mount's tree: {names:?}"
    );

    // (2) THE NEW MOUNT FAILS WITH THE SERVER'S REASON. Bounded, one line, and
    // naming the refusal — the three things that make it actionable.
    let t = Instant::now();
    let e = RemoteMeta::new(&node.meta, &v2)
        .err()
        .unwrap_or_else(|| panic!("a view mounted while its database was at its connection limit"));
    assert!(
        t.elapsed() < Duration::from_secs(60),
        "the refusal of a mount that cannot dial a connection took {:?}",
        t.elapsed()
    );
    let text = e.to_string();
    one_line(&text);
    let lower = text.to_lowercase();
    assert!(
        lower.contains("connection limit") || lower.contains("too many connections"),
        "the refusal must carry PostgreSQL's reason, not a bare errno. It said: {text}"
    );
    // The numbers this run actually measured, so a later change to the lane
    // defaults shows up as a moved line in the log instead of a silently vacuous
    // assertion: `warm` is what the limit was pinned to, `text` is the answer an
    // operator would read.
    eprintln!(
        "pg_conn_limit: limit pinned at the {warm} backends the open view held; the refused \
         mount answered: {text}"
    );

    // (3) IT IS A CEILING, NOT A POISON: lift it and the same mount comes up,
    // with no restart of the instance that was refusing.
    admin
        .batch_execute(&format!("ALTER DATABASE {db} WITH CONNECTION LIMIT -1"))
        .expect("lift the per-database connection limit");
    let m2 = RemoteMeta::new(&node.meta, &v2)
        .expect("the second view still cannot mount after the limit was lifted");
    m2.create_node(ROOT_INO, "after-the-lift", true, None, 0o0755, UID, GID)
        .expect("mkdir through the recovered mount");

    // Stop BEFORE the teardown drops the database: the instance owns backends
    // there, and PG 12 will not force them off.
    node.down();
    assert!(node._srv.is_finished());
}

//! One control-plane database, one pool (M4).
//!
//! A `meta-server` pointed at PostgreSQL uses the SAME database for three
//! control-plane tables: view definitions (`grovefs_control.view_defs`),
//! credentials/audit (`api_keys`/`mcp_calls`) and web accounts
//! (`web_accounts`/`web_grants`). Each of those stores used to build its own
//! `PgPool`, and a pool eager-connects so an unreachable control plane fails at
//! open — so an IDLE process, with no mount and no governance call, already sat
//! on three backends of one database, and after a burst it could retain up to
//! `3 × CONTROL_POOL_SIZE` = 12 of them. That is a `53300 remaining connection
//! slots are reserved…` refusal arriving for traffic that had not happened yet
//! (`tests/pg_conn_limit.rs` covers what such a refusal must then say).
//!
//! What is asserted here is the shape the fix had to keep:
//!
//!   * ONE warm session per control database per process, no matter how many
//!     stores name it (the registry is keyed on the EXPANDED URI, so a sharded
//!     `uri,uri` spec and the bare instance are the same pool);
//!   * a consumer's DDL still runs when it is NOT the one that opened the pool.
//!     Sharing sessions must not mean sharing table-creation luck, or whichever
//!     store started first would decide which tables exist — the exact drift
//!     that ordering bugs are made of;
//!   * every store keeps working THROUGH the shared handle (define a lens, mint
//!     a key, create an account) — a pool that deduplicates but breaks a caller
//!     is worse than three pools.
//!
//! The shard-placement pool is deliberately NOT shared: its sessions are shaped
//! with a 2 s `statement_timeout` so a wedged control plane cannot park a meta
//! worker, and putting credential checks and DDL behind that ceiling is a
//! contract change, not a connection saving (see the note in
//! `src/remote/meta/shard.rs`). Nothing here asserts about it.
//!
//! Each case creates its own database and tears it down (`pg_terminate_backend`
//! then `DROP DATABASE`, because PG 12 has no `WITH (FORCE)`), so a run leaves
//! no backend of its own to make the NEXT count wrong — the counting is only
//! meaningful against a database nobody else is in. Without `CREATEDB`, or
//! without a PostgreSQL at all, the file self-skips.

use grovefs::apikeys::ApiKeyStore;
use grovefs::control;
use grovefs::mcp::{ControlStore, PgControlStore};
use grovefs::webauth::{Role, WebAuthStore};
use std::net::TcpStream;
use std::time::{Duration, Instant};

/// A PostgreSQL this machine can reach, or `None` (→ self-skip). Same probe as
/// the connection-ceiling suite's, same reason.
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

/// `postgres://user:pw@host:port/grovefs` → (endpoint without database, database
/// name this run must not touch).
fn split_uri(pg: &str) -> Option<(String, String)> {
    let i = pg.rfind('/')?;
    Some((pg[..i].to_string(), pg[i + 1..].to_string()))
}

/// A connection to the maintenance database, where `CREATE`/`DROP DATABASE` for
/// another database have to run.
fn admin(base: &str) -> Option<postgres::Client> {
    postgres::Client::connect(format!("{base}/postgres").as_str(), postgres::NoTls).ok()
}

/// A private database for one case, dropped (loudly) with its backends
/// terminated first.
struct PrivDb {
    uri: String,
    name: String,
    admin: postgres::Client,
}

impl PrivDb {
    /// `None` when this machine cannot grant `CREATEDB` — a shared cluster's
    /// test role usually can, and if it cannot, the case has nothing to count.
    fn new(base: &str, tag: &str) -> Option<Self> {
        let mut admin = admin(base)?;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let name = format!("grovefs_pool_{tag}_{}_{:x}", std::process::id(), nanos);
        admin
            .batch_execute(&format!("CREATE DATABASE {name}"))
            .map_err(|e| eprintln!("skipping: cannot CREATE DATABASE {name} on {base}: {e}"))
            .ok()?;
        Some(Self {
            uri: format!("{base}/{name}"),
            name,
            admin,
        })
    }

    /// Backends currently dialing THIS database, excluding the counting session.
    fn backends(&mut self) -> usize {
        let rows = self
            .admin
            .query(
                "SELECT count(*) FROM pg_stat_activity WHERE datname = $1 AND pid <> pg_backend_pid()",
                &[&self.name],
            )
            .expect("pg_stat_activity is readable");
        rows[0].get::<_, i64>(0) as usize
    }

    /// Wait for the count to settle at `want`, then hand back what it actually
    /// is. Settling matters in both directions: a just-terminated backend lags
    /// in `pg_stat_activity`, and a connect that has not been sampled yet would
    /// read one short.
    fn wait_backends(&mut self, want: usize) -> usize {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let n = self.backends();
            if n == want || Instant::now() >= deadline {
                return n;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for PrivDb {
    fn drop(&mut self) {
        // The process-wide registry keeps the pool alive until exit, so the
        // backends have to be put out of their misery from the server side —
        // the one teardown step that makes the NEXT case's count trustworthy.
        let mut pids: Vec<i32> = Vec::new();
        {
            let rows = self
                .admin
                .query(
                    "SELECT pid FROM pg_stat_activity WHERE datname = $1",
                    &[&self.name],
                )
                .unwrap_or_default();
            for row in rows.iter() {
                pids.push(row.get::<_, i32>(0));
            }
        }
        for pid in pids {
            let _ = self.admin.query("SELECT pg_terminate_backend($1)", &[&pid]);
        }
        let _ = self
            .admin
            .batch_execute(&format!("DROP DATABASE IF EXISTS {}", self.name));
    }
}

/// The number the fix is about: three stores, one database, ONE warm session.
#[test]
fn three_control_plane_stores_on_one_database_hold_one_warm_session() {
    let Some(pg) = pg_uri() else {
        eprintln!("skipping: no PostgreSQL reachable (set GROVEFS_PG_TEST_URI)");
        return;
    };
    let Some((base, _)) = split_uri(&pg) else {
        eprintln!("skipping: cannot read a database name out of {pg}");
        return;
    };
    let Some(mut db) = PrivDb::new(&base, "trio") else { return };
    let dir = tempfile::Builder::new()
        .prefix("grovefs_pool_")
        .tempdir()
        .unwrap();

    // Open in whatever order; each runs its own DDL through the shared handle.
    let defs = PgControlStore::new(&db.uri).expect("definition store on a fresh database");
    let keys = ApiKeyStore::open_pg(&db.uri, dir.path()).expect("key store on a fresh database");
    let web = WebAuthStore::open_pg(&db.uri).expect("web store on a fresh database");

    // Three stores, one database: the honest count is the sessions they share,
    // not three times a warm floor. Before the registry this read 3.
    let n = db.wait_backends(1);
    assert_eq!(
        n, 1,
        "three control-plane stores held {n} backends of one database; one pool was the point"
    );

    // And every store is really usable THROUGH the shared handle — a pool that
    // deduplicates and breaks a caller would be worse than three pools.
    defs.put_def("bias_poolx", "alias alpha\n")
        .expect("define through the shared pool");
    assert_eq!(
        defs.get_def_raw("bias_poolx").unwrap().as_deref(),
        Some("alias alpha\n")
    );
    let (id, _secret) = keys.create("pool-trio").expect("mint through the shared pool");
    assert!(!id.is_empty());
    web.create("pool-trio", "not-a-real-hash", Role::User)
        .expect("create an account through the shared pool");
    assert!(web
        .get("pool-trio")
        .unwrap()
        .is_some_and(|a| a.account == "pool-trio"));

    // A fourth consumer of the same URI adds no session either — the registry,
    // not a fresh pool, is what it finds.
    let extra = control::shared(&db.uri, &[]).expect("reuse the pool");
    assert_eq!(extra.uri(), db.uri);
    let n = db.wait_backends(1);
    assert_eq!(n, 1, "a fourth store opened a second pool: {n} backends");
}

/// A sharded spec, a bare instance, a repeated spelling: all one pool. This is
/// the property that makes the count above independent of how the deployment
/// was written down.
#[test]
fn one_control_database_is_one_pool_however_the_spec_is_spelled() {
    let Some(pg) = pg_uri() else {
        eprintln!("skipping: no PostgreSQL reachable (set GROVEFS_PG_TEST_URI)");
        return;
    };
    let Some((base, _)) = split_uri(&pg) else {
        eprintln!("skipping: cannot read a database name out of {pg}");
        return;
    };
    let db = PrivDb::new(&base, "spell").expect("cannot CREATE DATABASE");

    let plain = control::shared(&db.uri, &[]).expect("open the instance");
    // The SAME database named as a two-instance spec whose control instance is
    // this one (what `--meta-store uri,uri` expands to) must not double up.
    let spec = format!("{},{}", db.uri, db.uri);
    let via_spec = control::shared(&spec, &[]).expect("open through a spec");
    assert!(
        std::sync::Arc::ptr_eq(&plain, &via_spec),
        "one control database reached two pools because it was spelled two ways"
    );
    // The handle is bound to the EXPANDED URI, so error text names one
    // canonical database whatever the deployment configured.
    assert_eq!(via_spec.uri(), db.uri);
}

/// Sharing sessions must NOT share table-creation luck: the store that arrives
/// second still creates its own tables. Pinned by making the FIRST caller one
/// that asks for no DDL at all.
#[test]
fn the_consumer_that_did_not_open_the_pool_still_gets_its_tables() {
    let Some(pg) = pg_uri() else {
        eprintln!("skipping: no PostgreSQL reachable (set GROVEFS_PG_TEST_URI)");
        return;
    };
    let Some((base, _)) = split_uri(&pg) else {
        eprintln!("skipping: cannot read a database name out of {pg}");
        return;
    };
    let mut db = PrivDb::new(&base, "ddl").expect("cannot CREATE DATABASE");

    // Register the pool WITHOUT any control DDL, on a brand-new database.
    let bare = control::shared(&db.uri, &[]).expect("a no-DDL open still hands out a handle");
    // Through the pool's own helper, so the session goes back and the count at
    // the end measures POOLED sessions, not this probe's borrow.
    let present = bare
        .query(
            "SELECT to_regclass('grovefs_control.view_defs') IS NOT NULL",
            &[],
            |r| r.get::<_, bool>(0),
        )
        .expect("to_regclass is readable");
    assert!(
        !present[0],
        "a bare open created the definition table it was not asked to own"
    );

    // Now the real store opens on the HOT pool: its DDL must run anyway.
    let defs = PgControlStore::new(&db.uri).expect("definition store on a hot pool");
    defs.put_def("bias_poold", "alias alpha\n")
        .expect("define through a pool someone else opened");
    assert!(defs
        .get_def_raw("bias_poold")
        .unwrap()
        .is_some_and(|t| t.starts_with("alias ")));

    // And the ordering works in the other direction too: keys on a pool whose
    // schema came from the definition store still get their own tables.
    let dir = tempfile::Builder::new()
        .prefix("grovefs_pool_")
        .tempdir()
        .unwrap();
    let keys = ApiKeyStore::open_pg(&db.uri, dir.path()).expect("key store on a hot pool");
    keys.create("pool-ddl").expect("mint on a pool it did not create");
    let n = db.wait_backends(1);
    assert_eq!(n, 1, "{n} backends after three consumers of one database");
}

//! The MCP control-plane probes must be **read-only in the strongest sense**:
//! on a PostgreSQL-bound store, `open_ro_view` may not run the schema-ensure
//! DDL that a MOUNT uses to lazily create a view. Without the guard, probing a
//! typo'd view name silently minted a `grovefs_v_<hex>` schema, and the next
//! `list_views` reported that phantom as a physical view the agent never
//! created — a write hiding behind a "read-only introspection" tool.
//!
//! These tests self-skip when the PG endpoint is unreachable — the default is
//! `grove/grovepw@127.0.0.1:5432/grovefs` (the convention of
//! `.build-bin/test_pg.sh` and `tests/pg_ssi.rs`; `GROVEFS_TEST_PG_URI` points
//! them elsewhere), so the default host stays GREEN.

use grovefs::control::schema_name_for;
use grovefs::mcp::{ControlStore, PgControlStore};
use postgres::{Client, NoTls};
use std::net::TcpStream;
use std::time::Duration;

const PG_URI: &str = "postgres://grove:grovepw@127.0.0.1:5432/grovefs";

/// The PG endpoint under test. Defaults to the port `.build-bin/test_pg.sh` and
/// `tests/pg_ssi.rs` use; override with `GROVEFS_TEST_PG_URI` to point the same
/// assertions at another server (the TCP probe below follows the URI, so a
/// non-listening endpoint still self-skips instead of failing).
fn pg_uri() -> String {
    std::env::var("GROVEFS_TEST_PG_URI").unwrap_or_else(|_| PG_URI.to_string())
}

/// "host:port" of a URI, for the reachability probe.
fn pg_addr(uri: &str) -> String {
    let after = uri.split_once('@').map(|(_, r)| r).unwrap_or(uri);
    after
        .split('/')
        .next()
        .unwrap_or("127.0.0.1:5432")
        .to_string()
}

fn pg_ready(uri: &str) -> bool {
    pg_addr(uri)
        .parse()
        .ok()
        .map(|a| TcpStream::connect_timeout(&a, Duration::from_millis(200)).is_ok())
        .unwrap_or(false)
}

/// A view name that no test creates: short enough for the hex-schema encoding
/// and unique enough not to collide with a developer's own schemas.
fn phantom_view(tag: &str) -> String {
    format!("probe{}{}", tag, std::process::id())
}

#[test]
fn probe_of_unknown_pg_view_refuses_and_creates_no_schema() {
    let uri = pg_uri();
    if !pg_ready(&uri) {
        eprintln!("SKIP mcp_probe_ro::probe_of_unknown_pg_view_refuses_and_creates_no_schema: no PostgreSQL on {}", pg_addr(&uri));
        return;
    }
    let store = PgControlStore::new(&uri).expect("open PG control store");
    let view = phantom_view("aa");

    // The honest refusal, worded like the file store's so one doc covers both.
    // (`Arc<dyn MetaProvider>` is not `Debug`, so unwrap the error side by hand.)
    let err = match store.open_ro_view(&view) {
        Ok(_) => panic!("a probe must not open (let alone create) a tree that is not there"),
        Err(e) => e,
    };
    assert!(
        err.contains(&format!("no metadata tree for view '{view}'")),
        "unexpected refusal: {err}"
    );
    assert!(
        err.contains("list_view_tree walks this store's own backend"),
        "refusal must name the boundary: {err}"
    );

    // And nothing landed: the phantom is neither a schema nor a listed view.
    assert!(
        !store.has_physical(&view).unwrap(),
        "the probe created a PG schema for '{view}'"
    );
    assert!(
        !store.list_physical().unwrap().contains(&view),
        "the probe made '{view}' visible as a physical view"
    );
}

#[test]
fn probe_of_existing_pg_view_walks_it_without_mutating_the_view_set() {
    let uri = pg_uri();
    if !pg_ready(&uri) {
        eprintln!("SKIP mcp_probe_ro::probe_of_existing_pg_view_walks_it_without_mutating_the_view_set: no PostgreSQL on {}", pg_addr(&uri));
        return;
    }
    let store = PgControlStore::new(&uri).expect("open PG control store");
    let view = phantom_view("bb");
    let before = store.list_physical().unwrap();

    // A MOUNT-like open still creates the tree lazily (that contract is the
    // mount's, not the probe's); the probe then reads it and changes nothing.
    drop(grovefs::pg_store::PgEngine::open(&uri, &view, false).expect("create PG tree"));
    let after_create = store.list_physical().unwrap();
    assert!(after_create.contains(&view), "created tree not listed");

    let provider = store.open_ro_view(&view).expect("probe an existing tree");
    let root = provider.readdir(1);
    assert!(root.is_empty() || root.iter().all(|e| !e.name.is_empty()));
    assert_eq!(
        store.list_physical().unwrap(),
        after_create,
        "the probe changed the view set"
    );

    // Leave the shared database as we found it.
    Client::connect(&uri, NoTls)
        .expect("connect to drop the test schema")
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS {} CASCADE",
            schema_name_for(&view)
        ))
        .expect("drop the test schema");
    assert_eq!(store.list_physical().unwrap(), before);
}

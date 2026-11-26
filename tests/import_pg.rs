//! Integration coverage for `grovefs import` against a LIVE PostgreSQL
//! deployment (`--meta-store postgres://…`): the same `grovefs::import::ingest`
//! walk that serves SQLite runs here against `MetaStore::Pg`, so metadata nodes
//! land in the view schema's `node` table while the BYTES still land in the
//! local `--data-dir` under the identical `blocks/<bucket>/<key>` layout a
//! mounted write uses (one storage convention per deployment — req #2).
//!
//! Covers: fresh import, incremental append of new files/dirs, same-name skip
//! (idempotence, add-only), and permission/timestamp fidelity.
//!
//! Self-skips (prints a note, passes trivially) when PG is not reachable on
//! 127.0.0.1:5432 with the `grove/grovepw` role — the same convention and
//! credentials as `tests/pg_ssi.rs` — so the suite stays green on hosts
//! without a database. `GROVEFS_PG_TEST_URI` overrides the endpoint (the TCP
//! probe follows the override) for pointing the suite at a cluster box.

use grovefs::backend::local::LocalBackend;
use grovefs::backend::StorageBackend;
use grovefs::import::ImportStats;
use grovefs::meta::{MetaProvider, MetaStore, ROOT_INO};
use std::collections::HashMap;
use std::fs::{self, FileTimes};
use std::net::TcpStream;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::time::{Duration, SystemTime};

fn pg_uri() -> String {
    std::env::var("GROVEFS_PG_TEST_URI")
        .unwrap_or_else(|_| "postgres://grove:grovepw@127.0.0.1:5432/grovefs".to_string())
}

/// host:port of the current URI, defaulting to 127.0.0.1:5432.
fn pg_endpoint() -> (String, u16) {
    let rest = pg_uri().split("://").nth(1).unwrap_or("").to_string();
    let hostport = rest.rsplit('@').next().unwrap_or("").to_string();
    let hostport = hostport.split('/').next().unwrap_or(&hostport).to_string();
    let mut it = hostport.rsplitn(2, ':');
    let port = it.next().and_then(|p| p.parse().ok()).unwrap_or(5432);
    let host = it.next().unwrap_or("127.0.0.1").to_string();
    (host, port)
}

fn pg_ready() -> bool {
    let (host, port) = pg_endpoint();
    let ip: std::net::IpAddr = match host.parse() {
        Ok(ip) => ip,
        Err(_) => match std::net::ToSocketAddrs::to_socket_addrs(&(host.as_str(), port)) {
            Ok(mut a) => match a.next() {
                Some(sa) => sa.ip(),
                None => return false,
            },
            Err(_) => return false,
        },
    };
    TcpStream::connect_timeout(&(ip, port).into(), Duration::from_millis(500)).is_ok()
}

/// Short unique view name (the PG schema name caps at 25 raw bytes).
fn fresh_view(tag: &str) -> String {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{tag}{pid:07}{nanos:06}")
}

/// The schema the view's tree lives in — mirrors `pg_store::schema_for`
/// (`grovefs_v_` + lowercase hex of the raw name bytes).
fn schema_of(view: &str) -> String {
    let mut hex = String::with_capacity(view.len() * 2);
    for b in view.as_bytes() {
        hex.push_str(&format!("{b:02x}"));
    }
    format!("grovefs_v_{hex}")
}

/// Best-effort teardown: drop the test schema when a local `psql` exists so
/// repeated runs do not leave schemas behind. The assertions never depend on
/// it (every run uses a fresh unique view name anyway).
fn cleanup_pg(view: &str) {
    let uri = pg_uri();
    let role = uri
        .split("://")
        .nth(1)
        .and_then(|r| r.split('@').next())
        .and_then(|u| u.split(':').next())
        .unwrap_or("grove")
        .to_string();
    let db = uri.rsplit('/').next().unwrap_or("grovefs").to_string();
    let (host, port) = pg_endpoint();
    let schema = schema_of(view);
    if std::process::Command::new("psql")
        .env("PGPASSWORD", "grovepw")
        .args(["-h", &host, "-p", &port.to_string(), "-U", &role, "-d", &db, "-c"])
        .arg(format!("DROP SCHEMA IF EXISTS {schema} CASCADE"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_err()
    {
        eprintln!("note: no psql on PATH; leaving test schema {schema} behind");
    }
}

/// Open a fresh BASE view through the SAME entry point `grovefs import` uses.
fn open_pg_store(view: &str) -> Option<MetaStore> {
    if !pg_ready() {
        let (host, port) = pg_endpoint();
        eprintln!("SKIP import_pg: no PostgreSQL reachable at {host}:{port}");
        return None;
    }
    match MetaStore::open_pg(&pg_uri(), view, false) {
        Ok(m) => Some(m),
        Err(e) => {
            // Reachable port but refused open = env problem (role/db), not a
            // code problem: skip with the reason instead of failing the suite.
            eprintln!("SKIP import_pg: cannot open PG store: {e}");
            None
        }
    }
}

fn seed_dirs(meta: &dyn MetaProvider, prefix: &str) -> HashMap<String, i64> {
    let mut dirs = HashMap::new();
    dirs.insert("/".to_string(), ROOT_INO);
    grovefs::import::ensure_dir_soft(meta, prefix, &mut dirs).unwrap();
    dirs
}

#[test]
fn pg_import_full_pipeline() {
    let view = fresh_view("ipg");
    let Some(meta) = open_pg_store(&view) else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let src = dir.path().join("legacy");
    fs::create_dir_all(src.join("sub")).unwrap();

    let f = src.join("a.txt");
    fs::write(&f, b"alpha").unwrap();
    fs::set_permissions(&f, fs::Permissions::from_mode(0o640)).unwrap();
    let mt = SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
    let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_500_000_000);
    fs::File::open(&f)
        .unwrap()
        .set_times(FileTimes::new().set_modified(mt).set_accessed(at))
        .unwrap();
    fs::write(src.join("sub/b.bin"), b"bravo").unwrap();
    // Symlinks stay skipped, never followed.
    std::os::unix::fs::symlink(&f, src.join("l.txt")).unwrap();

    let be = LocalBackend::new(&home).unwrap();
    let mut dirs = seed_dirs(&meta, "/legacy");
    let mut st = ImportStats::default();
    grovefs::import::ingest(&meta, &be, &src, "/legacy", &mut dirs, &mut st, None).unwrap();
    assert_eq!(st.dirs, 1, "only the real dir mirrors: {st:?}");
    assert_eq!(st.linked + st.copied, 2, "two files registered: {st:?}");
    assert_eq!(st.skipped, 1, "the symlink is counted, not imported");
    assert_eq!(st.failed, 0);

    // NODES live in the PG `node` table (through the engine interface): the
    // tree shape and the content behind each data_ref both answer.
    let l = meta.lookup(ROOT_INO, "legacy").expect("PG view tree root");
    let a = meta.lookup(l.id, "a.txt").expect("PG node a.txt");
    assert_eq!(a.mode & 0o7777, 0o640, "perm bits survive into PG");
    let fm = fs::metadata(&f).unwrap();
    assert_eq!(a.uid, fm.uid());
    assert_eq!(a.gid, fm.gid());
    assert_eq!(a.mtime, 1_600_000_000, "PG keeps the file's own clock");
    assert_eq!(a.atime, 1_500_000_000);
    let ka = a.data_ref.clone().expect("PG node carries a data_ref");
    assert_eq!(be.read(&ka, 0, 10).unwrap(), b"alpha");

    // BYTES keep the one blocks/<bucket>/<key> convention (req #2).
    let obj = be.object_path(&ka);
    assert!(obj.starts_with(home.join("blocks")), "{obj:?} outside blocks/");
    assert_eq!(obj.parent().unwrap().file_name().unwrap().len(), 2);
    assert_eq!(
        fs::metadata(&obj).unwrap().ino(),
        fm.ino(),
        "same-fs import is zero-copy under PG too"
    );

    // IDEMPOTENCE: the exact same re-run registers nothing and changes nothing.
    let before: Vec<String> = meta.readdir(l.id).into_iter().map(|n| n.name).collect();
    let mut st2 = ImportStats::default();
    grovefs::import::ingest(&meta, &be, &src, "/legacy", &mut dirs, &mut st2, None).unwrap();
    assert_eq!(st2.linked + st2.copied, 0, "re-run must not churn: {st2:?}");
    let after: Vec<String> = meta.readdir(l.id).into_iter().map(|n| n.name).collect();
    assert_eq!(before, after);

    // INCREMENTAL APPEND: new file + new dir after the watermark are picked up;
    // a source-side DELETE is never mirrored back as a view deletion.
    std::thread::sleep(Duration::from_millis(1100));
    let wm = st.watermark.expect("a real run records a watermark");
    let now = grovefs::import::epoch_secs(SystemTime::now());
    fs::write(src.join("sub/new.txt"), b"delta").unwrap();
    fs::create_dir_all(src.join("brand")).unwrap();
    fs::write(src.join("brand/e.txt"), b"epsilon").unwrap();
    let mut st3 = ImportStats::default();
    grovefs::import::ingest(
        &meta,
        &be,
        &src,
        "/legacy",
        &mut dirs,
        &mut st3,
        Some(wm.max(now - 1)),
    )
    .unwrap();
    let sub = meta.lookup(l.id, "sub").unwrap();
    let n = meta.lookup(sub.id, "new.txt").expect("PG sees the appended file");
    let kn = n.data_ref.clone().unwrap();
    assert_eq!(be.read(&kn, 0, 10).unwrap(), b"delta");
    let brand = meta.lookup(l.id, "brand").expect("PG sees the appended dir");
    let e = meta.lookup(brand.id, "e.txt").unwrap();
    let ke = e.data_ref.clone().unwrap();
    assert_eq!(be.read(&ke, 0, 10).unwrap(), b"epsilon");

    fs::remove_file(src.join("a.txt")).unwrap();
    let mut st4 = ImportStats::default();
    grovefs::import::ingest(&meta, &be, &src, "/legacy", &mut dirs, &mut st4, None).unwrap();
    assert!(meta.lookup(l.id, "a.txt").is_some(), "PG import is add-only too");

    cleanup_pg(&view);
}

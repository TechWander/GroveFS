//! End-to-end test of the remote METADATA path over a real localhost TCP
//! socket: a [`MetaServer`] rooted in a temp dir, driven by [`RemoteMeta`]
//! clients. Exercises the framing + `M_OPEN` handshake + dispatch + real SQLite
//! view operations without mounting FUSE, and proves the key property: two
//! clients of the same view share ONE authoritative view tree.

use std::net::TcpListener;

use grovefs::meta::{MetaProvider, NodeKind, ROOT_INO};
use grovefs::remote::meta::{open_event_stream, serve_on_meta, serve_on_meta_auth, RemoteMeta};
use grovefs::remote::proto;

fn spawn_meta_server() -> (String, tempfile::TempDir) {
    let dir = tempfile::Builder::new()
        .prefix("grovefs_msrv_")
        .tempdir()
        .unwrap();
    let base = dir.path().to_path_buf();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || serve_on_meta(listener, base));
    (addr, dir)
}

fn connect(addr: &str, view: &str) -> RemoteMeta {
    // Retry a moment: the server thread may not be accepting yet.
    for _ in 0..100 {
        if let Ok(c) = RemoteMeta::new(addr, view) {
            return c;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!("could not connect to test meta-server at {addr} as {view}");
}

const UID: u32 = 1000;
const GID: u32 = 1000;

#[test]
fn meta_core_ops_roundtrip() {
    let (addr, _d) = spawn_meta_server();
    let m = connect(&addr, "alice");

    // mkdir
    let dir = m
        .create_node(ROOT_INO, "projects", true, None, 0o0755, UID, GID)
        .unwrap();
    let nd = m.getattr(dir).unwrap();
    assert_eq!(nd.kind, NodeKind::Dir);
    assert_eq!(nd.name, "projects");

    // lookup finds it by name
    let lk = m.lookup(ROOT_INO, "projects").unwrap();
    assert_eq!(lk.id, dir);

    // create file node backed by a data key
    let file = m
        .create_node(ROOT_INO, "readme", false, Some("key-1"), 0o0644, UID, GID)
        .unwrap();
    let nf = m.getattr(file).unwrap();
    assert_eq!(nf.kind, NodeKind::Regular);
    assert_eq!(nf.data_ref.as_deref(), Some("key-1"));
    assert_eq!(nf.nlink, 1);

    // readdir lists both children deterministically
    let kids = m.readdir(ROOT_INO);
    let names: Vec<_> = kids.iter().map(|k| k.name.as_str()).collect();
    assert!(
        names.contains(&"projects") && names.contains(&"readme"),
        "got {names:?}"
    );

    // hard link shares the inode; nlink becomes 2
    let shared = m.link_node(file, ROOT_INO, "readme_hard").unwrap();
    assert_eq!(shared, file);
    assert_eq!(m.getattr(file).unwrap().nlink, 2);

    // unlink one hardlink name -> object NOT GC'd (still referenced)
    assert_eq!(m.unlink(ROOT_INO, "readme_hard").unwrap(), None);
    assert_eq!(m.getattr(file).unwrap().nlink, 1);

    // unlink the last name -> returns the freed data key for GC
    assert_eq!(
        m.unlink(ROOT_INO, "readme").unwrap().as_deref(),
        Some("key-1")
    );
    assert!(m.getattr(file).is_none());

    // symlink roundtrip + readlink target preserved in node
    let sl = m
        .create_symlink(ROOT_INO, "shortcut", "projects/inside", UID, GID)
        .unwrap();
    let ns = m.getattr(sl).unwrap();
    assert_eq!(ns.kind, NodeKind::Symlink);
    assert_eq!(ns.link_target.as_deref(), Some("projects/inside"));

    // setattr-style mutations cross the wire
    m.set_mode(sl, 0o0700);
    assert_eq!(m.getattr(sl).unwrap().mode & 0o7777, 0o0700);
    m.set_times(dir, Some(111), Some(222));
    let after = m.getattr(dir).unwrap();
    assert_eq!((after.atime, after.mtime), (111, 222));

    // rename within the tree
    m.rename(ROOT_INO, "projects", ROOT_INO, "projects2", false)
        .unwrap();
    assert!(m.lookup(ROOT_INO, "projects").is_none());
    assert_eq!(m.lookup(ROOT_INO, "projects2").unwrap().id, dir);

    // EEXIST / ENOTEMPTY / ENOENT error codes survive serialization
    assert!(m
        .create_node(ROOT_INO, "projects2", true, None, 0o755, UID, GID)
        .is_err());
}

#[test]
fn meta_two_clients_share_one_authoritative_view() {
    let (addr, _d) = spawn_meta_server();
    let a = connect(&addr, "shared");
    let b = connect(&addr, "shared"); // same view, independent client + socket

    // a creates a subtree
    let id = a
        .create_node(ROOT_INO, "from_a", true, None, 0o755, UID, GID)
        .unwrap();

    // b — a totally different connection — must immediately see it: the server
    // holds the single authoritative engine, so there are no divergent copies.
    let seen = b
        .lookup(ROOT_INO, "from_a")
        .expect("client b must see client a's node");
    assert_eq!(seen.id, id);
    assert!(b.readdir(ROOT_INO).iter().any(|n| n.name == "from_a"));

    // and writes are mutually visible both directions
    a.create_node(id, "nested", false, Some("k"), 0o644, UID, GID)
        .unwrap();
    assert_eq!(b.readdir(id).len(), 1);
}

#[test]
fn meta_per_user_isolation_across_clients() {
    let (addr, _d) = spawn_meta_server();
    let alice = connect(&addr, "alice");
    let bob = connect(&addr, "bob");

    alice
        .create_node(ROOT_INO, "secret", true, None, 0o700, UID, GID)
        .unwrap();
    // Different view -> different engine/DB on the same server -> isolated.
    assert!(bob.lookup(ROOT_INO, "secret").is_none());
    assert!(bob.readdir(ROOT_INO).is_empty());
}

/// A server started with a token must reject any `M_OPEN` presenting a
/// different one, so an unauthorized LAN host can't open arbitrary views' trees.
fn spawn_meta_server_auth(token: &str) -> (String, tempfile::TempDir) {
    let dir = tempfile::Builder::new()
        .prefix("grovefs_msrv_auth_")
        .tempdir()
        .unwrap();
    let base = dir.path().to_path_buf();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let token = token.to_string();
    std::thread::spawn(move || serve_on_meta_auth(listener, base, &token));
    (addr, dir)
}

#[test]
fn meta_server_token_auth_gates_access() {
    let (addr, _d) = spawn_meta_server_auth("s3cr3t");

    // Correct token: connects and can build a view (retry until server accepts).
    let good = (0..100)
        .find_map(
            |_| match RemoteMeta::new_with_token(&addr, "alice", "s3cr3t") {
                Ok(c) => Some(c),
                Err(_) => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    None
                }
            },
        )
        .expect("correct token should connect once the server is up");
    good.create_node(ROOT_INO, "private", true, None, 0o700, UID, GID)
        .unwrap();

    // Server is now definitely accepting. Empty and wrong tokens are rejected at
    // the handshake with PermissionDenied (fail-fast, no retry masking it).
    let e = RemoteMeta::new_with_token(&addr, "alice", "")
        .err()
        .expect("empty token must be rejected");
    assert_eq!(
        e.kind(),
        std::io::ErrorKind::PermissionDenied,
        "empty token: {e:?}"
    );
    let e = RemoteMeta::new_with_token(&addr, "alice", "guessed")
        .err()
        .expect("wrong token must be rejected");
    assert_eq!(
        e.kind(),
        std::io::ErrorKind::PermissionDenied,
        "wrong token: {e:?}"
    );

    // The victim's tree really was created centrally and remains readable only to
    // a client that presents the right token.
    let again = RemoteMeta::new_with_token(&addr, "alice", "s3cr3t").unwrap();
    assert!(again.lookup(ROOT_INO, "private").is_some());
}

/// The invalidation event bus: a dedicated subscriber socket must be pushed an
/// EV_ENTRY for structural changes and an EV_INODE for content/attr changes,
/// driven purely by another client's mutations of the same view.
#[test]
fn meta_event_bus_pushes_invalidations() {
    let (addr, _d) = spawn_meta_server();
    let mut sub = (0..100)
        .find_map(|_| match open_event_stream(&addr, "carol", "", 0) {
            Ok(s) => Some(s),
            Err(_) => {
                std::thread::sleep(std::time::Duration::from_millis(20));
                None
            }
        })
        .expect("subscriber should connect once the server is up");
    // Never hang a test on a missing event: bounded read, so a dropped push
    // surfaces as an assertion error instead of a stalled suite.
    sub.set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();

    let m = connect(&addr, "carol"); // separate work connection, same view

    // create -> EV_ENTRY(ROOT_INO, "docs")
    let dir = m
        .create_node(ROOT_INO, "docs", true, None, 0o755, UID, GID)
        .unwrap();
    let (kind, val, data) = proto::recv_response(&mut sub).unwrap();
    assert_eq!(kind, 0, "EV_ENTRY expected");
    assert_eq!(val as i64, ROOT_INO);
    let mut i = 0usize;
    assert_eq!(proto::get_key(&data, &mut i).unwrap(), "docs");

    // touch_mtime (the write path's signal) -> EV_INODE(dir) AND EV_ENTRY, because
    // an inode-only invalidation can leave a peer serving a read truncated to the
    // OLD cached size; the paired entry drop forces a re-lookup with live attrs.
    m.touch_mtime(dir);
    let (kind, val, _) = proto::recv_response(&mut sub).unwrap();
    assert_eq!(kind, 1, "EV_INODE expected");
    assert_eq!(val as i64, dir);
    let (kind, val, data) = proto::recv_response(&mut sub).unwrap();
    assert_eq!(kind, 0, "paired EV_ENTRY expected after EV_INODE");
    assert_eq!(val as i64, ROOT_INO);
    let mut i = 0usize;
    assert_eq!(proto::get_key(&data, &mut i).unwrap(), "docs");

    // rename -> two EV_ENTRY frames (old name then new name)
    m.rename(ROOT_INO, "docs", ROOT_INO, "docs2", false)
        .unwrap();
    for expected in ["docs", "docs2"] {
        let (kind, val, data) = proto::recv_response(&mut sub).unwrap();
        assert_eq!(kind, 0);
        assert_eq!(val as i64, ROOT_INO);
        let mut i = 0usize;
        assert_eq!(proto::get_key(&data, &mut i).unwrap(), expected);
    }
}

/// S3 zero-provisioning contract: mounting a view the server has never seen
/// auto-creates its authoritative DB on the M_OPEN handshake — deployments do
/// not run `init` per user up front. The created tree is immediately writable
/// and isolated (a second unknown view gets its own fresh root).
#[test]
fn mount_of_unknown_view_auto_creates_the_db() {
    let (addr, d) = spawn_meta_server();
    let base = d.path().to_path_buf();
    assert!(
        !base.join("meta/alice.db").exists(),
        "precondition: no such view yet"
    );

    let m = connect(&addr, "alice"); // handshake alone
    assert!(
        base.join("meta/alice.db").exists(),
        "M_OPEN for an unknown view must lazily create <base>/meta/<view>.db"
    );

    // The fresh view is a real, writable, isolated tree.
    let dir = m
        .create_node(ROOT_INO, "from-first-write", true, None, 0o755, UID, GID)
        .unwrap();
    assert_eq!(m.lookup(ROOT_INO, "from-first-write").unwrap().id, dir);

    let m2 = connect(&addr, "bob");
    assert!(
        m2.lookup(ROOT_INO, "from-first-write").is_none(),
        "views stay isolated"
    );
    assert!(base.join("meta/bob.db").exists());
}

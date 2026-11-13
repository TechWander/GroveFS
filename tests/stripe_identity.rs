//! End-to-end proof that `--member-id` striping is order-independent OVER THE
//! REAL TRANSPORT: two `DataServer`s advertise distinct member ids in their
//! `OP_OPEN` acks, a `RemoteBackend` surfaces each as `placement_key()`, and a
//! `ShardedBackend` sorted by that identity reads back a striped object
//! correctly no matter the order its `--data-servers` list is fed in — the
//! hazard a positional stripe silently corrupts. Also pins that the fingerprint
//! refuses a genuine member-set change and that an un-`--member-id` server
//! advertises no identity. No FUSE involved.

use std::net::TcpListener;
use std::sync::Arc;

use grovefs::backend::{self, sharded::ShardedBackend, StorageBackend};
use grovefs::remote::{
    proto,
    server::{serve_on, serve_on_member},
    RemoteBackend,
};

/// Bind an ephemeral listener, serve `backend` on it (advertising `member_id`
/// when `Some`), and leak the temp dir for the process lifetime. Returns the
/// client-side address string.
fn spawn(dir: tempfile::TempDir, member_id: Option<u64>) -> String {
    let backend: Arc<dyn StorageBackend> = Arc::from(backend::open_local(dir.path()).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || match member_id {
        Some(id) => serve_on_member(listener, backend, id),
        None => serve_on(listener, backend),
    });
    std::mem::forget(dir); // keep the data root alive
    addr
}

fn client(addr: &str) -> Arc<RemoteBackend> {
    for _ in 0..100 {
        if let Ok(c) = RemoteBackend::new(addr) {
            return Arc::new(c);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!("no server at {addr}");
}

fn member(id: u64) -> String {
    let dir = tempfile::Builder::new()
        .prefix("grovefs_id_")
        .tempdir()
        .unwrap();
    spawn(dir, Some(id))
}

#[test]
fn remote_advertises_member_id_and_positional_server_advertises_none() {
    let with_id = member(7);
    let c = client(&with_id);
    assert_eq!(c.placement_key(), Some(7), "server --member-id 7 must surface");

    let plain = {
        let dir = tempfile::Builder::new()
            .prefix("grovefs_plain_")
            .tempdir()
            .unwrap();
        spawn(dir, None)
    };
    assert_eq!(
        client(&plain).placement_key(),
        None,
        "a server without --member-id advertises no identity (NO_MEMBER_ID ack)"
    );
}

#[test]
fn reordered_identity_data_servers_read_a_striped_object_identically() {
    // Two members with ids 0 and 1, each a distinct data root. A 10 KiB object
    // over a 4 KiB stripe spans chunks 0,1,2 → every member is touched.
    let a = member(0);
    let b = member(1);
    let stripe = 4096u64;
    let data: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();

    // Write through the canonical id order (a=id0, b=id1).
    let canonical: Vec<Arc<dyn StorageBackend>> =
        vec![client(&a) as Arc<dyn StorageBackend>, client(&b) as Arc<dyn StorageBackend>];
    let sb = ShardedBackend::new(canonical, stripe);
    let key = sb.new_key("obj");
    assert_eq!(
        key.split(':').count(),
        4,
        "identity-pinned key carries the fingerprint: {key}"
    );
    sb.create(&key).unwrap();
    assert_eq!(sb.write(&key, 0, &data).unwrap(), data.len());

    // Reopen the SAME two servers but feed them in the REVERSED list order —
    // the misconfiguration that silently corrupts a positional stripe. Sorting
    // by id restores canonical placement, so reads are identical and no error.
    let flipped: Vec<Arc<dyn StorageBackend>> =
        vec![client(&b) as Arc<dyn StorageBackend>, client(&a) as Arc<dyn StorageBackend>];
    let sb2 = ShardedBackend::new(flipped, stripe);
    assert_eq!(sb2.len(&key).unwrap(), data.len() as u64);
    assert_eq!(
        sb2.read(&key, 0, data.len()).unwrap(),
        data,
        "reordered identity stripe must read exact bytes"
    );
    // Order never changes the fingerprint, so a fresh key matches either order.
    assert_eq!(sb2.new_key("x"), sb.new_key("x"));
}

#[test]
fn same_count_different_member_set_is_refused_over_the_wire() {
    // A stripe over ids {0,1}; then a mount claiming {0,2} (same count,
    // different set). Sorting slots onto different physical servers would
    // misroute chunk #1, so the fingerprint check refuses rather than reading.
    let m0 = member(0);
    let m1 = member(1);
    let m2 = member(2);
    let stripe = 4096u64;

    let a: Vec<Arc<dyn StorageBackend>> = vec![
        client(&m0) as Arc<dyn StorageBackend>,
        client(&m1) as Arc<dyn StorageBackend>,
    ];
    let sb = ShardedBackend::new(a, stripe);
    let key = sb.new_key("obj");
    sb.create(&key).unwrap();
    let data: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
    sb.write(&key, 0, &data).unwrap();

    let b: Vec<Arc<dyn StorageBackend>> = vec![
        client(&m0) as Arc<dyn StorageBackend>,
        client(&m2) as Arc<dyn StorageBackend>,
    ];
    let sb2 = ShardedBackend::new(b, stripe);
    let err = sb2
        .len(&key)
        .expect_err("a same-count different member set must not remap");
    assert!(err.to_string().contains("--member-id set"), "{err}");
}

/// Sanity: the sentinel constant both halves share is what an unconfigured
/// server echoes, and a client maps it to "no identity".
#[test]
fn no_member_id_sentinel_is_max() {
    assert_eq!(proto::NO_MEMBER_ID, u64::MAX);
}

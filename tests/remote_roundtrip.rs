//! End-to-end test of the remote data path over a real TCP socket on
//! localhost: a [`DataServer`] backed by a temp local dir, driven by a
//! [`RemoteBackend`] client. Exercises the full request/response pipeline
//! (framing + dispatch + on-disk object IO) without mounting FUSE.

use std::net::TcpListener;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use grovefs::backend::{self, StorageBackend};
use grovefs::remote::{
    server::{serve_on, serve_on_auth},
    RemoteBackend,
};

fn spawn_server() -> (String, tempfile::TempDir) {
    let dir = tempfile::Builder::new()
        .prefix("grovefs_rsrv_")
        .tempdir()
        .unwrap();
    let backend: Arc<dyn StorageBackend> = Arc::from(backend::open_local(dir.path()).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || serve_on(listener, backend));
    (addr, dir)
}

fn make_client(addr: &str) -> RemoteBackend {
    // Retry a moment: the server thread may not have started accepting yet.
    for _ in 0..50 {
        if let Ok(c) = RemoteBackend::new(addr) {
            return c;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!("could not connect to test data-server at {addr}");
}

#[test]
fn remote_write_read_len_fsync_remove() {
    let (addr, _dir) = spawn_server();
    let c = make_client(&addr);

    let key = "obj-1";
    c.create(key).unwrap();
    let payload = b"the quick brown fox jumps over the lazy dog";
    let n = c.write(key, 0, payload).unwrap();
    assert_eq!(n, payload.len());
    assert_eq!(c.len(key).unwrap(), payload.len() as u64);

    let back = c.read(key, 0, 4096).unwrap();
    assert_eq!(back, payload);

    // Positional read of a slice.
    let slice = c.read(key, 4, 5).unwrap();
    assert_eq!(slice, b"quick");

    c.fsync(key).unwrap();
    c.remove(key).unwrap();
    // After removal, length is no longer retrievable (ENOENT).
    assert!(c.len(key).is_err());
}

#[test]
fn remote_truncate_and_pwrite_gap() {
    let (addr, _dir) = spawn_server();
    let c = make_client(&addr);

    let key = "obj-2";
    c.create(key).unwrap();
    // Write past EOF leaves a zero hole.
    c.write(key, 8, b"abcd").unwrap();
    assert_eq!(c.len(key).unwrap(), 12);
    let data = c.read(key, 0, 12).unwrap();
    assert_eq!(&data[..8], &[0u8; 8]);
    assert_eq!(&data[8..], b"abcd");

    c.truncate(key, 3).unwrap();
    assert_eq!(c.len(key).unwrap(), 3);
}

#[test]
fn remote_statfs_reports_blocks() {
    let (addr, _dir) = spawn_server();
    let c = make_client(&addr);

    let s = c.statvfs().unwrap();
    // Sanity: real filesystem reports a nonzero block size and total.
    assert!(s.bsize > 0, "bsize should be nonzero");
    assert!(s.tblocks > 0, "tblocks should be nonzero");
}

#[test]
fn remote_concurrent_writes_no_corruption() {
    // Many threads sharing the client's connection pool must not interleave
    // frames on a socket. Each writes to its own key and reads it back.
    let (addr, _dir) = spawn_server();
    let c = Arc::new(make_client(&addr));

    let mut handles = Vec::new();
    for t in 0..16u32 {
        let c = c.clone();
        handles.push(std::thread::spawn(move || {
            let key = format!("mt-{t}");
            let block = [t as u8; 1024];
            c.create(&key).unwrap();
            for i in 0..8 {
                c.write(&key, i * 1024, &block).unwrap();
            }
            assert_eq!(c.len(&key).unwrap(), 8 * 1024);
            let back = c.read(&key, 0, 8 * 1024).unwrap();
            assert!(back.iter().all(|&b| b == t as u8), "mixed frames");
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
}

/// P3: reads >= 1MiB take the chunked parallel path. Byte-exact agreement with
/// a deterministic pattern across slice boundaries (and at a NON-aligned
/// offset, so slices straddle 1MiB borders) proves the stitching is in order.
#[test]
fn remote_chunked_read_stitches_in_order() {
    let (addr, _dir) = spawn_server();
    let c = make_client(&addr);

    let key = "big-read";
    c.create(key).unwrap();
    // 5 MiB of position-dependent pattern, written in 1MiB slices.
    let size = 5usize << 20;
    let data: Vec<u8> = (0..size)
        .map(|i| (i.wrapping_mul(31) % 251) as u8)
        .collect();
    let mut sent = 0usize;
    while sent < size {
        let n = (size - sent).min(1 << 20);
        c.write(key, sent as u64, &data[sent..sent + n]).unwrap();
        sent += n;
    }
    assert_eq!(c.len(key).unwrap(), size as u64);

    // Full chunked read from 0.
    assert_eq!(c.read(key, 0, size).unwrap(), data);
    // Unaligned start crossing slice borders: offset 1MiB-1000, len 3MiB+2000
    // (spans multiple READ_CHUNK slices with ragged head/tail).
    let off = (1usize << 20) - 1000;
    let len = 3 * (1usize << 20) + 2000;
    assert_eq!(c.read(key, off as u64, len).unwrap(), data[off..off + len]);
}

/// P3: a chunked read reaching past EOF zero-pads exactly like the single-
/// request path (each slice documents this contract; a short middle slice
/// must not shift later slices in the stitched result).
#[test]
fn remote_chunked_read_past_eof_zero_pads() {
    let (addr, _dir) = spawn_server();
    let c = make_client(&addr);

    let key = "big-eof";
    c.create(key).unwrap();
    c.write(key, 0, &[7u8; 3 << 20]).unwrap(); // 3 MiB object
    let back = c.read(key, 0, 4 << 20).unwrap(); // ask 4 MiB
    assert_eq!(back.len(), 4 << 20, "chunked read must return exactly len");
    assert!(back[..3 << 20].iter().all(|&b| b == 7));
    assert!(
        back[3 << 20..].iter().all(|&b| b == 0),
        "past-EOF must be zeros"
    );
}

/// The chunked read now lands every slice DIRECTLY in its final buffer slot.
/// It must still agree byte for byte with reading the same range in pieces
/// through the single-request path: identical slice boundaries, identical
/// short-read padding, no reordering, at aligned and unaligned offsets.
#[test]
fn remote_chunked_read_matches_piecewise_single_reads() {
    let (addr, _dir) = spawn_server();
    let c = make_client(&addr);

    let key = "chunked-equiv";
    c.create(key).unwrap();
    // Ragged size on purpose: the last chunk is neither full nor zero-length.
    let size = 5 * (1usize << 20) + 1234;
    let data: Vec<u8> = (0..size).map(|i| (i % 253) as u8).collect();
    c.write(key, 0, &data).unwrap();

    // (offset, len): aligned full read, unaligned straddle, and a single-slice-
    // plus-tail read, all inside the object.
    for &(off, len) in &[
        (0usize, size),
        (777usize, 3 * (1usize << 20)),
        (1usize << 20, 2 * (1usize << 20) + 7),
    ] {
        let chunked = c.read(key, off as u64, len).unwrap();
        // 256 KiB steps stay below CHUNKED_READ_MIN, so each read is one
        // ordinary request/response with its own short-read handling.
        let mut piecewise = Vec::with_capacity(len);
        let mut done = 0usize;
        while done < len {
            let n = (len - done).min(256 << 10);
            piecewise.extend_from_slice(&c.read(key, (off + done) as u64, n).unwrap());
            done += n;
        }
        assert_eq!(chunked.len(), len, "chunked read len mismatch at {off}+{len}");
        assert_eq!(chunked, piecewise, "chunked != piecewise at {off}+{len}");
        assert_eq!(chunked, data[off..off + len], "wrong bytes at {off}+{len}");
    }
}

/// A failed slice fails the whole chunked read: no zero-filled "success" and no
/// partial buffer leaking out. ENOENT must surface like the single-read path.
#[test]
fn remote_chunked_read_error_is_not_a_zero_filled_success() {
    let (addr, _dir) = spawn_server();
    let c = make_client(&addr);

    let e = c.read("no-such-object", 0, 2 << 20).unwrap_err();
    assert_eq!(
        e.raw_os_error(),
        Some(libc::ENOENT),
        "chunked read of a missing key must report ENOENT"
    );
}

/// P3: writes >= 4MiB take the parallel chunked path; content must land
/// byte-exact (server does positional pwrite per connection, so slices at
/// independent offsets can't interleave).
#[test]
fn remote_chunked_write_roundtrip() {
    let (addr, _dir) = spawn_server();
    let c = make_client(&addr);

    let key = "big-write";
    c.create(key).unwrap();
    let payload: Vec<u8> = (0..(10usize << 20)).map(|i| (i % 256) as u8).collect(); // 10MiB
    let n = c.write(key, 1024, &payload).unwrap(); // unaligned base offset
    assert_eq!(n, payload.len());
    assert_eq!(c.len(key).unwrap(), 1024 + payload.len() as u64);
    // Spot-check each 4MiB slice border through the small-read path.
    for (i, chunk) in payload.chunks(4 << 20).enumerate() {
        let got = c
            .read(key, 1024 + (i as u64) * (4 << 20), chunk.len() - 1)
            .unwrap();
        assert_eq!(got, &chunk[..chunk.len() - 1], "slice {i} border");
    }
}

/// A data-server started with a token must reject any `OP_OPEN` presenting a
/// different one, so an unauthorized LAN host can't read or write arbitrary
/// objects by key.
fn spawn_server_auth(token: &str) -> (String, tempfile::TempDir) {
    let dir = tempfile::Builder::new()
        .prefix("grovefs_rsrv_auth_")
        .tempdir()
        .unwrap();
    let backend: Arc<dyn StorageBackend> = Arc::from(backend::open_local(dir.path()).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let token = token.to_string();
    std::thread::spawn(move || {
        let fence = Arc::new(grovefs::remote::server::Fence::new_in_memory());
        serve_on_auth(listener, backend, &token, fence)
    });
    (addr, dir)
}

#[test]
fn data_server_token_auth_gates_access() {
    let (addr, _d) = spawn_server_auth("s3cr3t");

    // Correct token: connects and performs data ops (retry until server accepts).
    let good = (0..50)
        .find_map(|_| match RemoteBackend::new_with_token(&addr, "s3cr3t") {
            Ok(c) => Some(c),
            Err(_) => {
                std::thread::sleep(std::time::Duration::from_millis(20));
                None
            }
        })
        .expect("correct token should connect once the server is up");
    good.create("obj-a").unwrap();
    good.write("obj-a", 0, b"secret-bytes").unwrap();
    assert_eq!(good.read("obj-a", 0, 16).unwrap(), b"secret-bytes");

    // Server is definitely accepting now. Empty and wrong tokens are rejected at
    // the OP_OPEN handshake with PermissionDenied (fail-fast, no retry masking).
    let e = RemoteBackend::new_with_token(&addr, "")
        .err()
        .expect("empty token must be rejected");
    assert_eq!(
        e.kind(),
        std::io::ErrorKind::PermissionDenied,
        "empty token: {e:?}"
    );
    let e = RemoteBackend::new_with_token(&addr, "guessed")
        .err()
        .expect("wrong token must be rejected");
    assert_eq!(
        e.kind(),
        std::io::ErrorKind::PermissionDenied,
        "wrong token: {e:?}"
    );

    // The victim's data really landed centrally and remains readable only to a
    // client that presents the right token.
    let again = RemoteBackend::new_with_token(&addr, "s3cr3t").unwrap();
    assert_eq!(again.read("obj-a", 0, 16).unwrap(), b"secret-bytes");
}

/// Spin up a data-server whose witness [`Fence`] the test holds a shared handle
/// to, so it can raise the fencing token out-of-band (simulating a peer node's
/// promotion) while a client drives the data plane over a real socket.
fn spawn_fenced_server() -> (
    String,
    Arc<grovefs::remote::server::Fence>,
    tempfile::TempDir,
) {
    let dir = tempfile::Builder::new()
        .prefix("grovefs_fsrv_")
        .tempdir()
        .unwrap();
    let backend: Arc<dyn StorageBackend> = Arc::from(backend::open_local(dir.path()).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let fence = Arc::new(grovefs::remote::server::Fence::new_in_memory());
    let f = fence.clone();
    std::thread::spawn(move || serve_on_auth(listener, backend, "", f));
    (addr, fence, dir)
}

/// The core guarantee of data-plane fencing: a write-intent op whose stamped
/// epoch lags the witness token is refused outright (EROFS), so a client still
/// aligned with a since-fenced stale primary cannot slip content bytes past the
/// meta-plane gate. Reads are never gated; writes resume once the epoch catches up.
#[test]
fn stale_epoch_write_is_fenced_then_allowed_when_token_caught_up() {
    let (addr, fence, _dir) = spawn_fenced_server();
    // Retry until the server is accepting (the accept thread may lag).
    let epoch = Arc::new(AtomicU64::new(10));
    let eh = epoch.clone();
    let refresh = Arc::new(move || eh.load(Ordering::SeqCst));
    let c = (0..50)
        .find_map(|_| match RemoteBackend::new(&addr) {
            Ok(b) => Some(b.with_fence(epoch.clone(), refresh.clone())),
            Err(_) => {
                std::thread::sleep(std::time::Duration::from_millis(20));
                None
            }
        })
        .expect("fenced data-server should accept");

    // Witness grants a peer node epoch 10 -> token is now 10. The client already
    // holds a live epoch of 10, so create + write sail through the gate.
    assert_eq!(fence.claim(10), 0);
    c.create("obj").unwrap();
    assert_eq!(c.write("obj", 0, b"aligned").unwrap(), 7);

    // Now the client's view goes stale (it kept a superseded epoch after losing
    // the meta connection). Its refresher returns the same stale value, so the
    // one-shot retry cannot recover -> the content write is refused with EROFS...
    epoch.store(4, Ordering::SeqCst);
    let e = c.write("obj", 0, b"stale!").unwrap_err();
    assert_eq!(
        e.raw_os_error(),
        Some(libc::EROFS),
        "stale write must be fenced"
    );
    // ...and the bytes truly did NOT land: read-back still yields the aligned data.
    assert_eq!(c.read("obj", 0, 16).unwrap(), b"aligned");

    // Reads are never gated, even with a stale epoch.
    assert_eq!(&c.read("obj", 0, 7).unwrap(), b"aligned");

    // Once the client re-learns an epoch >= the token, writes flow again.
    epoch.store(11, Ordering::SeqCst);
    assert_eq!(c.write("obj", 0, b"rewritten").unwrap(), 9);
    assert_eq!(c.read("obj", 0, 16).unwrap(), b"rewritten");
}

/// The self-healing path: a client whose stamped epoch is stale, but whose
/// refresher can pull a current epoch from the authority, is transparently
/// recovered — the first attempt is fenced, the refresh + single retry succeed,
/// and the fresh epoch is folded back into the shared handle for later ops.
#[test]
fn fenced_write_refreshes_epoch_and_retries_once() {
    let (addr, fence, _dir) = spawn_fenced_server();
    let epoch = Arc::new(AtomicU64::new(5)); // starts stale
    let refresh = Arc::new(|| 10u64); // authority says the live epoch is 10
    let c = (0..50)
        .find_map(|_| match RemoteBackend::new(&addr) {
            Ok(b) => Some(b.with_fence(epoch.clone(), refresh.clone())),
            Err(_) => {
                std::thread::sleep(std::time::Duration::from_millis(20));
                None
            }
        })
        .expect("fenced data-server should accept");

    fence.claim(10); // token = 10 > client's initial 5

    // `create` is itself a gated op: epoch 5 -> EROFS -> refresh to 10 -> retry ok.
    c.create("obj").unwrap();
    // The retry folded the fresh epoch back into the shared handle.
    assert_eq!(epoch.load(Ordering::SeqCst), 10);
    // Subsequent write reuses the refreshed handle, passing the gate first try.
    assert_eq!(c.write("obj", 0, b"recovered").unwrap(), 9);
    assert_eq!(c.read("obj", 0, 16).unwrap(), b"recovered");
}

/// OP_LIST: the data-server enumerates what it HOLDS, in pages, so an auditor
/// on another machine never has to be pointed at the disks it is judging. The
/// cursor is the server's own, and a full walk must cover every object exactly
/// once with its authoritative size — the two facts `gc` deletes bytes on.
#[test]
fn remote_list_walks_every_object_in_pages() {
    let (addr, _dir) = spawn_server();
    let c = make_client(&addr);
    let mut want = std::collections::BTreeMap::new();
    for i in 0..40u32 {
        let key = format!("obj-{i:04}");
        let body = vec![b'x'; (i as usize % 5) + 1];
        c.create(&key).unwrap();
        c.write(&key, 0, &body).unwrap();
        want.insert(key, body.len() as u64);
    }
    // Page sizes that cut inside a bucket, exactly at the end, and past it.
    for page_size in [1usize, 3, 39, 40, 41, 1000] {
        let mut seen = std::collections::BTreeMap::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = c.list_objects(cursor.as_deref(), page_size).unwrap();
            for r in page.rows {
                assert!(
                    seen.insert(r.key.clone(), r.size).is_none(),
                    "{} listed twice (page size {page_size})",
                    r.key
                );
            }
            match page.next {
                Some(n) => cursor = Some(n),
                None => break,
            }
        }
        assert_eq!(seen, want, "page size {page_size} did not cover the server");
    }
}

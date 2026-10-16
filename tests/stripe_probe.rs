//! DIAGNOSTIC (ignored by default): isolates whether the ShardedBackend read
//! path costs throughput vs a plain RemoteBackend, with NO FUSE and NO client
//! page cache in the way. Run:  cargo test --release -- --ignored --nocapture
//! Read pattern emulates FUSE: sequential reads of `R` bytes over the object.

use std::net::TcpListener;
use std::sync::Arc;
use std::time::Instant;

use grovefs::backend::{self, sharded::ShardedBackend, StorageBackend};
use grovefs::remote::{server::serve_on, RemoteBackend};

fn spawn_server() -> String {
    let dir = tempfile::Builder::new().prefix("grovefs_tp_").tempdir().unwrap();
    let backend: Arc<dyn StorageBackend> = Arc::from(backend::open_local(dir.path()).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || serve_on(listener, backend));
    std::mem::forget(dir); // keep the temp dir alive for the process lifetime
    addr
}

fn client(addr: &str) -> RemoteBackend {
    for _ in 0..50 {
        if let Ok(c) = RemoteBackend::new(addr) {
            return c;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!("no server");
}

#[test]
#[ignore]
fn stripe_read_throughput_probe() {
    let mb = 128usize;
    let total = mb * 1024 * 1024;
    let payload = vec![7u8; total];

    let a = spawn_server();
    let b = spawn_server();

    // --- single server baseline ---
    let sa = client(&a);
    sa.create("obj").unwrap();
    sa.write("obj", 0, &payload).unwrap();

    // --- two-server striped ---
    let members: Vec<Arc<dyn StorageBackend>> = vec![
        Arc::new(client(&a)),
        Arc::new(client(&b)),
    ];
    let sb = ShardedBackend::new(members, 16 * 1024 * 1024);
    let key = sb.new_key("striped");
    sb.create(&key).unwrap();
    sb.write(&key, 0, &payload).unwrap();

    for r in [128 * 1024, 1024 * 1024, 4 * 1024 * 1024] {
        let mut off = 0u64;
        let t = Instant::now();
        while (off as usize) < total {
            let want = r.min(total - off as usize);
            let _ = sa.read("obj", off, want).unwrap();
            off += want as u64;
        }
        let single = t.elapsed();

        let mut off = 0u64;
        let t = Instant::now();
        while (off as usize) < total {
            let want = r.min(total - off as usize);
            let _ = sb.read(&key, off, want).unwrap();
            off += want as u64;
        }
        let stripe = t.elapsed();

        let smbps = mb as f64 / single.as_secs_f64();
        let tbps = mb as f64 / stripe.as_secs_f64();
        println!(
            "R={:>5}KiB  single={:6.0} MB/s ({:.3}s)   stripe={:6.0} MB/s ({:.3}s)   ratio={:.2}x",
            r / 1024,
            smbps,
            single.as_secs_f64(),
            tbps,
            stripe.as_secs_f64(),
            tbps / smbps
        );
    }

    // --- concurrency emulating the mount's n_threads worker dispatch ---
    // 4 threads each read a disjoint quarter of the object, concurrently.
    for r in [1024 * 1024] {
        for nw in [1usize, 4] {
            let q = total / nw;
            let t = Instant::now();
            std::thread::scope(|sc| {
                for w in 0..nw {
                    let sa = &sa;
                    sc.spawn(move || {
                        let mut off = (w * q) as u64;
                        let end = (w * q + q) as u64;
                        while off < end {
                            let want = (r as u64).min(end - off) as usize;
                            let _ = sa.read("obj", off, want).unwrap();
                            off += want as u64;
                        }
                    });
                }
            });
            let single = t.elapsed();

            let t = Instant::now();
            std::thread::scope(|sc| {
                for w in 0..nw {
                    let sb = &sb;
                    let key = key.as_str();
                    sc.spawn(move || {
                        let mut off = (w * q) as u64;
                        let end = (w * q + q) as u64;
                        while off < end {
                            let want = (r as u64).min(end - off) as usize;
                            let _ = sb.read(key, off, want).unwrap();
                            off += want as u64;
                        }
                    });
                }
            });
            let stripe = t.elapsed();
            println!(
                "CONC n={nw}  single={:6.0} MB/s   stripe={:6.0} MB/s   ratio={:.2}x",
                mb as f64 / single.as_secs_f64(),
                mb as f64 / stripe.as_secs_f64(),
                single.as_secs_f64() / stripe.as_secs_f64()
            );
        }
    }
}

//! A/B probe for the chunked-read buffer strategy (not a correctness gate —
//! byte-for-byte equivalence lives in `remote_roundtrip.rs`).
//!
//! The same 256 MiB object is read three ways, each in its OWN process so the
//! peak-RSS numbers cannot be polluted by memory an earlier round freed but
//! glibc kept resident:
//!   * `direct`  — today's path: every 1 MiB slice lands in its final slot of
//!     one pre-sized result buffer (`recv_response_into`), no stitching copy.
//!   * `slices`  — per-slice `Vec` (as the old `read_slice` returned), gathered
//!     across 8 workers and then dropped: the old request pattern plus its
//!     per-slice allocation, without the final concatenation.
//!   * `stitch`  — the old path in full: the gathered pieces are concatenated
//!     into a second `len`-byte buffer, i.e. `slices` + one extra copy of the
//!     whole result.
//!
//! The two legacy rounds drive the public `read()`, which re-enters the
//! chunked dispatcher per slice, so their elapsed times are an upper bound on
//! the old internal path; the RSS peaks are the like-for-like part. Run with:
//!
//! ```text
//! cargo test --release --test chunked_read_bench -- --ignored --nocapture
//! ```

use std::net::TcpListener;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use grovefs::backend::{self, StorageBackend};
use grovefs::remote::{server::serve_on, RemoteBackend};

const MIB: usize = 1 << 20;
const SIZE: usize = 256 * MIB;
const SLICE: usize = 1 << 20; // READ_CHUNK
const WORKERS: usize = 8; // MAX_PARALLEL_SLICES
const ROUNDS: [&str; 3] = ["direct", "slices", "stitch"];
const ROUND_ENV: &str = "GROVEFS_BENCH_ROUND";

fn spawn_server() -> (String, tempfile::TempDir) {
    let dir = tempfile::Builder::new()
        .prefix("grovefs_bench_")
        .tempdir()
        .unwrap();
    let backend: Arc<dyn StorageBackend> = Arc::from(backend::open_local(dir.path()).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || serve_on(listener, backend));
    (addr, dir)
}

fn make_client(addr: &str) -> RemoteBackend {
    for _ in 0..50 {
        if let Ok(c) = RemoteBackend::new(addr) {
            return c;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("could not connect to bench data-server at {addr}");
}

/// Current resident set size in KiB.
fn rss_kib() -> usize {
    let s = std::fs::read_to_string("/proc/self/status").unwrap();
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest
                .trim()
                .trim_end_matches("kB")
                .trim()
                .parse()
                .unwrap_or(0);
        }
    }
    0
}

/// Peak RSS reached while `f` runs, sampled every millisecond.
fn peak_rss_during(f: impl FnOnce()) -> (usize, usize) {
    let base = rss_kib();
    let peak = Arc::new(AtomicUsize::new(base));
    let stop = Arc::new(AtomicBool::new(false));
    let mon = {
        let peak = Arc::clone(&peak);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                peak.fetch_max(rss_kib(), Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(1));
            }
        })
    };
    f();
    stop.store(true, Ordering::Relaxed);
    mon.join().unwrap();
    (base, peak.load(Ordering::Relaxed))
}

/// The pre-change algorithm: per-slice `Vec` from a plain `read`, gathered
/// stride-partitioned across workers (the partition `read_chunked` used).
/// With `stitch`, the pieces are then concatenated in offset order — exactly
/// what the old `read_chunked` returned.
fn legacy_read(c: &RemoteBackend, key: &str, len: usize, stitch: bool) -> Vec<u8> {
    let nslices = len.div_ceil(SLICE);
    let nworkers = nslices.min(WORKERS);
    let mut all: Vec<(usize, Vec<u8>)> = std::thread::scope(|sc| {
        let mut hs = Vec::new();
        for w in 0..nworkers {
            hs.push(sc.spawn(move || {
                let mut mine: Vec<(usize, Vec<u8>)> = Vec::new();
                for i in (w..nslices).step_by(nworkers) {
                    let off = (i * SLICE) as u64;
                    let n = (len - i * SLICE).min(SLICE);
                    mine.push((i, c.read(key, off, n).unwrap()));
                }
                mine
            }));
        }
        let mut all: Vec<(usize, Vec<u8>)> = Vec::with_capacity(nslices);
        for h in hs {
            all.extend(h.join().unwrap());
        }
        all
    });
    if !stitch {
        return Vec::new();
    }
    all.sort_by_key(|(i, _)| *i);
    let mut out = Vec::with_capacity(len);
    for (_, d) in all {
        out.extend_from_slice(&d);
    }
    out
}

/// One round in a fresh process: load the pattern, then measure the read.
fn run_round(round: &str) {
    let (addr, _dir) = spawn_server();
    let c = make_client(&addr);

    let key = "bench-obj";
    c.create(key).unwrap();
    // Position-dependent pattern so any reordering shows up in the checksum.
    let chunk: Vec<u8> = (0..MIB).map(|i| (i.wrapping_mul(37) % 251) as u8).collect();
    let four = chunk.repeat(4);
    for off in (0..SIZE).step_by(4 * MIB) {
        c.write(key, off as u64, &four).unwrap();
    }
    assert_eq!(c.len(key).unwrap(), SIZE as u64);

    // Warm the page cache + connection pool so no round pays for cold IO.
    let warm = c.read(key, 0, 16 * MIB).unwrap();
    assert_eq!(warm.len(), 16 * MIB);
    drop(warm);
    std::thread::sleep(Duration::from_millis(300));

    let t0 = Instant::now();
    let mut out = Vec::new();
    let (base, peak) = peak_rss_during(|| {
        out = match round {
            "direct" => c.read(key, 0, SIZE).unwrap(),
            "stitch" => legacy_read(&c, key, SIZE, true),
            _ => legacy_read(&c, key, SIZE, false),
        };
    });
    let dt = t0.elapsed();
    // Cheap order-sensitive checksum, so a mis-stitched buffer can't pass.
    let sum: u64 = out.iter().fold(0u64, |a, &b| a.wrapping_mul(131).wrapping_add(b as u64));
    println!(
        "{round:>7}: {:>7.1} MiB/s  elapsed {:>6.3}s  rss {} -> {} MiB (peak +{} MiB)  len {}  sum {:#x}",
        SIZE as f64 / dt.as_secs_f64() / MIB as f64,
        dt.as_secs_f64(),
        base / 1024,
        peak / 1024,
        (peak - base) / 1024,
        out.len(),
        sum
    );
}

#[test]
#[ignore = "A/B probe: run explicitly with --ignored --nocapture"]
fn chunked_read_direct_vs_stitch_ab() {
    // Child mode: one round, measured in a clean address space.
    if let Ok(round) = std::env::var(ROUND_ENV) {
        run_round(&round);
        return;
    }
    // Parent mode: re-exec this test binary once per round.
    let exe = std::env::current_exe().unwrap();
    let mut sums = Vec::new();
    for round in ROUNDS {
        let out = Command::new(&exe)
            .args([
                "--ignored",
                "--exact",
                "chunked_read_direct_vs_stitch_ab",
                "--nocapture",
            ])
            .env(ROUND_ENV, round)
            .output()
            .expect("failed to spawn the round child");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let line = stdout
            .lines()
            .find(|l| l.trim_start().starts_with(round))
            .unwrap_or_else(|| panic!("{round}: no result line in\n{stdout}{}", String::from_utf8_lossy(&out.stderr)));
        println!("{line}");
        sums.push(
            line.rsplit("sum ")
                .next()
                .unwrap()
                .trim()
                .parse::<String>()
                .unwrap(),
        );
    }
    // Same bytes, same order — the direct path is a pure copy elision. (The
    // slice-only round drops its pieces, so it has no checksum to compare.)
    assert_eq!(sums[0], sums[2], "direct and stitched bytes must match");
}

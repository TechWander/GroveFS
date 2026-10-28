//! S1 fan-out scaling benchmark + invariant proof (opt-in, `#[ignore]` so it
//! never gates the fast unit board — run it explicitly):
//!
//! ```text
//!   cargo test --release -- --ignored --nocapture
//!   GROVEFS_FANOUT_SCALE=10000000 cargo test --release -- --ignored --nocapture
//! ```
//!
//! The design claim this pins down: a single storage root scales to tens of
//! millions of objects because objects fan out over a FIXED 256-bucket table,
//! so (a) every directory stays bounded (~scale/256 entries, no htree blow-up
//! that makes backup/`ls`/`du` hostiles) and (b) path resolution stays O(1)
//! (two joins, no scan) at create, read, AND random-access time. A flat
//! `blocks/<key>` store would put all `scale` entries in ONE directory.
//!
//! We measure real numbers rather than assert them: bulk-create throughput,
//! random-read latency distribution, and the observed max entries in any single
//! bucket directory. The default scale (300k) runs in seconds on a laptop; set
//! `GROVEFS_FANOUT_SCALE` to 1e7 to exercise the actual tens-of-millions target
//! on capable hardware (it needs the matching inodes/space, ~300k objects is a
//! few MB).

use grovefs::backend::local::LocalBackend;
use grovefs::backend::StorageBackend;
use std::time::Instant;

fn scale() -> usize {
    std::env::var("GROVEFS_FANOUT_SCALE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(300_000)
}

fn payload(i: usize) -> Vec<u8> {
    // 64 bytes, unique per object, so a random read can be verified by content.
    let mut v = Vec::with_capacity(64);
    v.extend_from_slice(b"grovefs-fanout-");
    v.extend_from_slice(format!("{i:016x}").as_bytes());
    while v.len() < 64 {
        v.push(b'.');
    }
    v
}

fn key(i: usize) -> String {
    format!("{i:016x}-0000-4000-8000-grovefsfanout")
}

#[test]
#[ignore]
fn fanout_scales_and_keeps_directories_bounded() {
    let n = scale();
    let mut root = std::env::temp_dir();
    let tag = format!("grovefs_fanout_{}", std::process::id());
    root.push(&tag);
    let _ = std::fs::remove_dir_all(&root);
    let be = LocalBackend::new(&root).unwrap();

    // ---- bulk create: throughput + the object really lands in a bucket ----
    let t0 = Instant::now();
    for i in 0..n {
        let k = key(i);
        be.write(&k, 0, &payload(i)).unwrap();
    }
    let create_secs = t0.elapsed().as_secs_f64();
    let mut distinct_buckets = 0usize;
    for b in 0..256u64 {
        let d = root.join("blocks").join(format!("{b:02x}"));
        if std::fs::read_dir(&d)
            .map(|mut r| r.next().is_some())
            .unwrap_or(false)
        {
            distinct_buckets += 1;
        }
    }
    assert_eq!(
        distinct_buckets, 256,
        "objects must spread over every bucket"
    );
    assert_eq!(
        std::fs::read_dir(root.join("blocks"))
            .unwrap()
            .filter(|e| e.as_ref().unwrap().file_type().unwrap().is_file())
            .count(),
        0,
        "no object may sit at the flat blocks root after a fan-out-native create"
    );

    // ---- per-directory bound: the whole point of the fan-out ----
    let mut max_bucket = 0usize;
    for b in 0..256u64 {
        let d = root.join("blocks").join(format!("{b:02x}"));
        let c = std::fs::read_dir(&d).unwrap().count();
        max_bucket = max_bucket.max(c);
    }
    let ideal = n as f64 / 256.0;
    // Uniform hashing at 300k+ keeps the fullest bucket within ~1.5x the mean
    // with overwhelming probability; 3x is a conservative hard bound that still
    // catches any prefix-correlation regression (which would spike one bucket).
    assert!(
        (max_bucket as f64) < ideal * 3.0 + 32.0,
        "fullest bucket has {max_bucket} entries, ideal {ideal:.0} — fan-out not uniform"
    );

    // ---- random read: O(1) path resolution + aggregate throughput ----
    let reads = (n / 4).clamp(20_000, 200_000);
    let mut rng = SplitMix::new(0x9E37_79B9_7F4A_7C15);
    let t1 = Instant::now();
    for _ in 0..reads {
        let i = (rng.next() % n as u64) as usize;
        let k = key(i);
        let got = be.read(&k, 0, 64).unwrap();
        assert_eq!(got, payload(i), "random read of {k} returned wrong bytes");
        // fd cache means this measures the lookup+pread hot path we promise.
    }
    let read_secs = t1.elapsed().as_secs_f64();
    let per_read_ms = read_secs * 1000.0 / reads as f64;

    println!("\n==== grovefs S1 fan-out benchmark ({} objects) ====", n);
    println!(
        "  create : {create_secs:.2}s  ({:.0} objects/sec)",
        n as f64 / create_secs
    );
    println!("  buckets used: 256/256, fullest bucket = {max_bucket} entries (mean {ideal:.0})");
    println!("  random read: {reads} ops in {read_secs:.2}s  ({:.1} ops/sec, {per_read_ms:.4} ms/op avg)",
        reads as f64 / read_secs);
    println!("  a flat single-directory store would hold all {n} entries in ONE dir;");
    println!(
        "  fan-out caps any directory at ~{} entries (n/256), path stays 2 joins deep.",
        max_bucket
    );
    println!("  ======================================================\n");

    // Absolute floor sanity: throughput must be non-degenerate (this is not a
    // perf *target*, just a guard that we are not accidentally measuring a hang).
    assert!(create_secs > 0.0 && read_secs > 0.0);
    assert!(
        reads as f64 / read_secs > 1000.0,
        "random read collapsed to {per_read_ms:.1} ms/op"
    );

    std::mem::drop(be);
    let _ = std::fs::remove_dir_all(&root);
}

/// Tiny deterministic PRNG so the random-read mix is reproducible without
/// pulling in a rand dependency for a benchmark.
struct SplitMix(u64);
impl SplitMix {
    fn new(seed: u64) -> Self {
        Self(seed)
    }
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

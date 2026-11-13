//! Per-mount bandwidth shaping for CONTENT bytes.
//!
//! A `RateLimiter` is a GCRA (leaky-bucket) shaper: every `acquire(n)` books
//! the next `n`-byte slot in a single shared time schedule and the caller
//! sleeps until its slot may be handed to the kernel. Slots are spaced
//! `n / rate` apart and never overlap, so the long-run rate is exactly `rate`
//! bytes/second and bytes are paced from the very first op (no startup burst).
//! One concession keeps huge single ops sane: an op larger than one second of
//! budget may be admitted up to one second before its slot ends, so a single
//! multi-second read/write is not stalled for its whole size.
//!
//! Why a schedule and not a refilling token counter: with a counter, several
//! workers sleeping concurrently each credit their own sleep's refill to the
//! next arrival, and the aggregate sneaks above the cap. A single monotonic
//! schedule booked under one mutex cannot double-count: slots never overlap.
//!
//! Deliberately CLIENT-side and per-mount (`--bw-read` / `--bw-write`), the
//! same layer as `--cache-ttl`: one mount opens exactly one view, so this is
//! the natural "cap this view" granularity. Honest boundary: it is
//! self-discipline, not isolation — a raw data-plane client that skips the
//! FUSE layer, or N mounts of the same view, each get their own budget, and
//! the server cannot attribute object bytes to a view (keys are
//! content-addressed and view-agnostic). Metadata ops are never throttled.
//!
//! Semantics: `0` means unlimited (the default, zero-cost — `acquire` is a
//! branch). The schedule is shared by every worker thread of one mount.

use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct RateLimiter {
    /// Bytes per second; 0 = unlimited.
    rate: u64,
    /// End of the last booked slot. `acquire` starts the next slot here (or
    /// at "now" if the schedule has drained), so slots never overlap.
    next: Mutex<Instant>,
}

impl RateLimiter {
    /// A limiter capped at `bytes_per_sec`; `0` builds an unlimited (inert)
    /// one so call sites need no `Option` branching on the hot path.
    pub fn new(bytes_per_sec: u64) -> Self {
        Self {
            rate: bytes_per_sec,
            next: Mutex::new(Instant::now()),
        }
    }

    pub fn is_unlimited(&self) -> bool {
        self.rate == 0
    }

    /// Book `bytes` in the shared schedule and sleep until an operation that
    /// size may be admitted: no earlier than the end of the previous slot, and
    /// for an op bigger than one second of budget no earlier than one second
    /// before its own slot ends (see the module docs). Called on the FUSE
    /// worker that is about to reply, so only that worker blocks.
    pub fn acquire(&self, bytes: u64) {
        if self.rate == 0 || bytes == 0 {
            return;
        }
        let slot = Duration::from_secs_f64(bytes as f64 / self.rate as f64);
        let now = Instant::now();
        let admit = {
            let mut next = self.next.lock().unwrap_or_else(|e| e.into_inner());
            let begin = if *next > now { *next } else { now };
            *next = begin + slot;
            // Pay the slot in full; a huge op may start up to 1 s early.
            let paced = begin + slot - Duration::from_secs(1);
            if begin > paced {
                begin
            } else {
                paced
            }
        };
        if let Some(wait) = admit.checked_duration_since(now) {
            if !wait.is_zero() {
                std::thread::sleep(wait);
            }
        }
    }
}

/// Parse a `--bw-read`/`--bw-write` rate: a byte count with an optional
/// `K`/`M`/`G` suffix (1024-based, case-insensitive), e.g. `500000`, `50M`,
/// `1G`. `0` disables the cap. Anything else (decimals, unknown suffixes,
/// overflow) is a hard error — a silently ignored cap is worse than a refusal.
pub fn parse_rate(s: &str) -> Result<u64, String> {
    let t = s.trim();
    if t.is_empty() {
        return Err("empty rate (expected bytes, or a K/M/G suffix — e.g. 50M)".to_string());
    }
    let (digits, mult) = match t.as_bytes()[t.len() - 1] {
        b'k' | b'K' => (&t[..t.len() - 1], 1u64 << 10),
        b'm' | b'M' => (&t[..t.len() - 1], 1u64 << 20),
        b'g' | b'G' => (&t[..t.len() - 1], 1u64 << 30),
        _ => (t, 1u64),
    };
    let n: u64 = digits.trim().parse().map_err(|_| {
        format!("invalid rate '{s}' (expected bytes, or a K/M/G suffix — e.g. 50M)")
    })?;
    n.checked_mul(mult)
        .ok_or_else(|| format!("rate '{s}' overflows u64 bytes/second"))
}

/// Parse a plain SIZE in bytes (not a rate): a byte count with an optional
/// `K`/`M`/`G` suffix (1024-based, case-insensitive), e.g. `16777216`, `16M`,
/// `1G`. Unlike [`parse_rate`] `0` is REFUSED — a zero stripe is meaningless
/// where a zero rate means "unlimited". Anything else malformed (decimals,
/// unknown suffixes, overflow) is a hard error, because a silently ignored
/// operator-supplied size is worse than a refusal.
pub fn parse_size_bytes(s: &str) -> Result<u64, String> {
    let t = s.trim();
    if t.is_empty() {
        return Err("empty size (expected bytes, or a K/M/G suffix — e.g. 16M)".to_string());
    }
    let (digits, mult) = match t.as_bytes()[t.len() - 1] {
        b'k' | b'K' => (&t[..t.len() - 1], 1u64 << 10),
        b'm' | b'M' => (&t[..t.len() - 1], 1u64 << 20),
        b'g' | b'G' => (&t[..t.len() - 1], 1u64 << 30),
        _ => (t, 1u64),
    };
    let n: u64 = digits.trim().parse().map_err(|_| {
        format!("invalid size '{s}' (expected bytes, or a K/M/G suffix — e.g. 16M)")
    })?;
    if n == 0 {
        return Err(format!("size '{s}' is zero (must be at least 1 byte)"));
    }
    n.checked_mul(mult)
        .ok_or_else(|| format!("size '{s}' overflows u64 bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rate_accepts_plain_and_suffixed_forms() {
        assert_eq!(parse_rate("0"), Ok(0));
        assert_eq!(parse_rate("1048576"), Ok(1 << 20));
        assert_eq!(parse_rate("512k"), Ok(512 << 10));
        assert_eq!(parse_rate("50M"), Ok(50 << 20));
        assert_eq!(parse_rate("2g"), Ok(2 << 30));
        assert_eq!(parse_rate(" 8M "), Ok(8 << 20));
    }

    #[test]
    fn parse_rate_refuses_garbage_and_overflow() {
        for bad in ["", "abc", "12Z", "1.5M", "-4", "M", "99999999999999999999G"] {
            assert!(parse_rate(bad).is_err(), "{bad:?} should be refused");
        }
    }

    #[test]
    fn parse_size_bytes_accepts_plain_and_suffixed_forms() {
        assert_eq!(parse_size_bytes("16777216"), Ok(16 << 20));
        assert_eq!(parse_size_bytes("512k"), Ok(512 << 10));
        assert_eq!(parse_size_bytes("16M"), Ok(16 << 20));
        assert_eq!(parse_size_bytes("2g"), Ok(2 << 30));
        assert_eq!(parse_size_bytes(" 4M "), Ok(4 << 20));
        assert_eq!(parse_size_bytes("1"), Ok(1));
    }

    #[test]
    fn parse_size_bytes_refuses_zero_garbage_and_overflow() {
        // `0` is refused here but valid for parse_rate (where it means "off").
        for bad in ["", "0", "0M", "abc", "12Z", "1.5M", "-4", "M", "99999999999999999999G"] {
            assert!(parse_size_bytes(bad).is_err(), "{bad:?} should be refused");
        }
    }

    #[test]
    fn unlimited_limiter_never_sleeps() {
        let l = RateLimiter::new(0);
        assert!(l.is_unlimited());
        let t = Instant::now();
        l.acquire(1 << 30); // a whole GiB against an inert limiter
        assert!(t.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn a_capped_limiter_paces_slots() {
        // 1 MiB/s: the first MiB fills its whole one-second slot and is
        // admitted at once; the next half-MiB waits for the schedule to reach
        // its slot end. Check both bounds: never instant, never overslept.
        let l = RateLimiter::new(1 << 20);
        let t = Instant::now();
        l.acquire(1 << 20);
        assert!(
            t.elapsed() < Duration::from_millis(50),
            "first slot is admitted now"
        );
        l.acquire(1 << 19);
        let e = t.elapsed();
        assert!(e >= Duration::from_millis(900), "{e:?} too short");
        assert!(e <= Duration::from_millis(1600), "{e:?} too long");
    }
}

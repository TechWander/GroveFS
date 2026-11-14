//! Deterministic cross-server striping backend (data plane, no meta change).
//!
//! A file's bytes are spread round-robin over `N` member backends in fixed
//! `STRIPE_BYTES` chunks: logical chunk `i` lives on member slot `i % N` under
//! the object key `{base}#{i}`. When every member advertises a stable identity
//! (a remote data-server started with `--member-id`), the slots are assigned by
//! SORTING those ids, so the order of the caller's `--data-servers` list no
//! longer decides placement — reordering it cannot misroute a chunk. The layout
//! is encoded into the durable `data_ref` key at create time — `s<N>:<stripe>:<fp>:<uuid>`
//! for an identity-pinned stripe (where `<fp>` fingerprints the exact member-id
//! set) and the historical `s<N>:<stripe>:<uuid>` for a positional one — so
//! reads are self-describing and an unprefixed key (a single-object write,
//! striping off) still resolves to slot 0. The metadata schema, `MetaProvider`
//! and the wire protocol are all untouched.
//!
//! Concurrency: a large read/write that spans several chunks fans its chunk
//! operations out with `std::thread::scope`, so aggregate bandwidth scales with
//! the number of servers rather than being capped by one node's socket.

use super::{FsStat, ObjectPage, StorageBackend};
use crate::stats;
use std::io;
use std::sync::Arc;
use std::thread;

/// Default bytes per stripe chunk, used when a mount names no `--stripe-bytes`
/// / `GROVEFS_STRIPE_BYTES`. A file smaller than the stripe lives entirely on
/// slot 0 (chunk 0), so small files pay no cross-server cost; larger files
/// distribute across every server. 16 MiB matches the plan's size threshold.
/// The mount path mints a `ShardedBackend` with the operator's configured
/// stripe (see `ShardedBackend::new`); this constant is only the fallback.
pub const STRIPE_BYTES: u64 = 16 * 1024 * 1024;

/// Stable leading token on the error message [`ShardedBackend::len_with`]
/// returns when a strict sweep finds a striped object whose chunks do not form
/// a contiguous run (a present chunk living beyond an absent one). Carried
/// inside an ordinary [`io::ErrorKind::Other`] so callers can detect corruption
/// with [`ShardedBackend::is_damaged`] without string-matching the human-readable
/// sentence that follows it.
const DAMAGE_TAG: &str = "grovefs:striped-damaged:";

/// A [`StorageBackend`] that shards each object across an ordered set of member
/// backends (typically one [`crate::remote::RemoteBackend`] per data server).
pub struct ShardedBackend {
    servers: Vec<Arc<dyn StorageBackend>>,
    stripe: u64,
    /// Fingerprint of the member-id set when placement is identity-pinned (every
    /// member advertised a distinct `--member-id`, and `servers` is sorted by
    /// it). `None` means positional placement: `servers` is in the caller's
    /// `--data-servers` order and keys carry no fingerprint. Stamped into every
    /// `new_key` so [`Self::check_layout`] can refuse a DIFFERENT member set
    /// (even one of the same size) rather than silently misroute chunks.
    membership: Option<u64>,
    /// Set once some object's chunk 0 has been found where this list puts it:
    /// the order of this address list is then known to agree with the writer's,
    /// so [`ShardedBackend::chunk0_elsewhere`] no longer has to re-check every
    /// chunk-0 miss. A reversed list never sets it and keeps reporting. Only
    /// meaningful for positional stripes — an identity-pinned stripe is correct
    /// by construction regardless of input order.
    order_verified: std::sync::atomic::AtomicBool,
}

/// Decoded stripe layout carried in a `data_ref` key.
struct Layout {
    n: usize,
    stripe: u64,
    /// Member-id fingerprint for a 4-field `s<N>:<stripe>:<fp>:<base>` key;
    /// `None` for the historical 3-field `s<N>:<stripe>:<base>` (positional).
    fp: Option<u64>,
    base: String,
}

/// FNV-1a over the member ids in canonical (sorted) order — a compact,
/// allocation-free fingerprint of the exact membership set. A reorder leaves it
/// unchanged (sorted set is the same); adding, removing, or swapping a member
/// changes it, which is precisely what [`ShardedBackend::check_layout`] must
/// detect.
fn membership_fingerprint(ids: &[u64]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &id in ids {
        for b in id.to_le_bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h
}

/// How many data-servers a `data_ref` was laid out across: `Some(N)` for the
/// `s<N>` prefix, `None` for a key with no layout prefix. An unstriped key is
/// not pinned to a list length — it names one whole object, wherever the
/// single-server writer put it, and any list looks it up on member 0 — so
/// reporting commands must only constrain the keys that state a count.
pub(crate) fn layout_server_count(key: &str) -> Option<usize> {
    parse_layout(key).map(|l| l.n)
}

/// Parse `s<N>:<stripe>[:<fp>]:<base>`; `None` for any other (unstriped) key.
/// A 4-field key carries a member-id fingerprint `<fp>` (16 hex digits, written
/// by an identity-pinned stripe); a 3-field key is a positional stripe. A v4
/// uuid never begins with `s` followed by digits+`:`, and a uuid/base never
/// contains a `:`, so the segment count disambiguates the two forms cleanly and
/// neither can be mistaken for an unstriped key.
fn parse_layout(key: &str) -> Option<Layout> {
    let rest = key.strip_prefix('s')?;
    let (n_str, tail) = rest.split_once(':')?;
    let n: usize = n_str.parse().ok()?;
    // Split the remainder into its `:`-separated components.
    let segs: Vec<&str> = tail.split(':').collect();
    let (stripe_str, fp, base) = match segs.len() {
        // s<N>:<stripe>:<base> — positional, no fingerprint.
        2 => (segs[0], None, segs[1]),
        // s<N>:<stripe>:<fp>:<base> — identity-pinned.
        3 => (segs[0], Some(u64::from_str_radix(segs[1], 16).ok()?), segs[2]),
        _ => return None,
    };
    let stripe: u64 = stripe_str.parse().ok()?;
    if n == 0 || stripe == 0 || base.is_empty() {
        return None;
    }
    Some(Layout {
        n,
        stripe,
        fp,
        base: base.to_string(),
    })
}

impl ShardedBackend {
    /// Build a stripe over `servers`. Placement is IDENTITY-pinned when every
    /// member reports a distinct [`StorageBackend::placement_key`] (a remote
    /// data-server started with `--member-id`): the members are then SORTED by
    /// that id, so the caller's `--data-servers` order no longer decides which
    /// slot a chunk lands in, and a fingerprint of the sorted id set is stamped
    /// into new keys so [`Self::check_layout`] refuses a different member set.
    /// Otherwise (any member un-identified, or a duplicate id, or a single
    /// member) placement stays POSITIONAL — exactly the list order — matching
    /// the historical behavior.
    pub fn new(servers: Vec<Arc<dyn StorageBackend>>, stripe: u64) -> Self {
        assert!(
            !servers.is_empty(),
            "ShardedBackend needs at least one server"
        );
        let keys: Vec<Option<u64>> = servers.iter().map(|s| s.placement_key()).collect();
        // Identity-pinned only when there is more than one member AND every one
        // advertises a UNIQUE id; a single member never stripes, so its keys
        // stay fingerprint-free for interoperability with positional mounts.
        let identified = servers.len() > 1
            && keys.iter().all(|k| k.is_some())
            && {
                let mut v: Vec<u64> = keys.iter().filter_map(|k| *k).collect();
                v.sort_unstable();
                v.dedup();
                v.len() == servers.len()
            };
        let (servers, membership) = if identified {
            let mut pairs: Vec<(u64, Arc<dyn StorageBackend>)> = servers
                .into_iter()
                .zip(keys.into_iter().flatten())
                .map(|(s, k)| (k, s))
                .collect();
            pairs.sort_by_key(|(k, _)| *k);
            let ids: Vec<u64> = pairs.iter().map(|(k, _)| *k).collect();
            let fp = membership_fingerprint(&ids);
            (pairs.into_iter().map(|(_, s)| s).collect(), Some(fp))
        } else {
            (servers, None)
        };
        Self {
            servers,
            stripe: stripe.max(1),
            membership,
            order_verified: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn with_default_stripe(servers: Vec<Arc<dyn StorageBackend>>) -> Self {
        Self::new(servers, STRIPE_BYTES)
    }

    pub fn server_count(&self) -> usize {
        self.servers.len()
    }

    /// The member backend + object key holding logical chunk `i`.
    fn chunk(&self, l: &Layout, i: u64) -> (usize, String) {
        let idx = (i as usize) % l.n.min(self.servers.len());
        (idx, format!("{}#{}", l.base, i))
    }

    /// A striped key encodes the server count it was created with (`l.n`) and,
    /// for an identity-pinned stripe, a fingerprint of the exact member-id set
    /// (`l.fp`). If this mount was brought up with a DIFFERENT number of data
    /// servers, the `i % n` mapping in [`Self::chunk`] would silently route
    /// logical chunks to the wrong member — reading or overwriting a different
    /// object. And with the SAME count but a different SET of `--member-id`s,
    /// sorting now maps slots to different physical servers, so the chunks still
    /// land wrong. Fail loudly at every dispatch entry instead of corrupting
    /// data on a misconfigured shrink/grow/swap of the `--data-servers` list.
    ///
    /// Reordering the SAME member set is deliberately NOT an error: placement is
    /// keyed by sorted id, so a reorder yields identical slots and an identical
    /// fingerprint — the exact hazard this removes.
    ///
    /// The stripe size is deliberately NOT checked here. Each `data_ref` carries
    /// its own stripe (`s<N>:<stripe>[:<fp>]:<uuid>`) and [`Self::chunks_for`]
    /// splits on `l.stripe`, never on `self.stripe` — so a mount reconfigured
    /// with a new `--stripe-bytes` still reads objects written under the old
    /// value, and only newly created objects use the current one. Enforcing
    /// stripe equality here would break that backward compatibility for no
    /// safety gain.
    fn check_layout(&self, l: &Layout) -> io::Result<()> {
        if l.n != self.servers.len() {
            return Err(io::Error::other(format!(
                "striped object {} was laid out across {} servers but this mount has {}; \
                 refusing to remap (would read/write wrong chunks)",
                l.base,
                l.n,
                self.servers.len()
            )));
        }
        // Same count but a different member SET would sort slots onto different
        // physical servers, so also pin the identity fingerprint when either side
        // has one. The four cases keep positional<->identity mounts from
        // silently remapping each other's chunks.
        match (l.fp, self.membership) {
            (None, None) => {} // both positional: count check above is sufficient
            (Some(key_fp), Some(mount_fp)) if key_fp == mount_fp => {} // same set
            (Some(_), Some(_)) => {
                return Err(io::Error::other(format!(
                    "striped object {} was laid out across {} servers with a different \
                     --member-id set than this mount; the same-count swap would map slots to \
                     the wrong data-servers, so refusing to read/write it",
                    l.base, l.n
                )));
            }
            (Some(_), None) => {
                return Err(io::Error::other(format!(
                    "striped object {} was written by an identity-pinned stripe (members carry \
                     --member-id) but this mount lists those data-servers without matching \
                     identities; start each data-server with the same --member-id set",
                    l.base
                )));
            }
            (None, Some(_)) => {
                return Err(io::Error::other(format!(
                    "striped object {} was written by a positional stripe (no --member-id) but \
                     this mount is identity-pinned; its chunk slots cannot be re-derived \
                     safely — read legacy objects through a positional mount or rewrite them",
                    l.base
                )));
            }
        }
        Ok(())
    }

    /// Outcome of asking a member whether a chunk exists: `Absent` is a
    /// *definitive* ENOENT, while `Unknown` means the probe kept failing with
    /// transient errors (already retried). Conflating `Unknown` with `Absent`
    /// once let the GC fan-out silently skip live chunks when two consecutive
    /// probes hit a transient error, leaving permanent orphans (striping GC
    /// flake); callers must treat the two distinctly.
    fn probe(&self, s: usize, ck: &str) -> Probe {
        for attempt in 0..3u32 {
            match self.servers[s].len(ck) {
                Ok(n) => return Probe::Present(n),
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Probe::Absent,
                Err(_) => {
                    if attempt + 1 < 3 {
                        thread::sleep(std::time::Duration::from_millis(50));
                    }
                }
            }
        }
        Probe::Unknown
    }

    /// Length of logical chunk `i`, as a tri-state suited to the monotone search
    /// in [`Self::len`]: `Ok(Some(n))` present, `Ok(None)` definitively absent
    /// (ENOENT), `Err(EIO)` when the probe kept failing transiently (Unknown) —
    /// never silently folded into "absent", which would understate the size.
    fn chunk_len(&self, l: &Layout, i: u64) -> io::Result<Option<u64>> {
        let (s, ck) = self.chunk(l, i);
        match self.probe(s, &ck) {
            Probe::Present(n) => Ok(Some(n)),
            Probe::Absent => Ok(None),
            Probe::Unknown => Err(io::Error::from_raw_os_error(libc::EIO)),
        }
    }

    /// Explain a missing chunk 0, which [`Self::len`] is about to read as an
    /// empty object.
    ///
    /// Every non-empty striped object has `{base}#0`, and the contiguity
    /// invariant puts it on member 0 — so if a DIFFERENT member holds it, the
    /// bytes are real and only the list order disagrees with the writer's.
    /// Answering `0` there is the worst possible output: a reversed `--data-servers`
    /// list reported every file in the tree as empty, in the mount and in `du`
    /// alike, with nothing on stderr. Detection costs one probe per member and
    /// only runs on the chunk-0-absent branch: empty files and misordered
    /// lists, never a healthy tree.
    fn chunk0_elsewhere(&self, l: &Layout) -> io::Result<()> {
        if self
            .order_verified
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return Ok(());
        }
        let ck = self.chunk(l, 0).1;
        for i in 1..self.servers.len() {
            match self.probe(i, &ck) {
                Probe::Present(_) => {
                    return Err(io::Error::other(format!(
                        "object {} keeps chunk #0 on data-server #{i}, but this --data-servers list \
                         orders it at #0: the servers are right and their order is not, so \
                         sizes and reads would be attributed to the wrong member",
                        l.base
                    )))
                }
                // A definite miss elsewhere is consistent with an empty object.
                Probe::Absent => {}
                // Nothing was confirmed, so emptiness is not established either.
                Probe::Unknown => return Err(io::Error::from_raw_os_error(libc::EIO)),
            }
        }
        Ok(())
    }

    /// Enumerate the (chunk_index, sub_offset, sub_len) tuples covering the
    /// logical range `[off, off+len)`, clamping the first/last partial chunks.
    fn chunks_for(&self, l: &Layout, off: u64, len: usize) -> Vec<(u64, u64, usize)> {
        let mut out = Vec::new();
        if len == 0 {
            return out;
        }
        let end = off + len as u64 - 1;
        let mut i = off / l.stripe;
        let last = end / l.stripe;
        while i <= last {
            let cbase = i * l.stripe;
            let sub_off = off.max(cbase) - cbase;
            let sub_len = ((cbase + l.stripe).min(off + len as u64) - (cbase + sub_off)) as usize;
            out.push((i, sub_off, sub_len));
            i += 1;
        }
        out
    }

    /// Number of worker threads to fan a multi-chunk op out across. One per
    /// chunk is ideal, but a huge fan-out (a large single I/O over a small
    /// `--stripe-bytes`) must not spawn thousands of threads and hold one
    /// in-flight `Vec<u8>` per chunk at once. Concurrency beyond the `n` member
    /// servers buys nothing — ops to one server serialize on its socket — so the
    /// ceiling scales with the member count and is capped by a fixed bound.
    /// Small spans (the common FUSE read/write, ≤ a few chunks) are unchanged:
    /// the result equals `span` whenever `span` fits the cap.
    fn fanout(&self, n: usize, span: usize) -> usize {
        span.min((n * 8).clamp(16, 64))
    }

    /// Restore the contiguity invariant `read`/`len` rely on (only the *trailing*
    /// chunk may be short or absent) before a write lands in chunk `i0`.
    ///
    /// A sparse write that jumps past the current end would otherwise leave the
    /// old short tail and/or interior chunks missing: `len` walks from chunk 0 and
    /// stops at the first chunk that is short or absent, so any data written
    /// beyond that gap becomes invisible (the file reads as truncated). Padding
    /// every chunk below `i0` to a full stripe keeps the walk contiguous.
    ///
    /// Cost is bounded by the gap, not the file size: the downward walk stops at
    /// the first already-full chunk, so an ordinary append or in-place rewrite
    /// pays a single `len` probe. Newly materialized chunks are sparse on the
    /// member filesystem (grown via `truncate`), so they consume no real blocks
    /// until actually written.
    fn pad_below(&self, l: &Layout, i0: u64) -> io::Result<()> {
        let mut j = i0;
        while j > 0 {
            j -= 1;
            let (s, ck) = self.chunk(l, j);
            match self.servers[s].len(&ck) {
                // Full here implies full further down (we maintain the invariant),
                // so stop — everything below `j` already reads as contiguous.
                Ok(c) if c >= l.stripe => break,
                // Present but short: this was the old EOF tail; pad it to a stripe.
                Ok(_) => self.servers[s].truncate(&ck, l.stripe)?,
                // Absent: a hole. Create + grow to a full stripe (sparse on disk).
                Err(_) => {
                    self.servers[s].create(&ck)?;
                    self.servers[s].truncate(&ck, l.stripe)?;
                }
            }
        }
        Ok(())
    }

    /// Measure an object's length, optionally *strictly*.
    ///
    /// `strict == false` is the ordinary length used by `getattr`/reads: it
    /// trusts the contiguity invariant (write keeps every chunk below a new
    /// write full — see [`Self::pad_below`]) and finds the end with an
    /// exponential+binary search in O(log A) probes.
    ///
    /// `strict == true` is for the explicit `grovefs du` diagnostic sweep only.
    /// A silently truncated measure is the exact failure `du` exists to avoid,
    /// so after the search stops at the first absent chunk it probes ONE chunk
    /// further: a present chunk living beyond an absent one means the search
    /// stopped early on a hole, i.e. the object is damaged, not short. It also
    /// checks the leading chunk: a real (non-empty) object always begins at
    /// chunk #0, so a later chunk present while #0 is gone is damage too.
    /// Returns [`io::ErrorKind::Other`] carrying [`DAMAGE_TAG`] in that case.
    ///
    /// Scope, stated honestly: this catches the under-report symptom (the
    /// search halts before still-present chunks). A hole buried entirely inside
    /// a run of present chunks does not change the measured size — it is a
    /// read-path integrity matter (a wrong-byte read), not a du size error, and
    /// is left to the deeper §5-5 line of work; detecting it would cost an
    /// O(chunk-count) walk that both `len` and `du` deliberately avoid.
    fn len_with(&self, key: &str, strict: bool) -> io::Result<u64> {
        let l = match parse_layout(key) {
            Some(l) => l,
            None => return self.servers[0].len(key),
        };
        self.check_layout(&l)?;
        let len0 = match self.chunk_len(&l, 0)? {
            None => {
                self.chunk0_elsewhere(&l)?;
                if strict && self.chunk_len(&l, 1)?.is_some() {
                    return Err(Self::damaged(format!(
                        "striped object {} is missing its leading chunk #0 but a later chunk \
                         is present — content is damaged, not empty; its true size is unknown",
                        l.base
                    )));
                }
                return Ok(0);
            }
            Some(c) => {
                self.order_verified
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                c
            }
        };
        if len0 < l.stripe {
            if strict && self.chunk_len(&l, 1)?.is_some() {
                return Err(Self::damaged(format!(
                    "striped object {} ends short inside chunk #0 yet chunk #1 is present — \
                     content is damaged, not short; its true size is unknown",
                    l.base
                )));
            }
            return Ok(len0);
        }
        let mut lo = 0u64;
        let mut hi = 1u64;
        while self.chunk_len(&l, hi)?.is_some() {
            lo = hi;
            match hi.checked_mul(2) {
                Some(next) if next <= u32::MAX as u64 => hi = next,
                _ => return Ok((lo + 1) * l.stripe),
            }
        }
        while hi - lo > 1 {
            let mid = lo + (hi - lo) / 2;
            match self.chunk_len(&l, mid)? {
                Some(_) => lo = mid,
                None => hi = mid,
            }
        }
        let last = self
            .chunk_len(&l, hi - 1)?
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EIO))?;
        if strict && self.chunk_len(&l, hi + 1)?.is_some() {
            return Err(Self::damaged(format!(
                "striped object {} has a gap at chunk #{} (a present chunk lies beyond the \
                 first absent one) — content is damaged, not short; its true size is unknown",
                l.base, hi
            )));
        }
        Ok((hi - 1) * l.stripe + last)
    }

    /// Strict length for the `du` diagnostic sweep; see [`Self::len_with`].
    pub fn len_strict(&self, key: &str) -> io::Result<u64> {
        self.len_with(key, true)
    }

    /// A corruption signal: bytes that ought to be there are not, and the size
    /// cannot be trusted. Carried as [`io::ErrorKind::Other`] with a stable
    /// [`DAMAGE_TAG`] prefix so callers (currently only `du`) can tell it apart
    /// from "absent" (a real miss, [`io::ErrorKind::NotFound`]) and from "nobody
    /// answered" (a transient failure) without stringly matching on the prose.
    fn damaged(why: String) -> io::Error {
        io::Error::other(format!("{} {why}", DAMAGE_TAG))
    }

    /// Whether `e` is a [`Self::damaged`] corruption signal (as opposed to a
    /// confirmed miss or an unanswered probe).
    pub fn is_damaged(e: &io::Error) -> bool {
        e.kind() == io::ErrorKind::Other && e.to_string().starts_with(DAMAGE_TAG)
    }

    /// The human-readable sentence behind a [`Self::is_damaged`] error, with
    /// the machine [`DAMAGE_TAG`] prefix trimmed, for a caller to quote in a
    /// warning line.
    pub fn damage_reason(e: &io::Error) -> String {
        let msg = e.to_string();
        match msg.strip_prefix(DAMAGE_TAG) {
            Some(rest) => rest.trim().to_string(),
            None => msg,
        }
    }
}

/// See [`ShardedBackend::probe`].
enum Probe {
    Present(u64),
    Absent,
    Unknown,
}

impl StorageBackend for ShardedBackend {
    fn new_key(&self, uuid: &str) -> String {
        // An identity-pinned stripe stamps the member-set fingerprint so a later
        // mount with a DIFFERENT set is refused by `check_layout`; a positional
        // stripe keeps the compact 3-field form for interoperability.
        match self.membership {
            Some(fp) => format!("s{}:{}:{:016x}:{}", self.servers.len(), self.stripe, fp, uuid),
            None => format!("s{}:{}:{}", self.servers.len(), self.stripe, uuid),
        }
    }

    fn create(&self, key: &str) -> io::Result<()> {
        match parse_layout(key) {
            // Materialize chunk 0 so the object exists with length 0; the rest
            // are created lazily by write/truncate.
            Some(l) => {
                self.check_layout(&l)?;
                let (s, ck) = self.chunk(&l, 0);
                self.servers[s].create(&ck)
            }
            None => self.servers[0].create(key),
        }
    }

    fn read(&self, key: &str, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let l = match parse_layout(key) {
            Some(l) => l,
            None => return self.servers[0].read(key, offset, len),
        };
        let _total = stats::start(stats::Stage::IoShardRead);
        self.check_layout(&l)?;
        let ranges = {
            let _t = stats::start(stats::Stage::IoShardSplit);
            self.chunks_for(&l, offset, len)
        };
        // A chunk whose object is absent reads as EOF: for a contiguously
        // written striped file the only chunks that can be missing are the
        // trailing ones (past i_size) — the kernel issues readahead reads that
        // straddle past EOF, and a hard ENOENT there would surface as a spurious
        // read error. So we translate NotFound into an empty (short) read and let
        // the caller see EOF, exactly like the single-object backend does. Any
        // other error is real and propagated.
        //
        // NB: we deliberately do NOT precompute `len()` here — walking every
        // chunk to find the end on each read is O(chunk-count) remote round-trips
        // and collapses throughput on large files (measured: 1230 -> 134 MB/s).
        let is_eof = |e: &io::Error| e.kind() == io::ErrorKind::NotFound;
        let mut results: Vec<(u64, Vec<u8>)> = Vec::with_capacity(ranges.len());
        if ranges.len() <= 1 {
            for (i, sub_off, sub_len) in ranges {
                let (s, ck) = self.chunk(&l, i);
                let rd = {
                    let _t = stats::start(stats::Stage::IoShardServerOp);
                    self.servers[s].read(&ck, sub_off, sub_len)
                };
                match rd {
                    Ok(buf) => results.push((i, buf)),
                    Err(e) if is_eof(&e) => {}
                    Err(e) => return Err(e),
                }
            }
        } else {
            let servers = &self.servers;
            let n = l.n.min(servers.len());
            let errs = std::sync::Mutex::new(None::<io::Error>);
            let collected = std::sync::Mutex::new(Vec::<(u64, Vec<u8>)>::with_capacity(ranges.len()));
            let cursor = std::sync::atomic::AtomicUsize::new(0);
            let base = l.base.as_str();
            let ranges_ref = &ranges;
            let workers = self.fanout(n, ranges.len());
            thread::scope(|scope| {
                // Fixed pool; each worker pulls chunk indices off the shared
                // cursor until exhausted, so threads = `workers`, not `ranges.len()`.
                for _ in 0..workers {
                    let (errs, collected, cursor, ranges_ref, servers, base) =
                        (&errs, &collected, &cursor, ranges_ref, servers, base);
                    scope.spawn(move || loop {
                        let k = cursor.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if k >= ranges_ref.len() {
                            break;
                        }
                        let (i, sub_off, sub_len) = ranges_ref[k];
                        let idx = i as usize % n;
                        let ck = format!("{}#{}", base, i);
                        let rd = {
                            let _t = stats::start(stats::Stage::IoShardServerOp);
                            servers[idx].read(&ck, sub_off, sub_len)
                        };
                        match rd {
                            Ok(buf) => collected.lock().unwrap().push((i, buf)),
                            // Trailing / readahead-past-EOF chunk: read as EOF,
                            // contribute nothing (an empty chunk adds no bytes).
                            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                            Err(e) => {
                                let mut g = errs.lock().unwrap();
                                if g.is_none() {
                                    *g = Some(e);
                                }
                            }
                        }
                    });
                }
            });
            // Scope the guard so it drops before `errs` goes out of life here.
            let first_err = { errs.lock().unwrap().take() };
            if let Some(e) = first_err {
                return Err(e);
            }
            results = collected.into_inner().unwrap();
        }
        results.sort_by_key(|(i, _)| *i);
        let out = {
            let _t = stats::start(stats::Stage::IoShardConcat);
            let mut out = Vec::with_capacity(len);
            for (_, buf) in results {
                out.extend_from_slice(&buf);
            }
            out
        };
        Ok(out)
    }

    fn write(&self, key: &str, offset: u64, data: &[u8]) -> io::Result<usize> {
        let l = match parse_layout(key) {
            Some(l) => l,
            None => return self.servers[0].write(key, offset, data),
        };
        let _total = stats::start(stats::Stage::IoShardWrite);
        self.check_layout(&l)?;
        let ranges = {
            let _t = stats::start(stats::Stage::IoShardSplit);
            self.chunks_for(&l, offset, data.len())
        };
        // A write that jumps past the current end must first make every chunk
        // below its target contiguous, or `len` would stop at the old short tail
        // / a missing interior chunk and hide the data beyond the gap. No-op for
        // in-place rewrites and ordinary appends (see [`Self::pad_below`]).
        if !data.is_empty() {
            self.pad_below(&l, offset / l.stripe)?;
        }
        let mut written = 0usize;
        if ranges.len() <= 1 {
            for (i, sub_off, sub_len) in ranges {
                let (s, ck) = self.chunk(&l, i);
                let w = {
                    let _t = stats::start(stats::Stage::IoShardServerOp);
                    self.servers[s].write(
                        &ck,
                        sub_off,
                        &data[(i * l.stripe + sub_off - offset) as usize..][..sub_len],
                    )
                };
                written += w?;
            }
            return Ok(written);
        }
        let servers = &self.servers;
        let n = l.n.min(servers.len());
        let total = std::sync::atomic::AtomicUsize::new(0);
        let errs = std::sync::Mutex::new(None::<io::Error>);
        let cursor = std::sync::atomic::AtomicUsize::new(0);
        let stripe = l.stripe;
        let base = l.base.as_str();
        let ranges_ref = &ranges;
        let workers = self.fanout(n, ranges.len());
        thread::scope(|scope| {
            for _ in 0..workers {
                let (total, errs, cursor, ranges_ref, servers, base) =
                    (&total, &errs, &cursor, ranges_ref, servers, base);
                scope.spawn(move || loop {
                    let k = cursor.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if k >= ranges_ref.len() {
                        break;
                    }
                    let (i, sub_off, sub_len) = ranges_ref[k];
                    let idx = i as usize % n;
                    let ck = format!("{}#{}", base, i);
                    // Slice of `data` belonging to this chunk: its logical start
                    // is `i*stripe + sub_off`, so the data offset is that minus
                    // the overall write offset.
                    let dstart = (i * stripe + sub_off - offset) as usize;
                    let sub = &data[dstart..dstart + sub_len];
                    let r = {
                        let _t = stats::start(stats::Stage::IoShardServerOp);
                        servers[idx].write(&ck, sub_off, sub)
                    };
                    match r {
                        Ok(w) => {
                            total.fetch_add(w, std::sync::atomic::Ordering::Relaxed);
                        }
                        Err(e) => {
                            let mut g = errs.lock().unwrap();
                            if g.is_none() {
                                *g = Some(e);
                            }
                        }
                    }
                });
            }
        });
        let first_err = { errs.lock().unwrap().take() };
        if let Some(e) = first_err {
            return Err(e);
        }
        Ok(total.load(std::sync::atomic::Ordering::Relaxed))
    }

    fn len(&self, key: &str) -> io::Result<u64> {
        // Ordinary (non-strict) length: see [`Self::len_with`] for the
        // contiguity assumption and the exponential+binary search that finds
        // the end in O(log A) probes, keeping `getattr`/READDIRPLUS off the
        // per-chunk walk this replaced.
        self.len_with(key, false)
    }

    fn fsync(&self, key: &str) -> io::Result<()> {
        let l = match parse_layout(key) {
            Some(l) => l,
            None => return self.servers[0].fsync(key),
        };
        let size = self.len(key)?;
        let chunks = (size.div_ceil(l.stripe).max(1)) as usize;
        // Each chunk flushes independently and errors are deliberately ignored
        // (a missing chunk has nothing to flush), so fan the remote flushes out
        // over the same bounded pool `read`/`write` use instead of issuing
        // `chunks` serial round-trips. A FUSE flush/close of a large or small-
        // stripe file previously paid one blocking RTT per chunk back-to-back.
        let servers = &self.servers;
        let n = l.n.min(servers.len());
        let base = l.base.as_str();
        let cursor = std::sync::atomic::AtomicUsize::new(0);
        let workers = self.fanout(n, chunks);
        thread::scope(|scope| {
            for _ in 0..workers {
                let (cursor, servers, base) = (&cursor, servers, base);
                scope.spawn(move || loop {
                    let k = cursor.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if k >= chunks {
                        break;
                    }
                    let ck = format!("{}#{}", base, k as u64);
                    let _ = servers[k % n].fsync(&ck); // best-effort
                });
            }
        });
        Ok(())
    }

    fn truncate(&self, key: &str, new_len: u64) -> io::Result<()> {
        let l = match parse_layout(key) {
            Some(l) => l,
            None => return self.servers[0].truncate(key, new_len),
        };
        self.check_layout(&l)?;
        if new_len == 0 {
            // Drop every chunk (GC is idempotent; ignore absent ones).
            let mut i = 0u64;
            loop {
                let (s, ck) = self.chunk(&l, i);
                match self.probe(s, &ck) {
                    Probe::Present(_) => {
                        let _ = self.servers[s].remove(&ck);
                    }
                    Probe::Absent => break,
                    Probe::Unknown => {
                        let _ = self.servers[s].remove(&ck);
                        break; // can't tell more; don't scan blindly
                    }
                }
                i += 1;
                if i > u32::MAX as u64 {
                    break;
                }
            }
            // Keep a single empty chunk 0 so the object still "exists".
            let (s, ck) = self.chunk(&l, 0);
            self.servers[s].truncate(&ck, 0)?;
            return Ok(());
        }
        // (last_chunk, length of last chunk)
        let (last_chunk, last_len) = {
            let t = new_len / l.stripe;
            let w = new_len % l.stripe;
            if w == 0 {
                if t == 0 {
                    (0, l.stripe)
                } else {
                    (t - 1, l.stripe)
                }
            } else {
                (t, w)
            }
        };
        // Bring chunks 0..=last_chunk to their target sizes (sparse growth is
        // free; shrinking just trims the tail). Each is an independent positional
        // truncate, so fan them over the bounded pool rather than issuing one
        // remote round-trip per chunk serially. The beyond-the-end removal walk
        // below stays serial: it stops at the first absent chunk (a contiguity
        // dependency, not an independent op).
        {
            let servers = &self.servers;
            let n = l.n.min(servers.len());
            let base = l.base.as_str();
            let stripe = l.stripe;
            let count = (last_chunk + 1) as usize;
            let cursor = std::sync::atomic::AtomicUsize::new(0);
            let errs = std::sync::Mutex::new(None::<io::Error>);
            let workers = self.fanout(n, count);
            thread::scope(|scope| {
                for _ in 0..workers {
                    let (cursor, errs, servers, base) = (&cursor, &errs, servers, base);
                    scope.spawn(move || loop {
                        let k = cursor.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if k >= count {
                            break;
                        }
                        let i = k as u64;
                        let want = if i == last_chunk { last_len } else { stripe };
                        let ck = format!("{}#{}", base, i);
                        if let Err(e) = servers[k % n].truncate(&ck, want) {
                            let mut g = errs.lock().unwrap();
                            if g.is_none() {
                                *g = Some(e);
                            }
                        }
                    });
                }
            });
            let first_err = errs.lock().unwrap().take();
            if let Some(e) = first_err {
                return Err(e);
            }
        }
        // Remove any chunk beyond the new end (old file was longer).
        let mut i = last_chunk + 1;
        loop {
            let (s, ck) = self.chunk(&l, i);
            match self.probe(s, &ck) {
                Probe::Present(_) => {
                    let _ = self.servers[s].remove(&ck);
                }
                Probe::Absent => break,
                Probe::Unknown => {
                    let _ = self.servers[s].remove(&ck);
                    break; // can't tell more; don't scan blindly
                }
            }
            i += 1;
            if i > u32::MAX as u64 {
                break;
            }
        }
        Ok(())
    }

    fn remove(&self, key: &str) -> io::Result<()> {
        let l = match parse_layout(key) {
            Some(l) => l,
            None => return self.servers[0].remove(key),
        };
        self.check_layout(&l)?;
        // Best-effort delete of every chunk the object could own. GC is
        // idempotent, so errors on already-gone chunks are ignored.
        let mut i = 0u64;
        let mut missing_run = 0;
        while missing_run < 2 {
            let (s, ck) = self.chunk(&l, i);
            match self.probe(s, &ck) {
                Probe::Present(_) => {
                    missing_run = 0;
                    // GC stays best-effort, but a failed chunk delete means a
                    // permanent orphan (nothing sweeps later) — say so loudly.
                    if let Err(e) = self.servers[s].remove(&ck) {
                        eprintln!(
                            "striped GC: remove chunk #{i} of {} on server {s} failed: {e}",
                            l.base
                        );
                    }
                }
                Probe::Absent => missing_run += 1,
                Probe::Unknown => {
                    // Try the (idempotent) delete anyway, but do NOT reset the
                    // run: a stretch of unknown probes must still terminate.
                    let _ = self.servers[s].remove(&ck);
                }
            }
            i += 1;
            if i > u32::MAX as u64 {
                break;
            }
        }
        Ok(())
    }

    fn list_objects(&self, cursor: Option<&str>, limit: usize) -> io::Result<ObjectPage> {
        // Striping puts each chunk on ONE member, so walking the members covers
        // every stored key exactly once — chunks included, which is exactly what
        // the auditor needs (`s<N>:<stripe>:<base>` is named by the trees, the
        // `#<i>` chunks are the bytes on disk).
        super::list_across_members(
            self.servers.len(),
            |i| self.servers[i].clone(),
            cursor,
            limit,
        )
    }

    fn statvfs(&self) -> io::Result<FsStat> {
        // Striping spreads bytes round-robin across EVERY member, so `df` must
        // report the aggregate capacity, not one node's slice: a single-member
        // view understates total space by Nx and would make a 1/N-full volume
        // look nearly full (or a full volume look roomy if server 0 happened to
        // be the least-loaded). Sum the block/inode counts over members.
        //
        // `bsize` is the FUSE-reported optimal transfer size; members share a
        // block size in practice, so keep the first one observed. `namemax` is
        // taken as the minimum across members — a name must fit every backing
        // store, so the most conservative one is the honest answer.
        //
        // Availability: a striped object needs all its member servers reachable
        // to read/write every chunk, so a member that fails to report is a real
        // outage. We skip failed members and aggregate those that respond (this
        // UNDERSTATES usable capacity, which is the safe direction for `df`);
        // only if NO member reports do we propagate an error so statfs reflects
        // the outage instead of fabricating a healthy volume.
        let mut acc: Option<FsStat> = None;
        let mut last_err: Option<io::Error> = None;
        for s in &self.servers {
            match s.statvfs() {
                Ok(st) => {
                    acc = Some(match acc {
                        None => st,
                        Some(a) => FsStat {
                            tblocks: a.tblocks.saturating_add(st.tblocks),
                            bfree: a.bfree.saturating_add(st.bfree),
                            bavail: a.bavail.saturating_add(st.bavail),
                            files: a.files.saturating_add(st.files),
                            ffree: a.ffree.saturating_add(st.ffree),
                            bsize: a.bsize,
                            namemax: a.namemax.min(st.namemax),
                        },
                    });
                }
                Err(e) => last_err = Some(e),
            }
        }
        acc.ok_or_else(|| last_err.unwrap_or_else(|| io::Error::from_raw_os_error(libc::EIO)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::local::LocalBackend;

    fn tmp_root(tag: &str, n: usize) -> Vec<std::path::PathBuf> {
        (0..n)
            .map(|k| {
                let mut p = std::env::temp_dir();
                p.push(format!("grovefs_shard_{tag}_{}_{}", std::process::id(), k));
                let _ = std::fs::remove_dir_all(&p);
                p
            })
            .collect()
    }

    fn members(roots: &[std::path::PathBuf]) -> Vec<Arc<dyn StorageBackend>> {
        roots
            .iter()
            .map(|r| Arc::new(LocalBackend::new(r).unwrap()) as Arc<dyn StorageBackend>)
            .collect()
    }

    /// A member that advertises a stable `--member-id` (like a remote
    /// data-server), wrapping a real `LocalBackend` so its bytes persist on disk
    /// and travel WITH the id rather than the list position — the property
    /// identity-pinned placement relies on.
    struct IdMember {
        id: u64,
        inner: Arc<dyn StorageBackend>,
    }
    impl StorageBackend for IdMember {
        fn placement_key(&self) -> Option<u64> {
            Some(self.id)
        }
        fn create(&self, k: &str) -> io::Result<()> {
            self.inner.create(k)
        }
        fn read(&self, k: &str, o: u64, l: usize) -> io::Result<Vec<u8>> {
            self.inner.read(k, o, l)
        }
        fn write(&self, k: &str, o: u64, d: &[u8]) -> io::Result<usize> {
            self.inner.write(k, o, d)
        }
        fn len(&self, k: &str) -> io::Result<u64> {
            self.inner.len(k)
        }
        fn fsync(&self, k: &str) -> io::Result<()> {
            self.inner.fsync(k)
        }
        fn truncate(&self, k: &str, n: u64) -> io::Result<()> {
            self.inner.truncate(k, n)
        }
        fn remove(&self, k: &str) -> io::Result<()> {
            self.inner.remove(k)
        }
        fn statvfs(&self) -> io::Result<FsStat> {
            self.inner.statvfs()
        }
    }

    /// Build `roots.len()` identity-pinned members with ids `0..n`, paired to
    /// each root.
    fn id_members(roots: &[std::path::PathBuf]) -> Vec<Arc<dyn StorageBackend>> {
        roots
            .iter()
            .enumerate()
            .map(|(k, r)| {
                Arc::new(IdMember {
                    id: k as u64,
                    inner: Arc::new(LocalBackend::new(r).unwrap()),
                }) as Arc<dyn StorageBackend>
            })
            .collect()
    }

    #[test]
    fn an_reordered_identity_stripe_reads_back_correctly() {
        // The hazard identity pinning removes: a positional stripe whose list
        // order changes mis-reads the data (see
        // `a_reversed_server_list_is_reported_not_read_as_empty`). With ids
        // advertised, reordering the SAME member set must be harmless — and a
        // swap that leaves slot 0 in place (indices 1<->2) is precisely the case
        // a positional stripe silently corrupts but chunk-0 detection misses.
        let roots = tmp_root("idorder", 3);
        let stripe = 4096u64;
        let base = id_members(&roots);
        let sb = ShardedBackend::new(base.clone(), stripe);
        let key = sb.new_key("obj");
        // identity-pinned keys carry the member fingerprint: 4 `:`-separated
        // fields (s<N>, stripe, fp, base).
        assert_eq!(key.split(':').count(), 4, "{key}");
        sb.create(&key).unwrap();
        let data: Vec<u8> = (0..20000u32).map(|i| (i % 251) as u8).collect();
        sb.write(&key, 0, &data).unwrap();
        assert_eq!(sb.len(&key).unwrap(), data.len() as u64);

        // Same members, slots 1 and 2 swapped in the input list.
        let mut scrambled = base.clone();
        scrambled.swap(1, 2);
        let sb2 = ShardedBackend::new(scrambled, stripe);
        assert_eq!(sb2.len(&key).unwrap(), data.len() as u64);
        assert_eq!(sb2.read(&key, 0, data.len()).unwrap(), data);
        assert_eq!(sb2.read(&key, 1000, 500).unwrap(), data[1000..1500]);
        // Sorting by id makes the fingerprint — and so every new key — order
        // independent.
        assert_eq!(sb2.new_key("x"), sb.new_key("x"));
        for r in roots {
            let _ = std::fs::remove_dir_all(r);
        }
    }

    #[test]
    fn a_different_member_set_of_the_same_count_is_refused() {
        // Same stripe width (N = 2) but a genuinely different member SET: id 1 is
        // replaced by id 2. Sorting slots onto different physical servers would
        // silently misroute chunk #1, so the fingerprint check must refuse.
        let roots = tmp_root("setswap", 3);
        let stripe = 4096u64;
        let mk = |pairs: &[(u64, &std::path::PathBuf)]| -> Vec<Arc<dyn StorageBackend>> {
            pairs
                .iter()
                .map(|(id, r)| {
                    Arc::new(IdMember {
                        id: *id,
                        inner: Arc::new(LocalBackend::new(r).unwrap()),
                    }) as Arc<dyn StorageBackend>
                })
                .collect()
        };
        let a = mk(&[(0, &roots[0]), (1, &roots[1])]);
        let sb = ShardedBackend::new(a, stripe);
        let key = sb.new_key("obj");
        sb.create(&key).unwrap();
        let data: Vec<u8> = (0..10000u32).map(|i| (i % 251) as u8).collect();
        sb.write(&key, 0, &data).unwrap();

        let b = mk(&[(0, &roots[0]), (2, &roots[2])]);
        let sb2 = ShardedBackend::new(b, stripe);
        let err = sb2
            .len(&key)
            .expect_err("a same-count different member set must not remap");
        assert!(err.to_string().contains("--member-id set"), "{err}");
        // Reordering the SAME set (ids 1,0) is fine, by contrast.
        let c = mk(&[(1, &roots[1]), (0, &roots[0])]);
        let sb3 = ShardedBackend::new(c, stripe);
        assert_eq!(sb3.len(&key).unwrap(), data.len() as u64);
        for r in roots {
            let _ = std::fs::remove_dir_all(r);
        }
    }

    #[test]
    fn positional_keys_keep_the_three_field_form() {
        // A stripe over un-identified members (no placement_key) must stay on
        // the historical 3-field layout, so existing positional data and the
        // compact `data_ref` are unaffected by the identity feature.
        let roots = tmp_root("posform", 2);
        let sb = ShardedBackend::new(members(&roots), 1024);
        let k = sb.new_key("abcd-uuid");
        assert_eq!(k, "s2:1024:abcd-uuid");
        assert_eq!(parse_layout(&k).unwrap().fp, None);
        for r in roots {
            let _ = std::fs::remove_dir_all(r);
        }
    }

    #[test]
    fn new_key_encodes_layout_and_parses_back() {
        let roots = tmp_root("key", 2);
        let sb = ShardedBackend::new(members(&roots), 1024);
        let k = sb.new_key("abcd-uuid");
        assert_eq!(k, "s2:1024:abcd-uuid");
        let l = parse_layout(&k).unwrap();
        assert_eq!((l.n, l.stripe, l.base.as_str()), (2, 1024, "abcd-uuid"));
        // an unstriped uuid key is never mistaken for a layout
        assert!(parse_layout("3f2a-1b-00").is_none());
        for r in roots {
            let _ = std::fs::remove_dir_all(r);
        }
    }

    #[test]
    fn roundtrip_strips_and_reads_back() {
        let roots = tmp_root("rt", 3);
        let stripe = 4096u64;
        let sb = ShardedBackend::new(members(&roots), stripe);
        let key = sb.new_key("file1");
        sb.create(&key).unwrap();
        // 10000 bytes spans chunks 0,1,2 (>2 stripes) across 3 servers.
        let data: Vec<u8> = (0..10000u32).map(|i| (i % 251) as u8).collect();
        let w = sb.write(&key, 0, &data).unwrap();
        assert_eq!(w, data.len());
        assert_eq!(sb.len(&key).unwrap(), data.len() as u64);
        let back = sb.read(&key, 0, data.len()).unwrap();
        assert_eq!(back, data, "striped read must reconstruct exact bytes");
        // a mid-file straddling read spanning two chunks
        let slice = sb.read(&key, 4000, 500).unwrap();
        assert_eq!(slice, &data[4000..4500]);
        sb.remove(&key).unwrap();
        assert_eq!(sb.len(&key).unwrap(), 0);
        for r in roots {
            let _ = std::fs::remove_dir_all(r);
        }
    }

    /// A tiny stripe makes one I/O span far more chunks than the fan-out cap,
    /// exercising the bounded worker-pool (each worker pulls many chunks off the
    /// shared cursor). Guards that capping thread count did not change results:
    /// byte totals, the sort-by-chunk-index in `read`, and straddling partial
    /// reads must match the one-thread-per-chunk path exactly.
    #[test]
    fn large_fanout_bounded_workers_reads_back() {
        let roots = tmp_root("fanout", 2);
        let stripe = 64u64;
        let sb = ShardedBackend::new(members(&roots), stripe);
        let key = sb.new_key("big");
        sb.create(&key).unwrap();
        let data: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect(); // 64 chunks
        // The whole span must be routed through a CAPPED pool, not 64 threads.
        let workers = sb.fanout(2, data.len() / stripe as usize);
        assert!(workers > 0 && workers < 64, "fan-out must be capped, got {workers}");
        let w = sb.write(&key, 0, &data).unwrap();
        assert_eq!(w, data.len());
        assert_eq!(sb.len(&key).unwrap(), data.len() as u64);
        assert_eq!(sb.read(&key, 0, data.len()).unwrap(), data, "capped read must reorder correctly");
        // a straddling partial read landing mid-chunk across the capped range
        assert_eq!(sb.read(&key, 3000, 500).unwrap(), &data[3000..3500]);
        sb.remove(&key).unwrap();
        for r in roots {
            let _ = std::fs::remove_dir_all(r);
        }
    }

    /// A `data_ref` carries its own stripe (`s<N>:<stripe>:<uuid>`), so a mount
    /// reconfigured with a different `--stripe-bytes`/`GROVEFS_STRIPE_BYTES`
    /// must still read objects written under the old value — only newly created
    /// objects follow the new stripe. `check_layout` deliberately ignores the
    /// stripe for exactly this, so this is the compatibility the CLI wiring
    /// relies on and the regression a future stripe check would break.
    #[test]
    fn an_old_stripe_key_reads_under_a_new_mount_stripe() {
        let roots = tmp_root("migstripe", 2);
        // Write across 2 servers with the OLD 1 KiB stripe.
        let old = ShardedBackend::new(members(&roots), 1024);
        let key = old.new_key("mig-uuid"); // s2:1024:mig-uuid
        old.create(&key).unwrap();
        let data: Vec<u8> = (0..3000u32).map(|i| (i % 251) as u8).collect();
        old.write(&key, 0, &data).unwrap();
        assert_eq!(key, "s2:1024:mig-uuid");

        // Reopen the SAME two servers under a NEW, larger stripe: reads and len
        // still split on the key's 1024 (from `l.stripe`), never the mount's
        // 4096 (`self.stripe`), so the object survives the reconfiguration.
        let new = ShardedBackend::new(members(&roots), 4096);
        assert_eq!(new.len(&key).unwrap(), data.len() as u64);
        assert_eq!(new.read(&key, 0, data.len()).unwrap(), data);
        assert_eq!(new.read(&key, 1500, 500).unwrap(), &data[1500..2000]);

        // A NEW object minted through the reconfigured mount carries the new
        // stripe, proving the override reaches `new_key` too.
        assert_eq!(new.new_key("fresh-uuid"), "s2:4096:fresh-uuid");
        for r in roots {
            let _ = std::fs::remove_dir_all(r);
        }
    }

    /// A reversed `--data-servers` list used to answer `0` for every striped file, the
    /// "whole tree looks empty" failure: the bytes are real and only the list
    /// order disagrees, which has to come back as an error, not a size.
    #[test]
    fn a_reversed_server_list_is_reported_not_read_as_empty() {
        let roots = tmp_root("order", 2);
        let stripe = 4096u64;
        let sb = ShardedBackend::new(members(&roots), stripe);
        let key = sb.new_key("obj1");
        sb.create(&key).unwrap();
        let data: Vec<u8> = (0..10000u32).map(|i| i as u8).collect();
        sb.write(&key, 0, &data).unwrap();
        assert_eq!(sb.len(&key).unwrap(), data.len() as u64);

        let m = members(&roots);
        let flipped = ShardedBackend::new(vec![m[1].clone(), m[0].clone()], stripe);
        let err = flipped.len(&key).expect_err("a reversed list must not answer 0");
        let msg = err.to_string();
        assert!(msg.contains("order"), "{msg}");
        assert!(msg.contains("wrong member"), "{msg}");
        for r in roots {
            let _ = std::fs::remove_dir_all(r);
        }
    }

    /// The same check must not turn an object with no bytes into an error: an
    /// empty striped object has no chunk #0 anywhere.
    #[test]
    fn an_empty_striped_object_still_measures_zero() {
        let roots = tmp_root("empty0", 2);
        let sb = ShardedBackend::new(members(&roots), 4096);
        let key = sb.new_key("emp");
        sb.create(&key).unwrap();
        assert_eq!(sb.len(&key).unwrap(), 0);
        for r in roots {
            let _ = std::fs::remove_dir_all(r);
        }
    }

    /// §6-2: the strict length `du` uses must name a striped under-report
    /// (chunks present beyond the point the monotone search stopped) instead of
    /// silently returning the short size — while a genuinely short TAIL stays a
    /// valid size, not a false "damaged". Files are deleted through fresh
    /// member handles and re-probed through yet another fresh `ShardedBackend`
    /// so the `LocalBackend` fd cache cannot mask an unlink.
    #[test]
    fn len_strict_flags_interior_and_leading_gaps_but_not_a_short_tail() {
        let roots = tmp_root("gap", 2);
        let stripe = 4096u64;
        let nchunks = 5u64;
        let data: Vec<u8> = (0..(stripe * nchunks) as usize)
            .map(|i| (i % 251) as u8)
            .collect();

        // Write a full contiguous 5-chunk object under `base`, then drop the
        // backend so later handles open the files fresh.
        let build = |base: &str| -> String {
            let sb = ShardedBackend::new(members(&roots), stripe);
            let key = sb.new_key(base);
            sb.create(&key).unwrap();
            sb.write(&key, 0, &data).unwrap();
            key
        };
        let remove_chunk = |key: &str, i: u64| {
            let l = parse_layout(key).unwrap();
            let ms = members(&roots);
            let ck = format!("{}#{}", l.base, i);
            ms[(i as usize) % l.n].remove(&ck).unwrap();
        };
        let reopen = || ShardedBackend::new(members(&roots), stripe);

        // 0) Intact: measures exactly, no error.
        let key = build("intact");
        assert_eq!(reopen().len_strict(&key).unwrap(), data.len() as u64);

        // 1) Interior gap: chunk #0 present, #1 gone, #2.. present.
        let key = build("interior");
        remove_chunk(&key, 1);
        let sb = reopen();
        // The old non-strict path still under-reports to one chunk — the very
        // silent short-report §3.6.3 recorded (64.0M -> 16.0M).
        assert_eq!(sb.len(&key).unwrap(), stripe);
        let err = sb
            .len_strict(&key)
            .expect_err("interior gap must be reported, not measured short");
        assert!(ShardedBackend::is_damaged(&err), "{err}");
        assert!(err.to_string().contains("has a gap at chunk #1"), "{err}");

        // 2) Leading gap: chunk #0 gone, a later chunk present — damaged,
        //    not the empty object `Ok(0)` it used to look like.
        let key = build("leading");
        remove_chunk(&key, 0);
        let err = reopen()
            .len_strict(&key)
            .expect_err("leading gap must be reported");
        assert!(ShardedBackend::is_damaged(&err), "{err}");
        assert!(
            err.to_string().contains("missing its leading chunk #0"),
            "{err}"
        );

        // 3) Short TAIL: only the last chunk is gone — a legitimate truncation,
        //    so strict must agree with len and NOT call it damaged.
        let key = build("tail");
        remove_chunk(&key, nchunks - 1);
        assert_eq!(
            reopen().len_strict(&key).unwrap(),
            stripe * (nchunks - 1)
        );

        for r in roots {
            let _ = std::fs::remove_dir_all(r);
        }
    }

    #[test]
    fn read_at_or_past_eof_is_empty_not_error() {
        // Regression guard for the truncate/GC path: the kernel issues readahead
        // reads at/after i_size; a striped file whose trailing chunks were freed
        // must return an empty/short read (like LocalBackend past EOF), never a
        // hard ENOENT from touching a removed chunk.
        let roots = tmp_root("eof", 2);
        let stripe = 4096u64;
        let sb = ShardedBackend::new(members(&roots), stripe);
        let key = sb.new_key("eof1");
        sb.create(&key).unwrap();
        let data: Vec<u8> = (0..5000u32).map(|i| i as u8).collect(); // spans chunk0+chunk1
        sb.write(&key, 0, &data).unwrap();
        // read exactly at EOF -> empty
        assert_eq!(sb.read(&key, 5000, 4096).unwrap().len(), 0);
        // read past EOF -> empty
        assert_eq!(sb.read(&key, 99999, 4096).unwrap().len(), 0);
        // a clamped straddling read returns only the in-range prefix, no error
        let tail = sb.read(&key, 4000, 8192).unwrap();
        assert_eq!(tail, &data[4000..]);
        // shrink so chunk1 is freed, then a past-EOF read must stay clean
        sb.truncate(&key, 100).unwrap();
        assert_eq!(sb.read(&key, 100, 4096).unwrap().len(), 0);
        assert_eq!(sb.read(&key, 0, 4096).unwrap(), &data[..100]);
        for r in roots {
            let _ = std::fs::remove_dir_all(r);
        }
    }

    #[test]
    fn truncate_shrinks_and_grows_consistently() {
        let roots = tmp_root("tr", 2);
        let stripe = 2048u64;
        let sb = ShardedBackend::new(members(&roots), stripe);
        let key = sb.new_key("t1");
        sb.create(&key).unwrap();
        let data: Vec<u8> = (0..6000u32).map(|i| i as u8).collect();
        sb.write(&key, 0, &data).unwrap();
        // shrink into the middle of chunk 1
        sb.truncate(&key, 2500).unwrap();
        assert_eq!(sb.len(&key).unwrap(), 2500);
        let back = sb.read(&key, 0, 2500).unwrap();
        assert_eq!(back, &data[..2500]);
        // truncate to zero empties the object but keeps it existing
        sb.truncate(&key, 0).unwrap();
        assert_eq!(sb.len(&key).unwrap(), 0);
        for r in roots {
            let _ = std::fs::remove_dir_all(r);
        }
    }

    #[test]
    fn layout_server_count_mismatch_fails_not_silent_remap() {
        // A key laid out across 3 servers must NOT be silently remapped when this
        // mount only has 2 members — that would read/write the wrong chunk. It
        // must fail loudly instead.
        let roots = tmp_root("mismatch", 2);
        let sb = ShardedBackend::new(members(&roots), 4096);
        // Forge a 3-server layout key while the backend holds only 2 members.
        let key = "s3:4096:deadbeef".to_string();
        assert!(sb.create(&key).is_err());
        assert!(sb.write(&key, 0, &[1, 2, 3]).is_err());
        assert!(sb.read(&key, 0, 4096).is_err());
        assert!(sb.len(&key).is_err());
        assert!(sb.truncate(&key, 10).is_err());
        assert!(sb.remove(&key).is_err());
        // The matching-arity path is unaffected.
        let ok = sb.new_key("ok");
        assert_eq!(sb.create(&ok).unwrap(), ());
        assert_eq!(sb.len(&ok).unwrap(), 0);
        for r in roots {
            let _ = std::fs::remove_dir_all(r);
        }
    }

    #[test]
    fn sparse_write_past_eof_is_not_hidden() {
        // Regression for the striping hole bug: a write that jumps past the
        // current end (lseek + write, e.g. `dd seek=`) used to leave the old short
        // tail and interior chunks absent, so `len` stopped at the first short /
        // missing chunk and everything beyond the gap read as truncated. `write`
        // now materializes the gap (pad_below), so the sparse region reads back
        // as zeros and the far data is visible.
        let roots = tmp_root("sparse", 2);
        let stripe = 4096u64;
        let sb = ShardedBackend::new(members(&roots), stripe);
        let key = sb.new_key("sp1");
        sb.create(&key).unwrap();

        // 100 bytes at the start -> chunk 0 becomes a short tail (len 100).
        let head: Vec<u8> = (0..100u32).map(|i| i as u8).collect();
        sb.write(&key, 0, &head).unwrap();
        assert_eq!(sb.len(&key).unwrap(), 100);

        // Jump way past the end: write 50 bytes at offset 10000 (chunk 2),
        // leaving a ~9900-byte hole across chunks 0 (short), 1 (absent), 2 (lead).
        let tail: Vec<u8> = (200..250u32).map(|i| i as u8).collect();
        let off = 10_000u64;
        sb.write(&key, off, &tail).unwrap();

        // The far write must be reflected in the length, not hidden by the hole.
        assert_eq!(
            sb.len(&key).unwrap(),
            off + tail.len() as u64,
            "len must span the hole"
        );

        // Head intact, hole reads as zeros, tail intact — full reconstruction.
        let all = sb.read(&key, 0, (off as usize) + tail.len()).unwrap();
        assert_eq!(&all[..100], &head[..], "head preserved");
        assert!(
            all[100..(off as usize)].iter().all(|&b| b == 0),
            "hole must read as zeros"
        );
        assert_eq!(&all[(off as usize)..], &tail[..], "far tail preserved");

        // Targeted reads at the boundaries.
        assert_eq!(sb.read(&key, off, 50).unwrap(), tail);
        assert_eq!(sb.read(&key, 5000, 10).unwrap(), vec![0u8; 10]); // inside the hole
        sb.remove(&key).unwrap();
        for r in roots {
            let _ = std::fs::remove_dir_all(r);
        }
    }

    /// A backend whose only role is to report a fixed [`FsStat`] (and optionally
    /// fail), so statvfs aggregation can be asserted deterministically without
    /// depending on the real free-block count of a shared temp device (which
    /// drifts between calls and would make a `xN` assertion flaky).
    struct MockStat {
        stat: FsStat,
        fail: bool,
    }

    impl StorageBackend for MockStat {
        fn create(&self, _key: &str) -> io::Result<()> {
            Ok(())
        }
        fn read(&self, _key: &str, _offset: u64, len: usize) -> io::Result<Vec<u8>> {
            Ok(vec![0u8; len])
        }
        fn write(&self, _key: &str, _offset: u64, data: &[u8]) -> io::Result<usize> {
            Ok(data.len())
        }
        fn len(&self, _key: &str) -> io::Result<u64> {
            Ok(0)
        }
        fn fsync(&self, _key: &str) -> io::Result<()> {
            Ok(())
        }
        fn truncate(&self, _key: &str, _new_len: u64) -> io::Result<()> {
            Ok(())
        }
        fn remove(&self, _key: &str) -> io::Result<()> {
            Ok(())
        }
        fn statvfs(&self) -> io::Result<FsStat> {
            if self.fail {
                Err(io::Error::from_raw_os_error(libc::EIO))
            } else {
                Ok(self.stat)
            }
        }
    }

    fn mock(tblocks: u64, bfree: u64, namemax: u32, fail: bool) -> Arc<dyn StorageBackend> {
        Arc::new(MockStat {
            stat: FsStat {
                tblocks,
                bfree,
                bavail: bfree,
                files: tblocks / 2,
                ffree: bfree / 2,
                bsize: 4096,
                namemax,
            },
            fail,
        })
    }

    #[test]
    fn statvfs_aggregates_every_member_not_just_first() {
        // Three members with distinct capacities must sum, not report server 0.
        let sb = ShardedBackend::new(
            vec![
                mock(100, 40, 255, false),
                mock(200, 60, 255, false),
                mock(300, 80, 255, false),
            ],
            4096,
        );
        let agg = sb.statvfs().unwrap();
        assert_eq!(agg.tblocks, 600, "total blocks must sum across members");
        assert_eq!(agg.bfree, 180, "free blocks must sum across members");
        assert_eq!(agg.files, 50 + 100 + 150);
        assert_eq!(agg.ffree, 20 + 30 + 40);
        assert_eq!(agg.bsize, 4096, "block size preserved");
        assert_eq!(agg.namemax, 255, "namemax preserved when equal");
    }

    #[test]
    fn statvfs_takes_min_namemax_and_skips_unreachable_members() {
        // A conservative namemax (min) and skipping a failed member while still
        // aggregating the reachable ones (understating capacity = safe).
        let sb = ShardedBackend::new(
            vec![
                mock(100, 40, 255, false),
                mock(100, 40, 64, true),
                mock(100, 40, 128, false),
            ],
            4096,
        );
        let agg = sb.statvfs().unwrap();
        // member[1] failed and is skipped: only 0 and 2 count.
        assert_eq!(agg.tblocks, 200, "failed member excluded");
        assert_eq!(agg.bfree, 80);
        assert_eq!(agg.namemax, 128, "min namemax over reachable members");
    }

    #[test]
    fn statvfs_errors_only_when_no_member_reports() {
        let sb = ShardedBackend::new(
            vec![mock(100, 40, 255, true), mock(100, 40, 255, true)],
            4096,
        );
        assert!(
            sb.statvfs().is_err(),
            "all members down must surface an error, not fake stats"
        );
    }

    #[test]
    fn len_binary_search_matches_exact_sizes_across_chunk_boundaries() {
        // The exponential+binary rewrite of `len` must reproduce the old linear
        // walk's exact size for every boundary shape: empty, single short chunk,
        // exact multiples (trailing chunk full, next absent), and partial tails.
        // Each size lands on a different first-absent index, so the binary search
        // is exercised at many pivots rather than one lucky case.
        let roots = tmp_root("lenb", 2);
        let stripe = 1024u64;
        let sb = ShardedBackend::new(members(&roots), stripe);
        for &size in &[
            0u64, 1, 1023, 1024, 1025, 2048, 3071, 4096, 5000, 8192, 8193, 12288, 13000,
        ] {
            let key = sb.new_key(&format!("f{size}"));
            sb.create(&key).unwrap();
            if size > 0 {
                let data: Vec<u8> = (0..size as u32).map(|i| (i % 251) as u8).collect();
                sb.write(&key, 0, &data).unwrap();
            }
            assert_eq!(sb.len(&key).unwrap(), size, "len mismatch at size {size}");
            // read clamps to the same EOF the search computed
            let back = sb.read(&key, 0, size as usize + 4096).unwrap();
            assert_eq!(back.len() as u64, size, "read must clamp at size {size}");
            sb.remove(&key).unwrap();
        }
        for r in roots {
            let _ = std::fs::remove_dir_all(r);
        }
    }
}

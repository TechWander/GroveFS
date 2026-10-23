//! Deterministic cross-server striping backend (data plane, no meta change).
//!
//! A file's bytes are spread round-robin over `N` member backends in fixed
//! `STRIPE_BYTES` chunks: logical chunk `i` lives on server `i % N` under the
//! object key `{base}#{i}`. The layout is encoded into the durable `data_ref`
//! key (`s<N>:<stripe>:<uuid>`) at create time so reads are self-describing and
//! an unprefixed (legacy) key still falls through to the single-object path —
//! the metadata schema, `MetaProvider` and the wire protocol are all untouched.
//!
//! Concurrency: a large read/write that spans several chunks fans its chunk
//! operations out with `std::thread::scope`, so aggregate bandwidth scales with
//! the number of servers rather than being capped by one node's socket.

use super::{FsStat, StorageBackend};
use std::io;
use std::sync::Arc;
use std::thread;

/// Bytes per stripe chunk. A file smaller than this lives entirely on server 0
/// (chunk 0), so small files pay no cross-server cost; larger files distribute
/// across every server. 16 MiB matches the plan's size threshold.
pub const STRIPE_BYTES: u64 = 16 * 1024 * 1024;

/// A [`StorageBackend`] that shards each object across an ordered set of member
/// backends (typically one [`crate::remote::RemoteBackend`] per data server).
pub struct ShardedBackend {
    servers: Vec<Arc<dyn StorageBackend>>,
    stripe: u64,
}

/// Decoded stripe layout carried in a `data_ref` key.
struct Layout {
    n: usize,
    stripe: u64,
    base: String,
}

/// Parse `s<N>:<stripe>:<base>`; `None` for any other (legacy) key. A v4 uuid
/// never begins with `s` followed by digits+`:`, so legacy keys cannot collide.
fn parse_layout(key: &str) -> Option<Layout> {
    let rest = key.strip_prefix('s')?;
    let (n_str, tail) = rest.split_once(':')?;
    let (stripe_str, base) = tail.split_once(':')?;
    let n: usize = n_str.parse().ok()?;
    let stripe: u64 = stripe_str.parse().ok()?;
    if n == 0 || stripe == 0 || base.is_empty() {
        return None;
    }
    Some(Layout { n, stripe, base: base.to_string() })
}

impl ShardedBackend {
    pub fn new(servers: Vec<Arc<dyn StorageBackend>>, stripe: u64) -> Self {
        assert!(!servers.is_empty(), "ShardedBackend needs at least one server");
        Self {
            servers,
            stripe: stripe.max(1),
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

    /// A striped key encodes the server count it was created with (`l.n`). If
    /// this mount was brought up with a DIFFERENT number of data servers, the
    /// `i % n` mapping in [`Self::chunk`] would silently route logical chunks to
    /// the wrong member — reading or overwriting a different object. Fail loudly
    /// at every dispatch entry instead of corrupting data on a misconfigured
    /// shrink/grow of the `--data` list.
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
}

/// See [`ShardedBackend::probe`].
enum Probe {
    Present(u64),
    Absent,
    Unknown,
}

impl StorageBackend for ShardedBackend {
    fn new_key(&self, uuid: &str) -> String {
        format!("s{}:{}:{}", self.servers.len(), self.stripe, uuid)
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
        self.check_layout(&l)?;
        let ranges = self.chunks_for(&l, offset, len);
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
                match self.servers[s].read(&ck, sub_off, sub_len) {
                    Ok(buf) => results.push((i, buf)),
                    Err(e) if is_eof(&e) => {}
                    Err(e) => return Err(e),
                }
            }
        } else {
            let servers = &self.servers;
            let errs = std::sync::Mutex::new(None::<io::Error>);
            thread::scope(|scope| {
                let mut handles = Vec::with_capacity(ranges.len());
                for (i, sub_off, sub_len) in ranges {
                    let errs = &errs;
                    let base = l.base.clone();
                    let n = l.n.min(servers.len());
                    handles.push(
                        scope.spawn(move || -> (u64, Vec<u8>) {
                            let idx = i as usize % n;
                            let ck = format!("{}#{}", base, i);
                            match servers[idx].read(&ck, sub_off, sub_len) {
                                Ok(buf) => (i, buf),
                                Err(e) if e.kind() == io::ErrorKind::NotFound => (i, Vec::new()),
                                Err(e) => {
                                    let mut g = errs.lock().unwrap();
                                    if g.is_none() {
                                        *g = Some(e);
                                    }
                                    (i, Vec::new())
                                }
                            }
                        }),
                    );
                }
                for h in handles {
                    results.push(h.join().unwrap());
                }
            });
            // Scope the guard so it drops before `errs` goes out of life here.
            let first_err = { errs.lock().unwrap().take() };
            if let Some(e) = first_err {
                return Err(e);
            }
        }
        results.sort_by_key(|(i, _)| *i);
        let mut out = Vec::with_capacity(len);
        for (_, buf) in results {
            out.extend_from_slice(&buf);
        }
        Ok(out)
    }

    fn write(&self, key: &str, offset: u64, data: &[u8]) -> io::Result<usize> {
        let l = match parse_layout(key) {
            Some(l) => l,
            None => return self.servers[0].write(key, offset, data),
        };
        self.check_layout(&l)?;
        let ranges = self.chunks_for(&l, offset, data.len());
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
                written += self.servers[s].write(&ck, sub_off, &data[(i * l.stripe + sub_off - offset) as usize..][..sub_len])?;
            }
            return Ok(written);
        }
        let servers = &self.servers;
        let total = std::sync::atomic::AtomicUsize::new(0);
        let errs = std::sync::Mutex::new(None::<io::Error>);
        thread::scope(|scope| {
            let mut handles = Vec::with_capacity(ranges.len());
            for (i, sub_off, sub_len) in ranges {
                let total = &total;
                let errs = &errs;
                let base = l.base.clone();
                let n = l.n.min(servers.len());
                handles.push(scope.spawn(move || {
                    let idx = i as usize % n;
                    let ck = format!("{}#{}", base, i);
                    // Slice of `data` belonging to this chunk: its logical start
                    // is `i*stripe + sub_off`, so the data offset is that minus
                    // the overall write offset.
                    let dstart = (i * l.stripe + sub_off - offset) as usize;
                    let sub = &data[dstart..dstart + sub_len];
                    match servers[idx].write(&ck, sub_off, sub) {
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
                }));
            }
            for h in handles {
                let _ = h.join();
            }
        });
        let first_err = { errs.lock().unwrap().take() };
        if let Some(e) = first_err {
            return Err(e);
        }
        Ok(total.load(std::sync::atomic::Ordering::Relaxed))
    }

    fn len(&self, key: &str) -> io::Result<u64> {
        let l = match parse_layout(key) {
            Some(l) => l,
            None => return self.servers[0].len(key),
        };
        self.check_layout(&l)?;
        // Chunks are CONTIGUOUS: `write` keeps every chunk below a new write full
        // (see `pad_below`), so exactly the range [0, A) is present and every
        // chunk at/after A is absent, with only the trailing chunk A-1 allowed to
        // be short. "chunk i is absent" is therefore a monotone predicate with a
        // single boundary — we find A with exponential bounding + binary search
        // in O(log A) probes instead of the old O(A) linear walk. That walk made
        // getattr (and READDIRPLUS over a directory of large striped files) fan
        // out one probe per chunk — e.g. ~6400 round-trips for a 100 GiB file at
        // a 16 MiB stripe — the `backend.len` hot-spot `read` itself refuses to
        // pay (see the comment there). A `Probe::Unknown` still fails honestly
        // (EIO) rather than reporting a too-short size.
        //
        // Chunk 0 first: absent => empty object; present-and-short => it is the
        // only (trailing) chunk, size == its length.
        let len0 = match self.chunk_len(&l, 0)? {
            None => return Ok(0),
            Some(c) => c,
        };
        if len0 < l.stripe {
            return Ok(len0);
        }
        // Chunk 0 is a full stripe, so A >= 1. Exponential search for a present
        // low bound `lo` and an absent high bound `hi` (present(lo), absent(hi)).
        let mut lo = 0u64;
        let mut hi = 1u64;
        while self.chunk_len(&l, hi)?.is_some() {
            lo = hi;
            match hi.checked_mul(2) {
                Some(next) if next <= u32::MAX as u64 => hi = next,
                // Beyond 2^32 chunks (>~68 YiB at a 16 MiB stripe) the old
                // walk bailed with the confirmed-full total; mirror that.
                _ => return Ok((lo + 1) * l.stripe),
            }
        }
        // Binary search the boundary in (lo, hi]: lo present, hi absent.
        while hi - lo > 1 {
            let mid = lo + (hi - lo) / 2;
            match self.chunk_len(&l, mid)? {
                Some(_) => lo = mid,
                None => hi = mid,
            }
        }
        // `hi` is the first absent chunk; `hi-1` is the trailing (present) chunk.
        let last = self.chunk_len(&l, hi - 1)?.ok_or_else(|| io::Error::from_raw_os_error(libc::EIO))?;
        Ok((hi - 1) * l.stripe + last)
    }

    fn fsync(&self, key: &str) -> io::Result<()> {
        let l = match parse_layout(key) {
            Some(l) => l,
            None => return self.servers[0].fsync(key),
        };
        let size = self.len(key)?;
        let chunks = size.div_ceil(l.stripe).max(1);
        for i in 0..chunks {
            let (s, ck) = self.chunk(&l, i);
            let _ = self.servers[s].fsync(&ck); // best-effort: a missing chunk has nothing to flush
        }
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
        // free; shrinking just trims the tail).
        for i in 0..=last_chunk {
            let want = if i == last_chunk { last_len } else { l.stripe };
            let (s, ck) = self.chunk(&l, i);
            self.servers[s].truncate(&ck, want)?;
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
                        eprintln!("striped GC: remove chunk #{i} of {} on server {s} failed: {e}", l.base);
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

    #[test]
    fn new_key_encodes_layout_and_parses_back() {
        let roots = tmp_root("key", 2);
        let sb = ShardedBackend::new(members(&roots), 1024);
        let k = sb.new_key("abcd-uuid");
        assert_eq!(k, "s2:1024:abcd-uuid");
        let l = parse_layout(&k).unwrap();
        assert_eq!((l.n, l.stripe, l.base.as_str()), (2, 1024, "abcd-uuid"));
        // legacy uuid keys are never mistaken for a layout
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
        assert_eq!(sb.len(&key).unwrap(), off + tail.len() as u64, "len must span the hole");

        // Head intact, hole reads as zeros, tail intact — full reconstruction.
        let all = sb.read(&key, 0, (off as usize) + tail.len()).unwrap();
        assert_eq!(&all[..100], &head[..], "head preserved");
        assert!(all[100..(off as usize)].iter().all(|&b| b == 0), "hole must read as zeros");
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
            vec![mock(100, 40, 255, false), mock(200, 60, 255, false), mock(300, 80, 255, false)],
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
            vec![mock(100, 40, 255, false), mock(100, 40, 64, true), mock(100, 40, 128, false)],
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
        let sb = ShardedBackend::new(vec![mock(100, 40, 255, true), mock(100, 40, 255, true)], 4096);
        assert!(sb.statvfs().is_err(), "all members down must surface an error, not fake stats");
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

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
        let ranges = self.chunks_for(&l, offset, data.len());
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
        // Walk chunks from 0; the file ends at the first chunk shorter than the
        // stripe (partial tail) or absent (not yet written). Exact for
        // contiguous files, which is the striping target; a sparse hole would
        // under-report (documented limitation, mirrors nothing the tests use).
        let mut total = 0u64;
        let mut i = 0u64;
        loop {
            let (s, ck) = self.chunk(&l, i);
            match self.servers[s].len(&ck) {
                Ok(c) => {
                    total = i * l.stripe + c;
                    if c < l.stripe {
                        return Ok(total);
                    }
                }
                Err(_) => return Ok(total),
            }
            i += 1;
            if i > u32::MAX as u64 {
                return Ok(total); // runaway guard
            }
        }
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
        if new_len == 0 {
            // Drop every chunk (GC is idempotent; ignore absent ones).
            let mut i = 0u64;
            loop {
                let (s, ck) = self.chunk(&l, i);
                let existed = self.servers[s].len(&ck).is_ok();
                if !existed && i > 0 {
                    break;
                }
                let _ = self.servers[s].remove(&ck);
                i += 1;
                if i > u32::MAX as u64 {
                    break;
                }
                if !existed {
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
            if self.servers[s].len(&ck).is_err() {
                break;
            }
            let _ = self.servers[s].remove(&ck);
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
        // Best-effort delete of every chunk the object could own. GC is
        // idempotent, so errors on already-gone chunks are ignored.
        let mut i = 0u64;
        let mut missing_run = 0;
        while missing_run < 2 {
            let (s, ck) = self.chunk(&l, i);
            if self.servers[s].len(&ck).is_ok() {
                missing_run = 0;
                let _ = self.servers[s].remove(&ck);
            } else {
                missing_run += 1;
            }
            i += 1;
            if i > u32::MAX as u64 {
                break;
            }
        }
        Ok(())
    }

    fn statvfs(&self) -> io::Result<FsStat> {
        // Report the first member's capacity; striping distributes bytes across
        // all of them, so this is a per-node view (a full aggregate is a later
        // refinement and only cosmetic for `df`).
        self.servers[0].statvfs()
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
                p.push(format!("yourfs_shard_{tag}_{}_{}", std::process::id(), k));
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
}

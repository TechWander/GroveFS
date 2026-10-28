//! TCP client implementation of [`StorageBackend`].
//!
//! This is the "脱离 NFS" remote data path: every object operation is framed by
//! [`crate::remote::proto`] and shipped to the [`super::server::DataServer`].
//!
//! Efficiency choices:
//! - A pool of persistent connections so repeated object ops reuse warm sockets
//!   instead of reconnecting every call. Since fuser 0.18 the high-level session
//!   dispatches FUSE callbacks concurrently across worker threads (`n_threads` +
//!   `clone_fd`), so one mount genuinely drives several pool entries at once;
//!   the server handles each connection on its own thread.
//! - Large ops are striped across several pooled sockets at once: reads of
//!   1 MiB or more issue parallel `OP_READ`s per 1 MiB slice, and writes of
//!   4 MiB or more issue parallel `OP_WRITE`s per 4 MiB slice (see the
//!   constants below).
//! - Reconnect resilience: a data-server restart closes every pooled socket at
//!   once. When a *reused* pooled connection fails mid-request we discard the
//!   whole pool and retry the operation once on a brand-new connection, so an
//!   in-flight app recovers transparently instead of seeing a spurious EIO.
//!   Fresh connects use a small bounded backoff to ride out a restart that is
//!   still in progress.

use super::proto;
use crate::backend::{FsStat, StorageBackend};
use libc::{EACCES, EIO, ENOENT, EROFS};
use parking_lot::Mutex;
use std::io;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::stats;

/// Reads at or above this size are split into [`READ_CHUNK`] slices and fetched
/// concurrently over separate pooled sockets, stitched back in offset order.
pub const CHUNKED_READ_MIN: usize = 1 << 20;
/// Slice size for chunked reads.
pub const READ_CHUNK: usize = 1 << 20;
/// Writes at or above this size are split into [`WRITE_CHUNK`] slices shipped
/// concurrently. Note: through a FUSE mount a single `write` callback never
/// exceeds `max_write` (1 MiB, negotiated in `init`), so the chunked write path
/// serves direct backend users (e.g. a future ShardedBackend fan-out) rather
/// than pretending mounted streams are parallelized here.
pub const CHUNKED_WRITE_MIN: usize = 4 << 20;
/// Slice size for chunked writes.
pub const WRITE_CHUNK: usize = 4 << 20;
/// Upper bound on simultaneously in-flight slices per chunked op: enough to
/// saturate a LAN link, low enough that one huge read cannot turn into a
/// 64-socket storm against the data-server.
const MAX_PARALLEL_SLICES: usize = 8;

pub struct RemoteBackend {
    addr: String,
    token: String,
    /// API key (`<id>:<secret>`) presented on `OP_OPEN` to a data-server started
    /// with `--require-api-key`. Empty for servers gated by the static token (or
    /// by nothing), which is the pre-existing behavior.
    api_key: String,
    /// Set once the first handshake has been accepted: any later refusal is then
    /// an auth LOSS on a live mount (a revoke landing mid-session), which
    /// `OP_OPEN`'s refusal path reports through [`super::signal_auth_loss`].
    opened: AtomicBool,
    pool: Arc<Mutex<Vec<TcpStream>>>,
    max_conns: usize,
    connect_timeout: Duration,
    /// Attempts made to open a brand-new socket before giving up, so a
    /// data-server that is still restarting is tolerated.
    connect_retries: u32,
    /// Sleep between fresh-connect attempts. Deliberately small and bounded:
    /// the FUSE session dispatch is single-threaded, so we must not stall it for
    /// long when the peer is truly down.
    reconnect_backoff: Duration,
    /// Live fencing epoch, shared with the meta client: stamped on every
    /// write-intent op so the data-server arbiter can refuse a stale primary's
    /// clients. Defaults to a detached 0 (gate inert) for non-HA use.
    fence_epoch: Arc<AtomicU64>,
    /// Re-query the authoritative epoch from the meta client (updates
    /// `fence_epoch`) and return it; invoked once when the data-server fences a
    /// write. Defaults to a no-op returning the current value.
    fence_refresh: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl RemoteBackend {
    /// Connect to `addr` with no access token (matches a server started without
    /// one). A bad address surfaces at mount time via the initial handshake.
    pub fn new(addr: &str) -> io::Result<Self> {
        Self::new_with_token(addr, "")
    }

    /// Like [`Self::new`], but presents `token` in the mandatory `OP_OPEN`
    /// handshake on every freshly-created socket. A server configured with a
    /// different token rejects the connection with `PermissionDenied` — the
    /// client fails fast rather than retrying, because a wrong secret is a
    /// configuration error, not a transient one.
    pub fn new_with_token(addr: &str, token: &str) -> io::Result<Self> {
        Self::new_with_creds(addr, token, "")
    }

    /// Like [`Self::new_with_token`], but also presents `api_key`
    /// (`<id>:<secret>`, minted by `grovefs apikey create --for mount`) on the
    /// `OP_OPEN` handshake — the credential form a `data-server
    /// --require-api-key` demands, so access is per-caller and revocable
    /// instead of one static cluster token. The server re-verifies the key while
    /// this connection lives, so a revoke reaches established mounts too (see
    /// [`crate::apikeys::KEY_RECHECK`]).
    pub fn new_with_creds(addr: &str, token: &str, api_key: &str) -> io::Result<Self> {
        let this = Self {
            addr: addr.to_string(),
            token: token.to_string(),
            api_key: api_key.to_string(),
            opened: AtomicBool::new(false),
            pool: Arc::new(Mutex::new(Vec::new())),
            max_conns: 64,
            connect_timeout: Duration::from_secs(5),
            connect_retries: 6,
            reconnect_backoff: Duration::from_millis(150),
            fence_epoch: Arc::new(AtomicU64::new(0)),
            fence_refresh: Arc::new(|| 0),
        };
        // Probe once: TCP connect + OP_OPEN handshake, so mount fails fast on a
        // dead or unauthorized server.
        let (s, _) = this.connect_fresh()?;
        this.opened.store(true, Ordering::SeqCst);
        this.put_conn(s);
        Ok(this)
    }

    /// Attach the fencing handle + refresher (shared with the meta client). Every
    /// write-intent op then stamps the live epoch so the data-server arbiter can
    /// refuse a stale primary's writes, and a fenced write refreshes the epoch
    /// from the authority and retries once. Non-HA callers never invoke this, so
    /// the epoch stays 0 and the gate stays inert.
    pub fn with_fence(
        mut self,
        epoch: Arc<AtomicU64>,
        refresh: Arc<dyn Fn() -> u64 + Send + Sync>,
    ) -> Self {
        self.fence_epoch = epoch;
        self.fence_refresh = refresh;
        self
    }

    /// Send a write-intent op: prepend the live fencing epoch, and on an `EROFS`
    /// fence verdict refresh the epoch from the authority once and retry. Reads
    /// never route through here (they carry no epoch). All `payload` bytes are
    /// exactly the pre-epoch body `dispatch` expects.
    fn round_trip_write(&self, op: u8, payload: &[u8]) -> io::Result<(i32, u64, Vec<u8>)> {
        let mut body = Vec::with_capacity(8 + payload.len());
        proto::put_u64(&mut body, self.fence_epoch.load(Ordering::SeqCst));
        body.extend_from_slice(payload);
        let first = self.round_trip(op, &body)?;
        if first.0 != EROFS {
            return Ok(first);
        }
        // Fenced: our stamped token lagged the arbiter's. Pull the live epoch
        // (reconnects to the current primary, refreshing the shared handle) and
        // retry once. If still fenced (e.g. mid-failover) the EROFS propagates and
        // the caller surfaces a read-only error until it fails over.
        let fresh = (self.fence_refresh)();
        self.fence_epoch.store(fresh, Ordering::SeqCst);
        let mut body2 = Vec::with_capacity(8 + payload.len());
        proto::put_u64(&mut body2, fresh);
        body2.extend_from_slice(payload);
        self.round_trip(op, &body2)
    }

    /// Borrow a connection: prefer a warm pooled socket (`reused = true`), else
    /// open a fresh one (see [`Self::connect_fresh`], `reused = false`).
    fn get_conn(&self) -> io::Result<(TcpStream, bool)> {
        if let Some(s) = self.pool.lock().pop() {
            return Ok((s, true));
        }
        self.connect_fresh()
    }

    /// Open a brand-new socket, retrying with a small bounded backoff so a
    /// mid-restart data-server is tolerated rather than surfacing an instant
    /// ECONNREFUSED. Runs the mandatory `OP_OPEN` handshake before returning,
    /// so the socket is ready to serve data-plane requests. An auth rejection
    /// (server replies `EACCES`) fails fast — retrying can't fix a wrong secret
    /// — and carries the server's refusal text (in the response payload) so an
    /// operator sees *which* half failed (missing key, wrong secret, revoked).
    fn connect_fresh(&self) -> io::Result<(TcpStream, bool)> {
        let mut last_err = io::Error::new(io::ErrorKind::ConnectionRefused, "no connect attempt");
        for attempt in 0..self.connect_retries {
            match TcpStream::connect(&self.addr) {
                Ok(s) => {
                    s.set_read_timeout(Some(self.connect_timeout))?;
                    s.set_write_timeout(Some(self.connect_timeout))?;
                    s.set_nodelay(true)?; // LAN: avoid Nagle delaying small request frames
                    let mut body = Vec::new();
                    proto::put_key(&mut body, &self.token);
                    proto::put_key(&mut body, &self.api_key);
                    let mut s = s;
                    match Self::call(&mut s, proto::OP_OPEN, &body) {
                        Ok((0, _, _)) => return Ok((s, false)),
                        Ok((EACCES, _, reason)) => {
                            let why =
                                refusal_text(&reason, "data-server rejected the access token");
                            // A refusal AFTER the mount was serving means the
                            // credential died mid-session (revoked): the mount
                            // cannot serve legitimately, so ask for a clean
                            // unmount instead of a live-but-refused view.
                            if self.opened.load(Ordering::SeqCst) {
                                super::signal_auth_loss(&why);
                            }
                            return Err(io::Error::new(io::ErrorKind::PermissionDenied, why));
                        }
                        Ok((_, _, _)) => {
                            last_err = io::Error::from_raw_os_error(EIO);
                        }
                        Err(e) => last_err = e,
                    }
                }
                Err(e) => {
                    last_err = e;
                }
            }
            if attempt + 1 < self.connect_retries {
                thread::sleep(self.reconnect_backoff);
            }
        }
        Err(last_err)
    }

    fn put_conn(&self, s: TcpStream) {
        let mut p = self.pool.lock();
        if p.len() < self.max_conns {
            p.push(s);
        }
        // else: dropped, letting the pool shrink under low concurrency
    }

    /// Send one request frame and read the matching response.
    ///
    /// Resilience: a data-server restart invalidates the entire connection pool
    /// at once. If the socket we used was a *reused* pooled one and the
    /// round-trip fails at the transport level, we wipe the pool and retry the
    /// operation once on a brand-new connection so the caller recovers
    /// transparently. A failure on a socket we just opened is propagated (there
    /// is nothing left to retry against).
    fn round_trip(&self, op: u8, body: &[u8]) -> io::Result<(i32, u64, Vec<u8>)> {
        let (mut s, reused) = {
            let _t = stats::start(stats::Stage::IoRemotePoolWait);
            self.get_conn()?
        };
        let rtt = match op {
            proto::OP_WRITE => stats::Stage::IoRemoteWrite,
            proto::OP_LEN => stats::Stage::IoRemoteLen,
            _ => stats::Stage::IoRemoteRead,
        };
        let first = {
            let _t = stats::start(rtt);
            Self::call(&mut s, op, body)
        };
        match first {
            Ok(r) => {
                self.note_refusal(&r);
                self.put_conn(s);
                Ok(r)
            }
            Err(first_err) => {
                drop(s); // never hand a dead socket back to the pool
                if !reused {
                    // Just-opened socket failed: the peer is really down.
                    return Err(first_err);
                }
                // Reused socket failed => stale pool. Clear it, reconnect, retry once.
                self.pool.lock().clear();
                let (mut s2, _) = self.connect_fresh()?;
                match Self::call(&mut s2, op, body) {
                    Ok(r) => {
                        self.note_refusal(&r);
                        self.put_conn(s2);
                        Ok(r)
                    }
                    Err(e2) => Err(e2),
                }
            }
        }
    }

    /// An `EACCES` on a DATA op (not a handshake) is only ever an auth verdict:
    /// `dispatch` reports storage failures as `ENOENT`/`EIO`/`EROFS`/`ENOSYS`
    /// and never `EACCES`, so this can only be the server's live-credential
    /// re-check refusing us — a revoke landing on this established connection.
    /// The op still fails normally for the caller (`EACCES`), and the mount
    /// tears itself down rather than serving a view it can no longer back.
    fn note_refusal(&self, r: &(i32, u64, Vec<u8>)) {
        if r.0 != EACCES || !self.opened.load(Ordering::SeqCst) {
            return;
        }
        let why = refusal_text(&r.2, "data-server revoked this connection's api key");
        super::signal_auth_loss(&why);
    }

    /// One send + one recv on a borrowed socket.
    fn call(s: &mut TcpStream, op: u8, body: &[u8]) -> io::Result<(i32, u64, Vec<u8>)> {
        proto::send_request(s, op, body)?;
        proto::recv_response(s)
    }

    /// One `OP_READ` slice, on its own pooled connection. Short reads (EOF)
    /// zero-pad to the full slice length so stitched results keep the same
    /// exactly-`len` semantics as the single-request path.
    fn read_slice(&self, key: &str, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let mut body = Vec::new();
        proto::put_key(&mut body, key);
        proto::put_u64(&mut body, offset);
        proto::put_u32(&mut body, len as u32);
        let (status, _, mut data) = self.round_trip(proto::OP_READ, &body)?;
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status));
        }
        data.resize(len, 0);
        Ok(data)
    }

    /// Parallel large read: [offset, offset+len) sliced at READ_CHUNK, each
    /// slice fetched concurrently over the connection pool via scoped threads,
    /// concatenated in offset order. Any failed slice fails the whole read
    /// (each slice already enjoyed its own one-shot stale-pool retry inside
    /// `round_trip`).
    fn read_chunked(&self, key: &str, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let _t = stats::start(stats::Stage::IoRemoteChunked);
        let slices: Vec<(u64, usize)> = (0..len)
            .step_by(READ_CHUNK)
            .map(|i| (offset + i as u64, (len - i).min(READ_CHUNK)))
            .collect();
        // Each worker returns its own indexed pieces (stride-partitioned, so
        // slots are disjoint) — no shared mutable state, no lock on the hot path.
        let slices = &slices;
        let stitched = thread::scope(|sc| {
            let nworkers = slices.len().min(MAX_PARALLEL_SLICES);
            let mut handles = Vec::with_capacity(nworkers);
            for w in 0..nworkers {
                handles.push(sc.spawn(move || {
                    let mut mine: Vec<(usize, Vec<u8>)> = Vec::new();
                    let mut res: Option<io::Error> = None;
                    for i in (w..slices.len()).step_by(nworkers) {
                        let (soff, slen) = slices[i];
                        match self.read_slice(key, soff, slen) {
                            Ok(data) => mine.push((i, data)),
                            Err(e) => {
                                if res.is_none() {
                                    res = Some(e);
                                }
                            }
                        }
                    }
                    (mine, res)
                }));
            }
            let mut first_err: Option<io::Error> = None;
            let mut all: Vec<(usize, Vec<u8>)> = Vec::with_capacity(slices.len());
            for h in handles {
                match h.join() {
                    Ok((mine, maybe_err)) => {
                        all.extend(mine);
                        if let Some(e) = maybe_err {
                            if first_err.is_none() {
                                first_err = Some(e);
                            }
                        }
                    }
                    // A worker panicked (outside our control): surface EIO
                    // rather than pretending EOF.
                    Err(_) => {
                        if first_err.is_none() {
                            first_err = Some(io::Error::from_raw_os_error(EIO));
                        }
                    }
                }
            }
            match first_err {
                Some(e) => Err(e),
                None => Ok(all),
            }
        })?;
        let mut stitched = stitched;
        // Workers returned their pieces interleaved by their own stride order;
        // sort by slice index so the concatenation is exactly offset-ordered.
        stitched.sort_by_key(|(i, _)| *i);
        let mut out = Vec::with_capacity(len);
        for (_, d) in stitched {
            out.extend_from_slice(&d);
        }
        Ok(out)
    }

    /// Parallel large write: data sliced at WRITE_CHUNK, slices shipped
    /// concurrently to independent offsets (positional `pwrite` on the server
    /// makes same-file parallel writes safe). Returns the total accepted bytes.
    fn write_chunked(&self, key: &str, offset: u64, data: &[u8]) -> io::Result<usize> {
        let _t = stats::start(stats::Stage::IoRemoteChunked);
        let slices: Vec<(u64, &[u8])> = data
            .chunks(WRITE_CHUNK)
            .enumerate()
            .map(|(i, c)| (offset + (i * WRITE_CHUNK) as u64, c))
            .collect();
        let slices = &slices;
        let totals = thread::scope(|sc| {
            let nworkers = slices.len().min(MAX_PARALLEL_SLICES);
            let mut handles = Vec::with_capacity(nworkers);
            for w in 0..nworkers {
                handles.push(sc.spawn(move || {
                    let mut mine: Vec<usize> = Vec::new();
                    let mut res: Option<io::Error> = None;
                    for i in (w..slices.len()).step_by(nworkers) {
                        let (soff, slice) = slices[i];
                        match self.write_one(key, soff, slice) {
                            Ok(n) => mine.push(n),
                            Err(e) => {
                                if res.is_none() {
                                    res = Some(e);
                                }
                            }
                        }
                    }
                    (mine, res)
                }));
            }
            let mut first_err: Option<io::Error> = None;
            let mut all: Vec<usize> = Vec::with_capacity(slices.len());
            for h in handles {
                match h.join() {
                    Ok((mine, maybe_err)) => {
                        all.extend(mine);
                        if let Some(e) = maybe_err {
                            if first_err.is_none() {
                                first_err = Some(e);
                            }
                        }
                    }
                    Err(_) => {
                        if first_err.is_none() {
                            first_err = Some(io::Error::from_raw_os_error(EIO));
                        }
                    }
                }
            }
            match first_err {
                Some(e) => Err(e),
                None => Ok(all),
            }
        })?;
        // Each slice's OP_WRITE is atomic server-side; short writes surface as
        // an error from write_one, so any total != slice length fails there.
        Ok(totals.into_iter().sum())
    }

    /// Single `OP_WRITE` for one slice (used by the chunked path; the public
    /// `write` entry routes here directly for small payloads).
    fn write_one(&self, key: &str, offset: u64, data: &[u8]) -> io::Result<usize> {
        let mut body = Vec::with_capacity(2 + key.len() + 8 + data.len());
        proto::put_key(&mut body, key);
        proto::put_u64(&mut body, offset);
        body.extend_from_slice(data);
        let (status, val, _) = self.round_trip_write(proto::OP_WRITE, &body)?;
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status));
        }
        if val as usize != data.len() {
            return Err(io::Error::from_raw_os_error(EIO));
        }
        Ok(val as usize)
    }
}

impl StorageBackend for RemoteBackend {
    fn create(&self, key: &str) -> io::Result<()> {
        let mut body = Vec::new();
        proto::put_key(&mut body, key);
        let (status, _, _) = self.round_trip_write(proto::OP_CREATE, &body)?;
        status_to_io(status)
    }

    fn read(&self, key: &str, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        if len >= CHUNKED_READ_MIN {
            return self.read_chunked(key, offset, len);
        }
        let mut body = Vec::new();
        proto::put_key(&mut body, key);
        proto::put_u64(&mut body, offset);
        proto::put_u32(&mut body, len as u32);
        let (status, _, data) = self.round_trip(proto::OP_READ, &body)?;
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status));
        }
        Ok(data)
    }

    fn write(&self, key: &str, offset: u64, data: &[u8]) -> io::Result<usize> {
        if data.len() >= CHUNKED_WRITE_MIN {
            return self.write_chunked(key, offset, data);
        }
        let mut body = Vec::with_capacity(2 + key.len() + 8 + data.len());
        proto::put_key(&mut body, key);
        proto::put_u64(&mut body, offset);
        body.extend_from_slice(data);
        let (status, val, _) = self.round_trip_write(proto::OP_WRITE, &body)?;
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status));
        }
        Ok(val as usize)
    }

    fn len(&self, key: &str) -> io::Result<u64> {
        let mut body = Vec::new();
        proto::put_key(&mut body, key);
        let (status, val, _) = self.round_trip(proto::OP_LEN, &body)?;
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status));
        }
        Ok(val)
    }

    fn fsync(&self, key: &str) -> io::Result<()> {
        let mut body = Vec::new();
        proto::put_key(&mut body, key);
        let (status, _, _) = self.round_trip_write(proto::OP_FSYNC, &body)?;
        status_to_io(status)
    }

    fn truncate(&self, key: &str, new_len: u64) -> io::Result<()> {
        let mut body = Vec::new();
        proto::put_key(&mut body, key);
        proto::put_u64(&mut body, new_len);
        let (status, _, _) = self.round_trip_write(proto::OP_TRUNC, &body)?;
        status_to_io(status)
    }

    fn remove(&self, key: &str) -> io::Result<()> {
        let mut body = Vec::new();
        proto::put_key(&mut body, key);
        let (status, _, _) = self.round_trip_write(proto::OP_REMOVE, &body)?;
        // Removing a missing object is not an error for GC purposes.
        if status == ENOENT {
            return Ok(());
        }
        status_to_io(status)
    }

    fn statvfs(&self) -> io::Result<FsStat> {
        // No key needed; server returns 7 big-endian u64 in the payload.
        let (status, _, data) = self.round_trip(proto::OP_STATFS, &[])?;
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status));
        }
        let field = |i: usize| -> u64 {
            if (i + 1) * 8 <= data.len() {
                u64::from_be_bytes(data[i * 8..i * 8 + 8].try_into().unwrap())
            } else {
                0
            }
        };
        Ok(FsStat {
            tblocks: field(0),
            bfree: field(1),
            bavail: field(2),
            files: field(3),
            ffree: field(4),
            bsize: field(5),
            namemax: field(6) as u32,
        })
    }
}

fn status_to_io(status: i32) -> io::Result<()> {
    if status == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(if status == 0 {
            EIO
        } else {
            status
        }))
    }
}

/// Decode an auth-refusal payload into a message. An old server sends an empty
/// body, so the fallback text keeps the pre-existing wording.
fn refusal_text(data: &[u8], fallback: &str) -> String {
    if data.is_empty() {
        fallback.to_string()
    } else {
        String::from_utf8_lossy(data).into_owned()
    }
}

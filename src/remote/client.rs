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
use crate::backend::{FsStat, ObjectPage, ObjectRow, StorageBackend};
use crate::platform::{EACCES, EIO, ENOENT, EROFS};
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
    /// The member id this data-server reported in its `OP_OPEN` ack, captured on
    /// the first successful handshake (the id is stable, so later reconnects are
    /// ignored). `u64::MAX` means the server was started without `--member-id`
    /// and therefore advertises no identity — [`Self::placement_key`] then
    /// returns `None` and a stripe places this member by list position.
    member_id: std::sync::OnceLock<u64>,
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
            member_id: std::sync::OnceLock::new(),
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
    /// never route through here (they carry no epoch).
    fn round_trip_write(&self, op: u8, payload: &[u8]) -> io::Result<(i32, u64, Vec<u8>)> {
        self.round_trip_write_parts(op, &[payload])
    }

    /// [`Self::round_trip_write`] for a body supplied as slices: the epoch is
    /// prepended as a separate part, so a bulk `OP_WRITE` payload is never
    /// copied just to make room for the 8-byte prologue.
    fn round_trip_write_parts(
        &self,
        op: u8,
        parts: &[&[u8]],
    ) -> io::Result<(i32, u64, Vec<u8>)> {
        let epoch = self.fence_epoch.load(Ordering::SeqCst).to_be_bytes();
        let mut framed: Vec<&[u8]> = Vec::with_capacity(parts.len() + 1);
        framed.push(&epoch);
        framed.extend_from_slice(parts);
        let first = self.round_trip_parts(op, &framed)?;
        if first.0 != EROFS {
            return Ok(first);
        }
        // Fenced: our stamped token lagged the arbiter's. Pull the live epoch
        // (reconnects to the current primary, refreshing the shared handle) and
        // retry once. If still fenced (e.g. mid-failover) the EROFS propagates and
        // the caller surfaces a read-only error until it fails over.
        let fresh = (self.fence_refresh)();
        self.fence_epoch.store(fresh, Ordering::SeqCst);
        let epoch2 = fresh.to_be_bytes();
        let mut framed2: Vec<&[u8]> = Vec::with_capacity(parts.len() + 1);
        framed2.push(&epoch2);
        framed2.extend_from_slice(parts);
        self.round_trip_parts(op, &framed2)
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
                        Ok((0, val, _)) => {
                            // Latch the server's stable member id from the ack.
                            // First handshake wins; the value never changes, so
                            // reconnects harmlessly fail the `set` and are
                            // dropped. `u64::MAX` = server started with no
                            // `--member-id` (positional placement).
                            let _ = self.member_id.set(val);
                            return Ok((s, false));
                        }
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
                    // Name the unreachable data-server so a mount/read failure on
                    // a dead member reads as "cannot reach data-server <addr>:
                    // Connection refused" rather than a bare errno (§5-12);
                    // ErrorKind is preserved for the retry logic above.
                    last_err = io::Error::new(
                        e.kind(),
                        format!("cannot reach data-server {}: {e}", self.addr),
                    );
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
        self.round_trip_parts(op, &[body])
    }

    /// [`Self::round_trip`] with the body supplied as slices, so a bulk payload
    /// rides in the same `writev` as the header instead of being copied into a
    /// frame buffer first.
    fn round_trip_parts(&self, op: u8, parts: &[&[u8]]) -> io::Result<(i32, u64, Vec<u8>)> {
        self.with_socket(rtt_stage(op), |s| {
            let r = Self::call_parts(s, op, parts)?;
            self.note_refusal(&r);
            Ok(r)
        })
    }

    /// [`Self::round_trip_parts`] with the response payload read STRAIGHT into
    /// `dest`, returning `(status, val, n)` (see
    /// [`proto::recv_response_into`]). This is the chunked-read path: each slice
    /// lands in its final position of the caller's buffer, so the payload is
    /// never staged in a `Vec` and then copied again to be stitched.
    fn round_trip_parts_into(
        &self,
        op: u8,
        parts: &[&[u8]],
        dest: &mut [u8],
    ) -> io::Result<(i32, u64, usize)> {
        self.with_socket(rtt_stage(op), |s| {
            let r = Self::call_parts_into(s, op, parts, dest)?;
            if r.0 == EACCES {
                // Same live-revoke verdict the Vec path reports; the refusal text
                // is the short payload we just landed at the front of `dest`.
                self.note_refusal(&(r.0, r.1, dest[..r.2].to_vec()));
            }
            Ok(r)
        })
    }

    /// Shared send/recv skeleton for every round trip: borrow a pooled socket,
    /// run `call`, return the socket on success.
    ///
    /// Resilience lives here so all callers inherit it: a *reused* pooled socket
    /// that fails at the transport level means the data-server restarted and the
    /// pool is stale — wipe it, reconnect, and retry exactly once. A failure on
    /// a socket we just opened is propagated (nothing left to retry against).
    fn with_socket<T>(
        &self,
        stage: stats::Stage,
        mut call: impl FnMut(&mut TcpStream) -> io::Result<T>,
    ) -> io::Result<T> {
        let (mut s, reused) = {
            let _t = stats::start(stats::Stage::IoRemotePoolWait);
            self.get_conn()?
        };
        let first = {
            let _t = stats::start(stage);
            call(&mut s)
        };
        match first {
            Ok(r) => {
                self.put_conn(s);
                Ok(r)
            }
            Err(first_err) => {
                drop(s); // never hand a dead socket back to the pool
                if !reused {
                    return Err(first_err);
                }
                // Reused socket failed => stale pool. Clear it, reconnect, retry once.
                self.pool.lock().clear();
                let (mut s2, _) = self.connect_fresh()?;
                let r = {
                    let _t = stats::start(stage);
                    call(&mut s2)
                }?;
                self.put_conn(s2);
                Ok(r)
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
        Self::call_parts(s, op, &[body])
    }

    /// One send + one recv on a borrowed socket, body supplied as slices.
    fn call_parts(
        s: &mut TcpStream,
        op: u8,
        parts: &[&[u8]],
    ) -> io::Result<(i32, u64, Vec<u8>)> {
        proto::send_request_parts(s, op, parts)?;
        proto::recv_response(s)
    }

    /// One send + one recv on a borrowed socket, landing the payload in `dest`.
    fn call_parts_into(
        s: &mut TcpStream,
        op: u8,
        parts: &[&[u8]],
        dest: &mut [u8],
    ) -> io::Result<(i32, u64, usize)> {
        proto::send_request_parts(s, op, parts)?;
        proto::recv_response_into(s, dest)
    }

    /// One `OP_READ` slice read STRAIGHT into `dest` (its final position in the
    /// caller's buffer), on its own pooled connection. Short reads (EOF)
    /// zero-pad `dest[n..]`, preserving the exactly-`len` slice semantics the
    /// previous Vec-then-resize path had.
    fn read_into(&self, key: &str, offset: u64, dest: &mut [u8]) -> io::Result<()> {
        let mut body = Vec::with_capacity(2 + key.len() + 12);
        proto::put_key(&mut body, key);
        proto::put_u64(&mut body, offset);
        proto::put_u32(&mut body, dest.len() as u32);
        let (status, _, n) = self.round_trip_parts_into(proto::OP_READ, &[&body], dest)?;
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status));
        }
        // A stale-pool retry may have left bytes from the failed attempt behind
        // `n`; a short tail must read as zeros, not as stale payload.
        dest[n..].fill(0);
        Ok(())
    }

    /// Parallel large read: [offset, offset+len) sliced at READ_CHUNK, each
    /// slice fetched concurrently over the connection pool via scoped threads.
    /// Every slice is read DIRECTLY into its slot of one `len`-byte result
    /// buffer — the boundaries are exactly `out.chunks_mut(READ_CHUNK)`, so no
    /// per-slice `Vec` and no stitching copy exist. Any failed slice fails the
    /// whole read (each slice already enjoyed its own one-shot stale-pool retry
    /// inside [`Self::round_trip_parts_into`]).
    fn read_chunked(&self, key: &str, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let _t = stats::start(stats::Stage::IoRemoteChunked);
        let mut out = vec![0u8; len];
        let nslices = len.div_ceil(READ_CHUNK);
        let nworkers = nslices.min(MAX_PARALLEL_SLICES);
        // Hand each worker the slots it will fill at its own stride — the same
        // partition the old slice list used, so per-worker ordering (and which
        // error surfaces first) is unchanged.
        let mut groups: Vec<Vec<(usize, &mut [u8])>> = (0..nworkers).map(|_| Vec::new()).collect();
        for (i, chunk) in out.chunks_mut(READ_CHUNK).enumerate() {
            groups[i % nworkers].push((i, chunk));
        }
        let first_err = thread::scope(|sc| {
            let mut handles = Vec::with_capacity(nworkers);
            for group in groups {
                handles.push(sc.spawn(move || {
                    let mut first: Option<io::Error> = None;
                    for (i, dest) in group {
                        let soff = offset + (i * READ_CHUNK) as u64;
                        if let Err(e) = self.read_into(key, soff, dest) {
                            // One bad slice fails the whole read; stop issuing
                            // the rest of this worker's slices.
                            first = Some(e);
                            break;
                        }
                    }
                    first
                }));
            }
            let mut first_err: Option<io::Error> = None;
            for h in handles {
                match h.join() {
                    Ok(Some(e)) => {
                        if first_err.is_none() {
                            first_err = Some(e);
                        }
                    }
                    Ok(None) => {}
                    // A worker panicked (outside our control): surface EIO
                    // rather than pretending EOF.
                    Err(_) => {
                        if first_err.is_none() {
                            first_err = Some(io::Error::from_raw_os_error(EIO));
                        }
                    }
                }
            }
            first_err
        });
        match first_err {
            Some(e) => Err(e),
            None => Ok(out),
        }
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
    /// `write` entry routes here directly for small payloads). The key/offset
    /// prologue is a tiny buffer; the payload slice is shipped as its own
    /// part, so its bytes are never copied client-side.
    fn write_one(&self, key: &str, offset: u64, data: &[u8]) -> io::Result<usize> {
        let mut head = Vec::with_capacity(2 + key.len() + 8);
        proto::put_key(&mut head, key);
        proto::put_u64(&mut head, offset);
        let (status, val, _) = self.round_trip_write_parts(proto::OP_WRITE, &[&head, data])?;
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status));
        }
        if val as usize != data.len() {
            return Err(io::Error::from_raw_os_error(EIO));
        }
        Ok(val as usize)
    }

    /// Batch stat: returns lengths for all keys in one round-trip.
    /// A missing key yields 0 (does not abort the batch).
    pub fn batch_len(&self, keys: &[String]) -> io::Result<Vec<u64>> {
        let mut body = Vec::with_capacity(4 + keys.len() * 40);
        proto::put_u32(&mut body, keys.len() as u32);
        for k in keys {
            proto::put_key(&mut body, k);
        }
        let (status, _, data) = self.round_trip(proto::OP_BATCH_LEN, &body)?;
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status));
        }
        let sizes: Vec<u64> = data
            .as_chunks::<8>()
            .0
            .iter()
            .map(|c| u64::from_be_bytes(*c))
            .collect();
        if sizes.len() != keys.len() {
            return Err(io::Error::other("batch_len: response count mismatch"));
        }
        Ok(sizes)
    }
}

impl StorageBackend for RemoteBackend {
    /// Report the member id this data-server carried in its `OP_OPEN` ack, so a
    /// [`crate::backend::ShardedBackend`] can pin chunk placement to this
    /// server's identity instead of its position in `--data-servers`. `None`
    /// when the server runs without `--member-id` (`u64::MAX` sentinel), which
    /// keeps the historical positional placement.
    fn placement_key(&self) -> Option<u64> {
        self.member_id
            .get()
            .copied()
            .filter(|v| *v != proto::NO_MEMBER_ID)
    }

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
        self.write_one(key, offset, data)
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

    fn list_objects(&self, cursor: Option<&str>, limit: usize) -> io::Result<ObjectPage> {
        // The cursor is shipped verbatim: its meaning belongs to the server's
        // storage layout, and this side only knows "keep going with this".
        let mut body = Vec::new();
        proto::put_u32(&mut body, limit as u32);
        proto::put_key(&mut body, cursor.unwrap_or(""));
        let (status, _, data) = self.round_trip(proto::OP_LIST, &body)?;
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status));
        }
        let mut i = 0usize;
        let n = proto::read_u32(&data, &mut i)? as usize;
        let mut rows = Vec::with_capacity(n);
        for _ in 0..n {
            let key = proto::get_key(&data, &mut i)?;
            let size = proto::read_u64(&data, &mut i)?;
            let mtime_secs = proto::read_u64(&data, &mut i)? as i64;
            rows.push(ObjectRow {
                key,
                size,
                mtime_secs,
            });
        }
        let next = proto::get_key(&data, &mut i)?;
        Ok(ObjectPage {
            rows,
            next: if next.is_empty() { None } else { Some(next) },
        })
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

/// Which `--stats` stage a round trip is booked under, by opcode (the same
/// mapping every round trip used before the paths were unified).
fn rtt_stage(op: u8) -> stats::Stage {
    match op {
        proto::OP_WRITE => stats::Stage::IoRemoteWrite,
        proto::OP_LEN => stats::Stage::IoRemoteLen,
        _ => stats::Stage::IoRemoteRead,
    }
}

/// Decode an auth-refusal payload into a message. A refusal with an empty body
/// cannot name its cause, so the caller's generic wording is used instead.
fn refusal_text(data: &[u8], fallback: &str) -> String {
    if data.is_empty() {
        fallback.to_string()
    } else {
        String::from_utf8_lossy(data).into_owned()
    }
}

//! Remote metadata path: the view tree served by a central [`MetaServer`] and
//! consumed by clients through [`RemoteMeta`], which implements
//! [`crate::meta::MetaProvider`]. This is the metadata half of the "central
//! store, clients mount over LAN" topology — the analogue of [`super::server`]
//! for the *data* plane.
//!
//! Why this exists: with the view database living on each client, two machines
//! mounting the same view get two independent copies that diverge. Pointing both
//! at one `MetaServer` collapses them onto a single authoritative SQLite tree,
//! so a folder one client creates is instantly the same view everywhere, while
//! per-view isolation still holds (the server keeps one engine per view).
//!
//! Wire framing reuses [`super::proto`] (fixed header + variable body, big-endian
//! integers, one request/response per socket). Metadata adds a connection-level
//! `M_OPEN` handshake that binds a socket to a view. Responses reuse the
//! `(status, val, data)` triple: for metadata `status` is 0 on success or a small
//! [`MetaError`] code (1..7) — *not* an errno — and `data` carries serialized
//! [`NodeInfo`].

use super::proto;
use crate::meta::{LockConflict, LockType, MetaEngine, MetaError, MetaProvider, MetaStore, NodeInfo, NodeKind};
use fuser::{INodeNo, Notifier};
use libc::EIO;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::{self, BufReader, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

// --- op codes (distinct from the data-plane OP_* in proto.rs) --------------
const M_OPEN: u8 = 16;
const M_LOOKUP: u8 = 17;
const M_GETATTR: u8 = 18;
const M_READDIR: u8 = 19;
const M_CREATE: u8 = 20;
const M_SYMLINK: u8 = 21;
const M_LINK: u8 = 22;
const M_UNLINK: u8 = 23;
const M_RENAME: u8 = 24;
const M_SETMODE: u8 = 25;
const M_SETUIDGID: u8 = 26;
const M_SETTIMES: u8 = 27;
const M_TOUCH: u8 = 28;
/// Turn a connection into a pure event sink: after `M_OPEN` the client sends
/// `M_SUBSCRIBE` and the server pushes invalidation frames for every mutating
/// op performed by *other* clients of the same view. The connection then issues
/// no further requests (it only reads events until it disconnects).
const M_SUBSCRIBE: u8 = 29;
/// HA replication (primary -> standby), never sent by mount clients. `M_PING`
/// is a role heartbeat carrying the sender's epoch and elicits the receiver's
/// epoch back; `M_PUSH` ships one whole `<view>.db` snapshot; `M_FORWARD`
/// relays a batch of invalidation events so clients subscribed to the standby
/// also see writes that happened on the primary (within one replication tick).
const M_PING: u8 = 30;
const M_PUSH: u8 = 31;
const M_FORWARD: u8 = 32;
/// POSIX advisory record lock ops. `M_SETLK` acquires/modifies/releases (a
/// write-intent op, so standby rejects it with `E_ROFS`); `M_GETLK` queries a
/// conflicting lock without mutating state; `M_RELEASE_LOCKS` drops every lock
/// held by an owner (driven by FUSE `release` on last close).
const M_SETLK: u8 = 33;
const M_GETLK: u8 = 34;
const M_RELEASE_LOCKS: u8 = 35;
/// Lease-renewal heartbeat: the mount daemon bumps `expires_at` on every lock
/// row tagged with its session id, so a live holder never expires while a dead
/// one (which stops renewing) ages out inside one lease.
const M_RENEW: u8 = 36;

// --- pushed invalidation event kinds (carried in a response frame's status) --
// EV_ENTRY: val = parent inode, data = length-prefixed name -> kernel
// inval_entry (drop the cached dentry so the next lookup re-fetches).
// EV_INODE: val = inode -> kernel inval_inode over the whole file, which
// invalidates BOTH the cached attributes (size/mtime) and the page cache, so a
// peer's write to an already-open file becomes visible without waiting a TTL.
const EV_ENTRY: i32 = 0;
const EV_INODE: i32 = 1;

// --- MetaError <-> wire status code (1..7; 0 reserved for success) ---------
const E_EXISTS: i32 = 1;
const E_NOTFOUND: i32 = 2;
const E_NOTEMPTY: i32 = 3;
const E_TYPEMISMATCH: i32 = 4;
const E_NOTPERM: i32 = 5;
const E_CYCLE: i32 = 6;
const E_DB: i32 = 7;
// Returned by the server on the M_OPEN handshake when the presented token does
// not match the server's configured one. Not a MetaError code — it only ever
// appears during connect, where the client maps it to a PermissionDenied error.
const E_AUTH: i32 = 8;
// Returned by a HA *standby* (or a demoted ex-primary) for any write-intent op
// while it is read-only. Not a MetaError code; the client reads it as a signal
// to fail over to its backup address rather than as data corruption.
const E_ROFS: i32 = 9;
// Returned on the HA control channel (node <-> node) when the peer carries a
// higher epoch than the sender: the sender is a stale ex-primary and must
// demote itself to standby. Node-level fencing — mount clients never see this
// status (they fail over via E_ROFS / transport errors, not epoch).
const E_EPOCH: i32 = 10;

/// Write-intent ops: everything from `M_CREATE` through `M_TOUCH`, plus the
/// lock-mutating ops `M_SETLK` / `M_RELEASE_LOCKS` (they INSERT/DELETE on the
/// `locks` table). A read-only HA node refuses these with `E_ROFS`; reads
/// (`LOOKUP`/`GETATTR`/`READDIR`/`M_GETLK`), `M_OPEN`, `M_SUBSCRIBE` and the
/// replication ops are always served.
fn is_write_op(op: u8) -> bool {
    (M_CREATE..=M_TOUCH).contains(&op) || op == M_SETLK || op == M_RELEASE_LOCKS || op == M_RENEW
}

// --- HA replication timing -------------------------------------------------
// A primary pushes a fresh snapshot of every view db to its standby every
// `REPL_INTERVAL`; the standby's watchdog promotes itself if the primary's
// heartbeats stop for `PROMOTE_TIMEOUT`. RPO ~= REPL_INTERVAL (see the docs on
// `start_replicator`/`start_promotion_watchdog` for the honest loss window).
const REPL_INTERVAL: Duration = Duration::from_millis(500);
const PROMOTE_TIMEOUT: Duration = Duration::from_secs(3);
const WATCHDOG_INTERVAL: Duration = Duration::from_millis(250);
const CONTROL_TIMEOUT: Duration = Duration::from_secs(2);

/// Fail-safe grace before a `--witness` node that cannot reach the arbiter
/// gives up its write authority. Set to `PROMOTE_TIMEOUT`: that is the window in
/// which a partitioned peer could already have promoted past us, so once we have
/// been unable to *prove* exclusive authority at the witness for that long we
/// stand down rather than risk a double-write. Kept at parity with promotion so
/// the two sides of the fence agree on the same upper bound.
const WITNESS_FENCE_GRACE: Duration = PROMOTE_TIMEOUT;

/// Arbiter lease length a node requests at the witness. A primary renews it in
/// the [`REPL_INTERVAL`] replicator tick, so in steady state it is always fresh
/// and a mutating op is admitted by a cheap LOCAL validity check instead of a
/// per-write arbiter round-trip. Equal to [`WITNESS_FENCE_GRACE`] so the arbiter
/// lease and the async fail-safe bound the same window.
const WITNESS_LEASE: Duration = WITNESS_FENCE_GRACE;

/// Safety margin subtracted from the lease when a node tracks its expiry on its
/// OWN clock: a holder stops trusting its cached lease [`WITNESS_LEASE_SLACK`]
/// before the arbiter would let a peer claim it, covering bounded host clock skew
/// plus one round-trip. Far shorter than the lease, so it only trims the tail.
const WITNESS_LEASE_SLACK: Duration = Duration::from_millis(500);

/// Per-arbiter timeout for the SYNCHRONOUS re-proof on the latency-critical write
/// path ([`fenced_for_write`], taken only once the local lease has already
/// expired). Shorter than [`CONTROL_TIMEOUT`] so a blackholed arbiter cannot add a
/// full background round-trip to a view's write; the fan-out is parallel, hence the
/// whole quorum re-proof is bounded by this single value. A timed-out probe simply
/// counts as *unreachable* (never as fenced), so safety is unchanged — only the
/// stall shrinks. Background callers (replicator tick, watchdog promotion, startup
/// register) keep the generous [`CONTROL_TIMEOUT`] since latency there is invisible
/// to writes. Also bounds `connect`, which is otherwise unbounded.
const WITNESS_PROBE_TIMEOUT: Duration = Duration::from_millis(1000);

fn err_code(e: &MetaError) -> i32 {
    match e {
        MetaError::Exists => E_EXISTS,
        MetaError::NotFound => E_NOTFOUND,
        MetaError::NotEmpty => E_NOTEMPTY,
        MetaError::TypeMismatch => E_TYPEMISMATCH,
        MetaError::OpNotPermitted => E_NOTPERM,
        MetaError::BadParentCycle => E_CYCLE,
        MetaError::Db(_) => E_DB,
    }
}

fn err_from_code(c: i32) -> MetaError {
    match c {
        E_EXISTS => MetaError::Exists,
        E_NOTFOUND => MetaError::NotFound,
        E_NOTEMPTY => MetaError::NotEmpty,
        E_TYPEMISMATCH => MetaError::TypeMismatch,
        E_NOTPERM => MetaError::OpNotPermitted,
        E_CYCLE => MetaError::BadParentCycle,
        _ => MetaError::Db("remote meta error".to_string()),
    }
}

// --- scalar / option helpers ------------------------------------------------
fn put_i64(buf: &mut Vec<u8>, v: i64) {
    buf.extend_from_slice(&(v as u64).to_be_bytes());
}
fn get_i64(body: &[u8], i: &mut usize) -> io::Result<i64> {
    Ok(proto::read_u64(body, i)? as i64)
}
fn put_u8(buf: &mut Vec<u8>, v: u8) {
    buf.push(v);
}
fn get_u8(body: &[u8], i: &mut usize) -> io::Result<u8> {
    if *i + 1 > body.len() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "meta: short"));
    }
    let v = body[*i];
    *i += 1;
    Ok(v)
}
/// Option<&str>: a presence byte then, if present, a length-prefixed string.
fn put_opt(buf: &mut Vec<u8>, o: Option<&str>) {
    match o {
        Some(s) => {
            buf.push(1);
            proto::put_key(buf, s);
        }
        None => buf.push(0),
    }
}
fn get_opt(body: &[u8], i: &mut usize) -> io::Result<Option<String>> {
    if get_u8(body, i)? == 1 {
        Ok(Some(proto::get_key(body, i)?))
    } else {
        Ok(None)
    }
}
fn put_opt_i64(buf: &mut Vec<u8>, o: Option<i64>) {
    match o {
        Some(v) => {
            buf.push(1);
            put_i64(buf, v);
        }
        None => buf.push(0),
    }
}
fn get_opt_i64(body: &[u8], i: &mut usize) -> io::Result<Option<i64>> {
    if get_u8(body, i)? == 1 {
        Ok(Some(get_i64(body, i)?))
    } else {
        Ok(None)
    }
}

// --- LockConflict (de)serialization ----------------------------------------
// Wire layout: [u64 start][u64 end][u8 ltype][u32 pid]. Field order MUST stay
// in sync between encode and decode.
fn encode_lock_conflict(c: &LockConflict, buf: &mut Vec<u8>) {
    proto::put_u64(buf, c.start);
    proto::put_u64(buf, c.end);
    put_u8(buf, c.ty.to_u8());
    proto::put_u32(buf, c.pid);
}
fn decode_lock_conflict(data: &[u8]) -> Option<LockConflict> {
    let mut i = 0usize;
    let start = proto::read_u64(data, &mut i).ok()?;
    let end = proto::read_u64(data, &mut i).ok()?;
    let ty = LockType::from_u8(get_u8(data, &mut i).ok()?)?;
    let pid = proto::read_u32(data, &mut i).ok()?;
    Some(LockConflict { start, end, ty, pid })
}

// --- NodeInfo (de)serialization --------------------------------------------
// Field order MUST stay in sync between put_node and node_from.
fn put_node(buf: &mut Vec<u8>, n: &NodeInfo) {
    put_i64(buf, n.id);
    put_i64(buf, n.parent_id);
    put_u8(
        buf,
        match n.kind {
            NodeKind::Dir => 0,
            NodeKind::Regular => 1,
            NodeKind::Symlink => 2,
        },
    );
    proto::put_key(buf, &n.name);
    proto::put_u32(buf, n.mode);
    proto::put_u32(buf, n.uid);
    proto::put_u32(buf, n.gid);
    put_i64(buf, n.mtime);
    put_i64(buf, n.atime);
    put_i64(buf, n.ctime);
    proto::put_u64(buf, n.nlink);
    proto::put_u64(buf, n.size);
    put_opt(buf, n.data_ref.as_deref());
    put_opt(buf, n.link_target.as_deref());
}

fn node_from(body: &[u8], i: &mut usize) -> io::Result<NodeInfo> {
    let id = get_i64(body, i)?;
    let parent_id = get_i64(body, i)?;
    let kind_raw = get_u8(body, i)?;
    let name = proto::get_key(body, i)?;
    let mode = proto::read_u32(body, i)?;
    let uid = proto::read_u32(body, i)?;
    let gid = proto::read_u32(body, i)?;
    let mtime = get_i64(body, i)?;
    let atime = get_i64(body, i)?;
    let ctime = get_i64(body, i)?;
    let nlink = proto::read_u64(body, i)?;
    let size = proto::read_u64(body, i)?;
    let data_ref = get_opt(body, i)?;
    let link_target = get_opt(body, i)?;
    let kind = match kind_raw {
        0 => NodeKind::Dir,
        2 => NodeKind::Symlink,
        _ => NodeKind::Regular,
    };
    Ok(NodeInfo {
        id,
        parent_id,
        name,
        kind,
        data_ref,
        link_target,
        mode,
        uid,
        gid,
        mtime,
        atime,
        ctime,
        nlink,
        size,
    })
}

fn put_nodes(buf: &mut Vec<u8>, nodes: &[NodeInfo]) {
    proto::put_u32(buf, nodes.len() as u32);
    for n in nodes {
        put_node(buf, n);
    }
}

fn nodes_from(data: &[u8]) -> io::Result<Vec<NodeInfo>> {
    let mut i = 0usize;
    let count = proto::read_u32(data, &mut i)? as usize;
    let mut out = Vec::with_capacity(count.min(4096));
    for _ in 0..count {
        out.push(node_from(data, &mut i)?);
    }
    Ok(out)
}

// ======================= client: RemoteMeta ================================

/// A [`MetaProvider`] that ships view-tree operations to a [`MetaServer`]. One
/// instance is bound to one view; its pooled sockets each carry that view from
/// the `M_OPEN` handshake. Connection pooling and reconnect-on-restart mirror
/// [`super::client::RemoteBackend`] — see that module for the concurrency caveat
/// (fuser's high-level session is single-threaded, so one mount drives the pool
/// serially).
pub struct RemoteMeta {
    /// Failover address list (primary first, then backups). A single address
    /// behaves exactly like the original one-node client.
    addrs: Vec<String>,
    /// Index into `addrs` of the currently-preferred node; advanced on failover
    /// when a node's retry budget is exhausted.
    active: AtomicUsize,
    view: String,
    token: String,
    /// Mount process uid presented on the `M_OPEN` handshake so the central
    /// `meta-server` can apply the optional per-view uid whitelist
    /// ([`crate::acl::AclStore']). Self-reported (advisory), not a hard identity.
    uid: u32,
    pool: Arc<Mutex<Vec<TcpStream>>>,
    max_conns: usize,
    connect_timeout: Duration,
    connect_retries: u32,
    reconnect_backoff: Duration,
    /// Shared with the data-plane client ([`super::client::RemoteBackend`]):
    /// updated to the meta-server's fencing epoch on every `M_OPEN` handshake,
    /// so the client's stamped writes track the live token even across failover.
    fence: Arc<AtomicU64>,
}

impl RemoteMeta {
    /// Connect to `addr` and complete the `M_OPEN` handshake for `view` with no
    /// token (matches a server started without one), so a bad address or an
    /// unknown-view failure surfaces at mount time.
    pub fn new(addr: &str, view: &str) -> io::Result<Self> {
        Self::new_with_token(addr, view, "")
    }

    /// The presenting uid: the mount process's real euid, reported to the
    /// server's optional view whitelist.
    fn self_uid() -> u32 {
        unsafe { libc::geteuid() }
    }

    /// Like [`RemoteMeta::new`], but presents `token` in the `M_OPEN` handshake.
    /// A server configured with a different token rejects the connection with
    /// `PermissionDenied`, so mount fails fast rather than silently running
    /// against an unauthorized view.
    ///
    /// `addr` may be a comma-separated failover list (`primary,standby`): the
    /// client sticks to the first reachable address and only falls over to a
    /// later one once an address exhausts its connect-retry budget.
    pub fn new_with_token(addr: &str, view: &str, token: &str) -> io::Result<Self> {
        Self::new_with_token_uid(addr, view, token, Self::self_uid())
    }

    /// Like [`RemoteMeta::new_with_token`], but presents an explicit `uid` on the
    /// handshake (the `mount --as-uid` override) instead of the process euid, so
    /// a shared meta-server can be exercised/used under a chosen whitelist slot.
    pub fn new_with_token_uid(
        addr: &str,
        view: &str,
        token: &str,
        uid: u32,
    ) -> io::Result<Self> {
        let this = Self {
            addrs: split_addrs(addr),
            active: AtomicUsize::new(0),
            view: view.to_string(),
            token: token.to_string(),
            uid,
            pool: Arc::new(Mutex::new(Vec::new())),
            max_conns: 32,
            connect_timeout: Duration::from_secs(5),
            connect_retries: 6,
            reconnect_backoff: Duration::from_millis(150),
            fence: Arc::new(AtomicU64::new(0)),
        };
        // Probe once (connect + handshake) so mount fails fast on a dead server.
        let (s, _) = this.connect_fresh()?;
        this.put_conn(s);
        Ok(this)
    }

    fn get_conn(&self) -> io::Result<(TcpStream, bool)> {
        if let Some(s) = self.pool.lock().pop() {
            return Ok((s, true));
        }
        self.connect_fresh()
    }

    /// Open a fresh socket, apply LAN socket options, and run the `M_OPEN`
    /// handshake so the returned socket is ready for metadata ops. Walks the
    /// failover list: it burns the full connect-retry budget on the currently
    /// preferred address (riding out a brief restart in place), and only then
    /// advances to the next address and promotes it to preferred. A token
    /// rejection is a config error, so it fails fast rather than cycling nodes.
    fn connect_fresh(&self) -> io::Result<(TcpStream, bool)> {
        let mut last_err = io::Error::new(io::ErrorKind::ConnectionRefused, "no connect attempt");
        let n = self.addrs.len();
        let start = self.active.load(Ordering::Relaxed) % n;
        for k in 0..n {
            let idx = (start + k) % n;
            let addr = &self.addrs[idx];
            for attempt in 0..self.connect_retries {
                match TcpStream::connect(addr) {
                    Ok(s) => {
                        let _ = s.set_read_timeout(Some(self.connect_timeout));
                        let _ = s.set_write_timeout(Some(self.connect_timeout));
                        let _ = s.set_nodelay(true);
                        let mut body = Vec::new();
                        proto::put_key(&mut body, &self.view);
                        proto::put_key(&mut body, &self.token);
                        proto::put_u32(&mut body, self.uid);
                        let mut s = s;
                        match Self::call(&mut s, M_OPEN, &body) {
                            Ok((0, epoch, _)) => {
                                // Cache the authoritative fencing epoch so the
                                // data-plane client stamps writes with the live
                                // token (advances automatically on failover).
                                self.fence.store(epoch, Ordering::SeqCst);
                                self.active.store(idx, Ordering::Relaxed);
                                return Ok((s, false));
                            }
                            // A rejected token is a configuration error, not a
                            // transient one — retrying or failing over can't fix it.
                            Ok((E_AUTH, _, _)) => {
                                return Err(io::Error::new(
                                    io::ErrorKind::PermissionDenied,
                                    "meta-server rejected the access token",
                                ));
                            }
                            Ok((_, _, _)) => {
                                last_err = io::Error::from_raw_os_error(EIO);
                                // server reported an open failure (e.g. bad DB)
                            }
                            Err(e) => last_err = e,
                        }
                    }
                    Err(e) => last_err = e,
                }
                if attempt + 1 < self.connect_retries {
                    thread::sleep(self.reconnect_backoff);
                }
            }
        }
        Err(last_err)
    }

    fn put_conn(&self, s: TcpStream) {
        let mut p = self.pool.lock();
        if p.len() < self.max_conns {
            p.push(s);
        }
    }

    /// Send + recv with the same stale-pool recovery as the data client: a
    /// failure on a *reused* socket clears the pool and retries once fresh.
    fn round_trip(&self, op: u8, body: &[u8]) -> io::Result<(i32, u64, Vec<u8>)> {
        let (mut s, reused) = self.get_conn()?;
        match Self::call(&mut s, op, body) {
            Ok(r) => {
                self.put_conn(s);
                Ok(r)
            }
            Err(first_err) => {
                drop(s);
                if !reused {
                    return Err(first_err);
                }
                self.pool.lock().clear();
                let (mut s2, _) = self.connect_fresh()?;
                match Self::call(&mut s2, op, body) {
                    Ok(r) => {
                        self.put_conn(s2);
                        Ok(r)
                    }
                    Err(e2) => Err(e2),
                }
            }
        }
    }

    fn call(s: &mut TcpStream, op: u8, body: &[u8]) -> io::Result<(i32, u64, Vec<u8>)> {
        proto::send_request(s, op, body)?;
        proto::recv_response(s)
    }

    // ---- response interpretation helpers ----
    fn call_node(&self, op: u8, body: &[u8]) -> Option<NodeInfo> {
        match self.round_trip(op, body) {
            Ok((0, _, data)) if !data.is_empty() => node_from(&data, &mut 0).ok(),
            _ => None,
        }
    }

    fn call_id(&self, op: u8, body: &[u8]) -> Result<i64, MetaError> {
        match self.round_trip(op, body) {
            Ok((0, val, _)) => Ok(val as i64),
            Ok((c, _, _)) => Err(err_from_code(c)),
            Err(_) => Err(MetaError::Db("remote meta transport".to_string())),
        }
    }

    fn call_status(&self, op: u8, body: &[u8]) -> Result<(), MetaError> {
        match self.round_trip(op, body) {
            Ok((0, _, _)) => Ok(()),
            Ok((c, _, _)) => Err(err_from_code(c)),
            Err(_) => Err(MetaError::Db("remote meta transport".to_string())),
        }
    }
}

impl MetaProvider for RemoteMeta {
    fn lookup(&self, parent: i64, name: &str) -> Option<NodeInfo> {
        let mut b = Vec::new();
        put_i64(&mut b, parent);
        proto::put_key(&mut b, name);
        self.call_node(M_LOOKUP, &b)
    }

    fn getattr(&self, ino: i64) -> Option<NodeInfo> {
        let mut b = Vec::new();
        put_i64(&mut b, ino);
        self.call_node(M_GETATTR, &b)
    }

    fn readdir(&self, parent: i64) -> Vec<NodeInfo> {
        let mut b = Vec::new();
        put_i64(&mut b, parent);
        match self.round_trip(M_READDIR, &b) {
            Ok((0, _, data)) => nodes_from(&data).unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn create_node(
        &self,
        parent: i64,
        name: &str,
        is_dir: bool,
        data_ref: Option<&str>,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<i64, MetaError> {
        let mut b = Vec::new();
        put_i64(&mut b, parent);
        put_u8(&mut b, is_dir as u8);
        put_opt(&mut b, data_ref);
        proto::put_u32(&mut b, mode);
        proto::put_u32(&mut b, uid);
        proto::put_u32(&mut b, gid);
        proto::put_key(&mut b, name);
        self.call_id(M_CREATE, &b)
    }

    fn create_symlink(&self, parent: i64, name: &str, target: &str, uid: u32, gid: u32) -> Result<i64, MetaError> {
        let mut b = Vec::new();
        put_i64(&mut b, parent);
        proto::put_key(&mut b, target);
        proto::put_u32(&mut b, uid);
        proto::put_u32(&mut b, gid);
        proto::put_key(&mut b, name);
        self.call_id(M_SYMLINK, &b)
    }

    fn link_node(&self, src_ino: i64, new_parent: i64, new_name: &str) -> Result<i64, MetaError> {
        let mut b = Vec::new();
        put_i64(&mut b, src_ino);
        put_i64(&mut b, new_parent);
        proto::put_key(&mut b, new_name);
        self.call_id(M_LINK, &b)
    }

    fn unlink(&self, parent: i64, name: &str) -> Result<Option<String>, MetaError> {
        let mut b = Vec::new();
        put_i64(&mut b, parent);
        proto::put_key(&mut b, name);
        match self.round_trip(M_UNLINK, &b) {
            Ok((0, _, data)) => {
                if data.is_empty() {
                    Ok(None)
                } else {
                    proto::get_key(&data, &mut 0)
                        .map(Some)
                        .map_err(|_| MetaError::Db("bad gc key frame".to_string()))
                }
            }
            Ok((c, _, _)) => Err(err_from_code(c)),
            Err(_) => Err(MetaError::Db("remote meta transport".to_string())),
        }
    }

    fn rename(&self, parent: i64, name: &str, new_parent: i64, new_name: &str, noreplace: bool) -> Result<Option<String>, MetaError> {
        let mut b = Vec::new();
        put_i64(&mut b, parent);
        proto::put_key(&mut b, name);
        put_i64(&mut b, new_parent);
        proto::put_key(&mut b, new_name);
        b.push(noreplace as u8);
        match self.round_trip(M_RENAME, &b) {
            Ok((0, _, data)) => {
                if data.is_empty() {
                    Ok(None)
                } else {
                    proto::get_key(&data, &mut 0)
                        .map(Some)
                        .map_err(|_| MetaError::Db("bad gc key frame".to_string()))
                }
            }
            Ok((c, _, _)) => Err(err_from_code(c)),
            Err(_) => Err(MetaError::Db("remote meta transport".to_string())),
        }
    }

    fn set_mode(&self, ino: i64, mode: u32) {
        let mut b = Vec::new();
        put_i64(&mut b, ino);
        proto::put_u32(&mut b, mode);
        let _ = self.call_status(M_SETMODE, &b);
    }

    fn set_uid_gid(&self, ino: i64, uid: u32, gid: u32) {
        let mut b = Vec::new();
        put_i64(&mut b, ino);
        proto::put_u32(&mut b, uid);
        proto::put_u32(&mut b, gid);
        let _ = self.call_status(M_SETUIDGID, &b);
    }

    fn set_times(&self, ino: i64, atime: Option<i64>, mtime: Option<i64>) {
        let mut b = Vec::new();
        put_i64(&mut b, ino);
        put_opt_i64(&mut b, atime);
        put_opt_i64(&mut b, mtime);
        let _ = self.call_status(M_SETTIMES, &b);
    }

    fn touch_mtime(&self, ino: i64) {
        let mut b = Vec::new();
        put_i64(&mut b, ino);
        let _ = self.call_status(M_TOUCH, &b);
    }

    #[allow(clippy::too_many_arguments)]
    fn lock_op(
        &self,
        ino: i64,
        owner: u64,
        start: u64,
        end: u64,
        ty: LockType,
        pid: u32,
        sid: u64,
    ) -> Result<Option<LockConflict>, MetaError> {
        let mut b = Vec::new();
        put_i64(&mut b, ino);
        proto::put_u64(&mut b, owner);
        proto::put_u64(&mut b, start);
        proto::put_u64(&mut b, end);
        put_u8(&mut b, ty.to_u8());
        proto::put_u32(&mut b, pid);
        proto::put_u64(&mut b, sid);
        match self.round_trip(M_SETLK, &b) {
            Ok((0, 0, _)) => Ok(None),
            Ok((0, 1, data)) => Ok(decode_lock_conflict(&data)),
            Ok((c, _, _)) => Err(err_from_code(c)),
            Err(_) => Err(MetaError::Db("remote meta transport".to_string())),
        }
    }

    fn get_lock(&self, ino: i64, owner: u64, start: u64, end: u64, ty: LockType) -> Option<LockConflict> {
        let mut b = Vec::new();
        put_i64(&mut b, ino);
        proto::put_u64(&mut b, owner);
        proto::put_u64(&mut b, start);
        proto::put_u64(&mut b, end);
        put_u8(&mut b, ty.to_u8());
        match self.round_trip(M_GETLK, &b) {
            Ok((0, 1, data)) => decode_lock_conflict(&data),
            _ => None,
        }
    }

    fn release_locks_of_owner(&self, owner: u64) {
        let mut b = Vec::new();
        proto::put_u64(&mut b, owner);
        let _ = self.call_status(M_RELEASE_LOCKS, &b);
    }

    fn renew_locks(&self, sid: u64) {
        let mut b = Vec::new();
        proto::put_u64(&mut b, sid);
        // Best-effort heartbeat: a transport blip is non-fatal, the lease still
        // has a full window before it would expire, and the next tick retries.
        let _ = self.call_status(M_RENEW, &b);
    }

    fn fence_handle(&self) -> Arc<AtomicU64> {
        // Shared with the data-plane client: the same Arc the handshake updates,
        // so stamped writes track the live token without per-op round-trips.
        self.fence.clone()
    }

    fn fetch_fence_epoch(&self) -> u64 {
        // Called on the rare fenced-write path: reconnect (walking the failover
        // list) so `connect_fresh` re-reads the current node's epoch into the
        // shared handle, then report it. A transport failure just returns the
        // last-known value; the write then re-fails and the client fails over.
        match self.connect_fresh() {
            Ok((s, _)) => {
                self.put_conn(s);
                self.fence.load(Ordering::SeqCst)
            }
            Err(_) => self.fence.load(Ordering::SeqCst),
        }
    }
}

// ======================= server: MetaServer ================================

/// HA role of a meta-server node. `Primary` serves reads + writes and ships
/// snapshots to its `peer`; `Standby` serves reads + event subscriptions from
/// the replicated copy and rejects write-intent ops with [`E_ROFS`]. The
/// *configured* role is only the starting point — the live role in [`HaState`]
/// flips on promotion (standby -> primary) and demotion (ex-primary -> standby).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    Primary,
    Standby,
}

impl Role {
    fn as_u8(self) -> u8 {
        match self {
            Role::Primary => 0,
            Role::Standby => 1,
        }
    }
    fn from_u8(v: u8) -> Role {
        if v == 1 {
            Role::Standby
        } else {
            Role::Primary
        }
    }
}

/// Mutable HA coordination shared by the accept loop, the replication sender and
/// the standby's promotion watchdog. `None` wherever HA is unconfigured, which
/// keeps a plain meta-server behaving exactly as before: one authoritative
/// read-write node, no replication, no read-only gate.
struct HaState {
    /// Live role; read per request to gate write-intent ops.
    role: AtomicU8,
    /// Monotonic fencing epoch. A promoted standby bumps it so a recovered,
    /// stale ex-primary can be told to stand down instead of double-writing.
    epoch: AtomicI64,
    /// When the standby last heard a heartbeat from the primary; the watchdog
    /// promotes once this goes stale (primary presumed dead).
    last_peer: Mutex<Instant>,
    /// Local monotonic instant by which this node's arbiter lease is trusted as
    /// exclusive authority. Renewed on every successful lease grant (startup,
    /// promotion, and the replicator tick); set to `now` (expired) whenever we
    /// lose the lease or get fenced. A primary admits a write if this is still in
    /// the future — no arbiter round-trip on the hot path.
    lease_deadline: Mutex<Instant>,
}

impl HaState {
    fn new(initial: Role) -> Arc<Self> {
        Arc::new(Self {
            role: AtomicU8::new(initial.as_u8()),
            epoch: AtomicI64::new(0),
            last_peer: Mutex::new(Instant::now()),
            // No lease yet: the first write (or the startup register) must prove
            // authority at the arbiter before the local fast path is trusted.
            lease_deadline: Mutex::new(Instant::now()),
        })
    }
    fn role(&self) -> Role {
        Role::from_u8(self.role.load(Ordering::SeqCst))
    }
    fn set_role(&self, r: Role) {
        self.role.store(r.as_u8(), Ordering::SeqCst);
    }
    /// True while our cached arbiter lease is still trusted.
    fn lease_valid(&self) -> bool {
        Instant::now() < *self.lease_deadline.lock()
    }
    /// Reset the local lease window after a grant, trimming [`WITNESS_LEASE_SLACK`]
    /// so we stop serving before the arbiter could hand the lease to a peer.
    fn lease_renew(&self) {
        *self.lease_deadline.lock() = Instant::now() + WITNESS_LEASE - WITNESS_LEASE_SLACK;
    }
    /// Drop the lease immediately (fenced / demoted): forces the next write back
    /// through the synchronous arbiter proof.
    fn lease_clear(&self) {
        *self.lease_deadline.lock() = Instant::now();
    }
}

/// Which metadata backend a [`MetaServer`] should drive. The default
/// ([`StoreKind::Sqlite`]) keeps the historical embedded-SQLite path (one db per
/// view under `<base>/meta/`, HA snapshot shipping via `VACUUM INTO`).
/// [`StoreKind::Pg`] routes every view through
/// [`MetaStore::Pg`]/[`crate::pg_store::PgEngine`] against the given libpq-style
/// connection URI; per-view isolation is via a PG schema (`grovefs_v_<hex>`)
/// instead of a separate db. PG deployments rely on PostgreSQL's own
/// replication/logical decoding for durability, so GroveFS-native HA
/// (`--role standby`/`--peer`/`--witness`) is rejected at `serve()` time and
/// [`MetaStore::backup_to_bytes`] reports unsupported for this variant.
#[derive(Clone, Debug)]
pub enum StoreKind {
    Sqlite,
    Pg(String),
}

/// Everything a connection thread needs to serve a node. Wrapped in `Arc` so the
/// accept loop hands one to each spawned thread and the HA background threads
/// share the same cache / role / peer view.
struct MetaRuntime {
    base: PathBuf,
    token: String,
    cache: Arc<Mutex<HashMap<String, Arc<MetaStore>>>>,
    subs: Subscribers,
    ha: Option<Arc<HaState>>,
    peer: Option<String>,
    /// Invalidation events a primary has produced but not yet relayed to the
    /// standby; drained by the replicator each cycle and shipped as `M_FORWARD`.
    /// Drained/broadcast on the standby side to its own subscribers.
    forward: Mutex<Vec<(String, Vec<Event>)>>,
    /// Opt-in synchronous replication (L2). When set, every committed mutating
    /// op on the primary blocks until the standby has durably renamed the
    /// pushed snapshot and ACKed. RPO drops from ~`REPL_INTERVAL` to 0 for
    /// those ops; write latency gains ~one control-channel round-trip plus a
    /// `VACUUM INTO` fsync. Network partitions/overruns degrade to async with
    /// a warn (never lie about RPO).
    sync_repl: bool,
    /// Serializes sync-repl pushes onto one control socket: concurrent write
    /// ops queue here so a single request/response sequence stays coherent
    /// (the standby echoes ACK per frame; interleaving would corrupt framing).
    standby_push: Mutex<()>,
    /// Fencing witness address(es): a comma-separated list of `data-server`
    /// `host:port` arbiters, `None` when unset. When present, the promotion
    /// watchdog and the replicator consult them as a MAJORITY quorum
    /// ([`witness_quorum`], [`quorum_size`]) and FAIL SAFE if they cannot form a
    /// lease quorum (primary self-demotes, standby refuses to promote). A primary
    /// holds a time-bounded LEASE at each arbiter ([`WITNESS_LEASE`]), renewed in
    /// the replicator tick, and admits a mutating op from a cheap local lease check
    /// rather than re-proving at the arbiters on every write
    /// ([`MetaRuntime::fenced_for_write`]): a grant is authoritative only once a
    /// strict majority hold it, and each arbiter refuses overlapping leases, so a
    /// majority lease is itself proof of exclusive authority, and the double-write
    /// window is bounded by the lease safety margin, not by write frequency. Losing
    /// a minority of arbiters neither freezes writes (a quorum is still reachable)
    /// nor resurrects a stale primary (a quorum still fences it); `N=1` behaves
    /// exactly as a single witness used to.
    /// The meta gate alone cannot stop content bytes: `fs_impl::write` lands on
    /// the storage backend before the debounced meta touch, so the DATA plane is
    /// fenced independently — clients stamp their epoch onto write-intent frames
    /// and the witness data-server refuses a stale one. The data-server holds the
    /// fence check and the backend write atomically against a promotion (a claim
    /// drains in-flight writes), so a stale-epoch write can never land
    /// concurrently with the new primary's: content torn-writes are closed on the
    /// arbiter node, not merely bounded (see [`super::server`]'s data-plane gate
    /// and [`super::client::RemoteBackend`]).
    witness: Option<String>,
    /// Optional per-view uid whitelist (`<base>/meta/acl.db`). Unrestricted views
    /// (no rows) are served to everyone; restricted views gate the `M_OPEN` read
    /// (else `E_AUTH`) and each write-intent op (else `E_ROFS`). See [`crate::acl`].
    acl: Arc<crate::acl::AclStore>,
    /// Which metadata backend `engine_for` should open against (see
    /// [`StoreKind`]). `Pg(_)` disables the GroveFS-native HA paths by
    /// construction: `serve()` rejects the `--role standby`/`--peer`/`--witness`
    /// flags for a PG store, and sync-repl / replicator ticks never fire.
    store: StoreKind,
}

impl MetaRuntime {
    /// Effective role: always Primary for a plain (non-HA) node.
    fn role(&self) -> Role {
        self.ha.as_ref().map(|h| h.role()).unwrap_or(Role::Primary)
    }

    /// Synchronous data-plane fencing re-proof, run before a mutating op is
    /// dispatched on a primary that has `--witness` configured. With a live
    /// majority arbiter lease this is a cheap local check and returns without any
    /// network round-trip: while a quorum of arbiters hold our (non-overlapping)
    /// lease they cannot have granted it to a peer, so a valid lease *is* proof of
    /// exclusive authority. Only when the lease is missing or expired (first write
    /// after startup or a failover, or no quorum has been reachable for a lease)
    /// does it re-prove at the arbiters synchronously — a quorum reporting
    /// `prev > mine` means a peer has promoted past us, so we demote to standby and
    /// this op (and every later write, via the standby gate) is refused `E_ROFS`.
    ///
    /// The replicator tick renews the lease every [`REPL_INTERVAL`], so in steady
    /// state no write pays an arbiter round-trip. The double-write window is
    /// bounded by the lease safety margin, not by write frequency: a peer can
    /// only take the token after our lease lapses, and we stop trusting it
    /// [`WITNESS_LEASE_SLACK`] before that.
    ///
    /// Deliberately NOT fenced on a witness *error*: a transient arbiter blip must
    /// not fail every single write. Sustained unreachability is still handled by
    /// the async fail-safe in the replicator ([`WITNESS_FENCE_GRACE`]), which
    /// self-demotes after the grace window. Returns `true` only on a proven fence.
    fn fenced_for_write(&self) -> bool {
        let (Some(ha), Some(w)) = (&self.ha, &self.witness) else {
            return false;
        };
        if ha.role() != Role::Primary {
            return false; // standby gate already refuses writes
        }
        if ha.lease_valid() {
            return false; // live lease proves exclusive authority, no round-trip
        }
        let mine = ha.epoch.load(Ordering::SeqCst);
        let addrs = split_addrs(w);
        let q = quorum_size(addrs.len());
        let res = witness_quorum(&addrs, &self.token, mine, WITNESS_LEASE, WITNESS_PROBE_TIMEOUT);
        if res.fenced(q) {
            ha.set_role(Role::Standby);
            ha.epoch.store(res.prev_max, Ordering::SeqCst);
            ha.lease_clear();
            self.cache.lock().clear();
            eprintln!("meta HA: fenced at write time (a quorum of {q}/{} arbiters holds {} > our {mine}); demoted to standby, op refused", addrs.len(), res.prev_max);
            true
        } else {
            // A majority still holds our lease: resume the fast path. Otherwise
            // (below lease quorum but no majority proves a peer ahead) we neither
            // renew nor demote here — the async tick fail-safe owns that case.
            if res.leased(q) {
                ha.lease_renew();
            }
            false
        }
    }
}

/// Central metadata authority. Holds one [`MetaEngine`] per view (lazily opened
/// against `<base>/meta/<view>.db`), shared across all connection threads, so
/// every client mounting the same view sees one consistent tree. Serving a whole
/// view from a single engine means SQLite writes are serialized in-process by
/// the engine's own mutex — no cross-process divergence by construction.
///
/// With `--role`/`--peer` it additionally runs as one half of a two-node
/// primary/standby pair (see [`MetaServer::with_ha`]).
pub struct MetaServer {
    listen: String,
    base: PathBuf,
    token: String,
    role: Role,
    peer: Option<String>,
    sync_repl: bool,
    witness: Option<String>,
    store: StoreKind,
}

/// Upper bound on the primary's wait for a standby ACK under `--sync-repl`.
/// Chosen to comfortably exceed a loopback or LAN `VACUUM INTO` + rename cycle
/// (~1–5ms typically) while still being short enough that a wedged standby
/// does not stall the meta plane beyond one client-side retry window.
const SYNC_REPL_ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

impl MetaServer {
    /// A server that accepts any `M_OPEN` (empty token) — the original, LAN-
    /// trusted behavior.
    pub fn new(listen: &str, base: &Path) -> Self {
        Self {
            listen: listen.to_string(),
            base: base.to_path_buf(),
            token: String::new(),
            role: Role::Primary,
            peer: None,
            sync_repl: false,
            witness: None,
            store: StoreKind::Sqlite,
        }
    }

    /// A server that only serves clients presenting `token` on the `M_OPEN`
    /// handshake. With a non-empty token, unknown/unauthorized callers can no
    /// longer open arbitrary view trees over the LAN.
    pub fn with_token(listen: &str, base: &Path, token: &str) -> Self {
        Self {
            listen: listen.to_string(),
            base: base.to_path_buf(),
            token: token.to_string(),
            role: Role::Primary,
            peer: None,
            sync_repl: false,
            witness: None,
            store: StoreKind::Sqlite,
        }
    }

    /// Configure this node's HA role and its peer's address. A `Primary` with a
    /// `peer` periodically ships consistent snapshots of every view db to that
    /// peer and heartbeats its epoch; a `Standby` serves reads read-only from the
    /// replicated copy, rejects writes with [`E_ROFS`], and promotes itself if the
    /// primary's heartbeats stop. Called with `peer = None` and `role = Primary`
    /// this is a plain single-node server (unchanged behavior).
    pub fn with_ha(mut self, role: Role, peer: Option<String>) -> Self {
        self.role = role;
        self.peer = peer.filter(|p| !p.is_empty());
        self
    }

    /// Enable synchronous replication for mutating ops (L2). Ignored unless
    /// HA is configured (`--peer`). When the standby is unreachable or slow,
    /// each op waits up to [`SYNC_REPL_ACK_TIMEOUT`] and then degrades to the
    /// existing async path with a single-line warn (so RPO is honestly bounded
    /// by `REPL_INTERVAL` again rather than silently reported as 0).
    pub fn with_sync_repl(mut self, yes: bool) -> Self {
        self.sync_repl = yes;
        self
    }

    /// Point this node at one or more fencing witnesses (comma-separated
    /// `data-server` `host:port` arbiters) that both HA halves can reach. Each
    /// arbitrates a time-bounded, NON-OVERLAPPING LEASE on the node's epoch (the
    /// data-server's `Fence`, probed via [`witness_probe_one`] and aggregated by
    /// [`witness_quorum`]): authority is proven only when a strict majority
    /// ([`quorum_size`]) of them grant, and each grants one only after its current
    /// holder's has expired, so two nodes can never hold authority at once — and
    /// because two majorities always intersect, a same-value promotion race is won
    /// by exactly one node. A live primary renews its lease every tick and admits a
    /// mutating op from a cheap LOCAL lease check ([`MetaRuntime::fenced_for_write`])
    /// — a valid majority lease *is* proof of exclusive authority, so no arbiter
    /// fan-out is needed per write (one per renewal, amortized across the window). A
    /// node whose lease a majority denies (a quorum holds `prev >= its epoch`) has
    /// been fenced and stands down. The quorum is used fail-safe: a node that
    /// *cannot form a lease quorum* for [`WITNESS_FENCE_GRACE`] (one lease) gives up
    /// write authority (primary self-demotes, standby won't promote), trading
    /// availability so a partition never leaves an *unbounded* double-primary. The
    /// double-write window is bounded by the lease safety margin
    /// [`WITNESS_LEASE_SLACK`] — a peer can only take the token after our lease
    /// lapses, and we stop trusting it that much earlier — not by write frequency.
    /// Losing a MINORITY of arbiters neither freezes writes (a quorum is still
    /// reachable) nor resurrects a stale primary (a quorum still fences it); `N=1`
    /// behaves exactly as a single witness used to. (A transient arbiter error is
    /// NOT treated as fenced here; the async fail-safe still owns sustained
    /// outages.) That meta proof alone cannot stop content bytes (`fs_impl::write`
    /// hits the storage backend before the debounced meta touch), so the DATA plane
    /// is fenced too: a mounted client stamps its live epoch onto every write-intent
    /// frame and the witness data-server refuses one whose epoch lags its token,
    /// closing the last gap a stale primary could otherwise write through. That
    /// content gate only bites on a data-server whose token the quorum actually
    /// raises, i.e. one IN the `--witness` list: deploy the witness co-located with
    /// the storage clients write to (the intended topology), otherwise a content
    /// store OUTSIDE the witness set keeps a zero token, its data-plane gate stays
    /// inert, and only the meta-plane fence protects it. Each
    /// witness data-server persists the token (`<root>/.fence_token`), so losing a
    /// minority of those stores cannot resurrect a deposed primary. Without
    /// `--witness`, HA keeps best-effort peer-only fencing, which a strict two-node
    /// split-brain can defeat — deploy a witness (an ODD number, 3 is the robust
    /// default) for safety.
    pub fn with_witness(mut self, addr: Option<String>) -> Self {
        self.witness = addr.filter(|a| !a.is_empty());
        self
    }

    /// Choose the metadata backend. `uri` accepts:
    /// * `None` or `"sqlite"` (or anything not a `postgres[ql]://` URI) — the
    ///   default embedded-SQLite backend, one db per view under `<base>/meta/`.
    /// * `postgres://user:pw@host:port/db` (or `postgresql://...`) — the
    ///   PostgreSQL backend, one PG schema per view. Combined with
    ///   `--role/--peer/--witness` at `serve()` time this returns an
    ///   [`io::ErrorKind::InvalidInput`] because GroveFS-native HA snapshot
    ///   shipping (`VACUUM INTO`) is a SQLite-specific mechanism; run PG with
    ///   PostgreSQL's own streaming/logical replication instead.
    ///
    /// The `acl.db` whitelist store remains local SQLite regardless of this
    /// choice — ACL rows are per-`meta-server` control-plane state, not part of
    /// the shared view tree.
    pub fn with_store(mut self, uri: Option<String>) -> Self {
        match uri {
            Some(u) if u.starts_with("postgres://") || u.starts_with("postgresql://") => {
                self.store = StoreKind::Pg(u);
            }
            _ => self.store = StoreKind::Sqlite,
        }
        self
    }

    pub fn serve(&self) -> io::Result<()> {
        // PG mode never exercises the GroveFS-native HA pipeline: the
        // `VACUUM INTO` snapshot mechanism and the fencing-witness lease are
        // SQLite-specific. Refuse the combination at startup instead of letting
        // a caller assume `--peer` still replicates (it silently wouldn't).
        if matches!(self.store, StoreKind::Pg(_))
            && (self.peer.is_some() || self.witness.is_some() || self.role == Role::Standby)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--store postgres://… cannot be combined with --role standby / --peer / --witness: \n                     GroveFS-native HA snapshot shipping is SQLite-only.\n                     Run PostgreSQL's own streaming / logical replication for HA,\n                     or keep the meta-server single-instance under PG.",
            ));
        }
        std::fs::create_dir_all(self.base.join("meta"))?;
        // Open the optional view whitelist BEFORE binding so a client (or the
        // test board's `wait_port` + log grep) never sees the listen port
        // reachable while the SQLite ACL setup is still running — otherwise
        // the port becomes connectable before the `synchronous replication ON`
        // / `registered fencing epoch` log lines have flushed, and readiness
        // probes race startup I/O.
        let acl = Arc::new(
            crate::acl::AclStore::open(&self.base).expect("open acl.db"),
        );
        let listener = TcpListener::bind(&self.listen)?;
        println!(
            "meta-server listening on {} (base {}, role {:?}{})",
            self.listen,
            self.base.display(),
            self.role,
            self.peer
                .as_ref()
                .map(|p| format!(", peer {p}"))
                .unwrap_or_default()
        );
        // HA coordination is live only when a peer was configured or we start as
        // a standby; otherwise `ha = None` preserves single-node semantics.
        let ha = if self.peer.is_some() || self.role == Role::Standby {
            Some(HaState::new(self.role))
        } else {
            None
        };
        let rt = Arc::new(MetaRuntime {
            base: self.base.clone(),
            token: self.token.clone(),
            cache: Arc::new(Mutex::new(HashMap::new())),
            subs: Arc::new(Mutex::new(HashMap::new())),
            ha: ha.clone(),
            peer: self.peer.clone(),
            forward: Mutex::new(Vec::new()),
            sync_repl: self.sync_repl && self.peer.is_some(),
            standby_push: Mutex::new(()),
            witness: self.witness.clone(),
            acl,
            store: self.store.clone(),
        });
        if rt.sync_repl {
            println!("meta-server: synchronous replication ON (RPO=0 for mutating ops, ACK timeout {SYNC_REPL_ACK_TIMEOUT:?})");
        }
        // Fail LOUD on a malformed witness list. `split_addrs` collapses a
        // degenerate input (e.g. a lone comma) to a single bogus entry, which
        // would otherwise silently degrade to "arbiter unreachable -> fail-safe
        // standby" and look exactly like a real partition. An entry without a
        // `host:port` colon cannot connect, so warn that it counts as a dead
        // arbiter; and note an even N so a redundant node is not chosen by
        // accident (odd N is the robust shape for a majority quorum).
        if let Some(w) = &rt.witness {
            let addrs = split_addrs(w);
            let malformed: Vec<&String> = addrs.iter().filter(|a| !a.contains(':')).collect();
            if !malformed.is_empty() {
                eprintln!(
                    "meta HA: WARNING --witness entry(ies) {malformed:?} are not host:port; they will be probed as unreachable arbiters (fail-safe), which is almost certainly a misconfiguration rather than a partition"
                );
            }
            let n = addrs.len();
            if n > 1 && n.is_multiple_of(2) {
                eprintln!(
                    "meta HA: --witness lists {n} (even) arbiters; a lease quorum needs {}/{} grants — an odd count is recommended so no arbiter is redundant and no tie can strand the quorum",
                    quorum_size(n),
                    n
                );
            }
        }
        if let Some(ha) = &ha {
            // Seed fencing below what a peer may already know after a restart.
            ha.epoch.store(max_epoch_on_disk(&rt), Ordering::SeqCst);
            // If we start as Primary and a witness is configured, register our
            // epoch there before serving: a higher value means the peer already
            // promoted past us while we were down, so we boot straight into
            // standby instead of opening a write window on the first tick.
            if self.role == Role::Primary {
                if let Some(w) = &rt.witness {
                    let mine = ha.epoch.load(Ordering::SeqCst);
                    let addrs = split_addrs(w);
                    let q = quorum_size(addrs.len());
                    let res = witness_quorum(&addrs, &rt.token, mine, WITNESS_LEASE, CONTROL_TIMEOUT);
                    if res.fenced(q) {
                        ha.set_role(Role::Standby);
                        ha.epoch.store(res.prev_max, Ordering::SeqCst);
                        rt.cache.lock().clear();
                        println!("meta HA: starting as standby (a quorum of {q}/{} arbiters holds {} > our on-disk {mine}; peer already promoted)", addrs.len(), res.prev_max);
                    } else if res.leased(q) {
                        // Majority granted our lease: establish the initial lease so
                        // the first writes take the fast path; the tick renews it.
                        ha.lease_renew();
                        println!("meta HA: registered fencing epoch {mine} at witness quorum ({q}/{} arbiters)", addrs.len());
                    } else {
                        // Cannot form a lease quorum (too few arbiters reachable,
                        // or a live incumbent lease still blocks us at a majority).
                        // Fail-safe: with a peer configured we cannot prove we are
                        // not the stale half, so boot read-only; the watchdog
                        // promotes only once a quorum is reachable again. A lone
                        // node (no peer) has no double-write risk, so serve.
                        if rt.peer.is_some() {
                            ha.set_role(Role::Standby);
                            rt.cache.lock().clear();
                            eprintln!("meta HA: only {}/{} arbiters reachable at startup (need quorum {q}); fail-safe starting as standby (cannot prove exclusive authority against a configured peer)", res.reachable, addrs.len());
                        } else {
                            eprintln!("meta HA: cannot obtain a lease quorum ({q}/{}) at startup; no peer configured, serving as primary (fencing not applicable to a lone node)", addrs.len());
                        }
                    }
                }
            }
            if rt.peer.is_some() {
                start_replicator(rt.clone());
            }
            start_promotion_watchdog(rt.clone(), ha.clone());
        }
        serve_runtime(listener, rt);
        Ok(())
    }
}

/// Accept loop split out so tests can bind an ephemeral port and know the addr.
pub fn serve_on_meta(listener: TcpListener, base: PathBuf) {
    serve_on_meta_auth(listener, base, "")
}

/// Accept loop with an access token (`""` disables the check). Single-node (no
/// HA), matching the historical behavior all existing tests rely on.
pub fn serve_on_meta_auth(listener: TcpListener, base: PathBuf, token: &str) {
    let acl_base = base.clone();
    let rt = Arc::new(MetaRuntime {
        base,
        token: token.to_string(),
        cache: Arc::new(Mutex::new(HashMap::new())),
        subs: Arc::new(Mutex::new(HashMap::new())),
        ha: None,
        peer: None,
        forward: Mutex::new(Vec::new()),
        sync_repl: false,
        standby_push: Mutex::new(()),
        witness: None,
        acl: Arc::new(
            crate::acl::AclStore::open(&acl_base).expect("open acl.db"),
        ),
        store: StoreKind::Sqlite,
    });
    serve_runtime(listener, rt);
}

fn serve_runtime(listener: TcpListener, rt: Arc<MetaRuntime>) {
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let rt = rt.clone();
                thread::spawn(move || {
                    if let Err(e) = handle_meta_conn(s, rt) {
                        if e.kind() != io::ErrorKind::UnexpectedEof {
                            eprintln!("meta conn error: {e}");
                        }
                    }
                });
            }
            Err(e) => eprintln!("meta accept failed: {e}"),
        }
    }
}

/// Open (or reuse) the authoritative engine for `view`. A HA standby opens its
/// replicated copy strictly read-only (so a stray write is refused at the SQLite
/// layer, not merely discouraged); a primary opens read-write. Under
/// [`StoreKind::Pg`] the standby/readonly path never fires (`serve()` refuses
/// the combination), so `readonly` is passed through as `false`.
fn engine_for(rt: &MetaRuntime, view: &str) -> io::Result<Arc<MetaStore>> {
    let mut m = rt.cache.lock();
    if let Some(e) = m.get(view) {
        return Ok(e.clone());
    }
    let store = match &rt.store {
        StoreKind::Sqlite => {
            let path = rt.base.join("meta").join(format!("{view}.db"));
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).ok();
            }
            let readonly = rt.role() == Role::Standby;
            MetaStore::open_sqlite(&rt.base, view, readonly)
                .map_err(|e| io::Error::other(e.to_string()))?
        }
        StoreKind::Pg(uri) => MetaStore::open_pg(uri, view, false)
            .map_err(|e| io::Error::other(format!("pg open for view='{view}' uri='{uri}': {e}")))?,
    };
    let engine = Arc::new(store);
    m.insert(view.to_string(), engine.clone());
    Ok(engine)
}

// --- server-side invalidation event bus ------------------------------------

/// A pending kernel-cache invalidation the server pushes to a view's other
/// mounts after a successful mutating op, so peers converge without waiting a
/// TTL and without `--strict`.
#[derive(Clone)]
enum Event {
    /// Drop the cached dentry `(parent, name)` so the next lookup re-fetches
    /// (covers create / unlink / rename / symlink / link).
    Entry { parent: i64, name: String },
    /// Drop cached attrs + page cache for `ino` (content or attr changed; this
    /// is what makes a peer's `write` -> `touch_mtime` visible immediately).
    Inode { ino: i64 },
}

/// Per-view set of event-sink sockets (write halves), keyed by a connection id
/// so each subscriber thread can unregister exactly its own handle on EOF.
type Subscribers = Arc<Mutex<HashMap<String, HashMap<u64, TcpStream>>>>;

/// Monotonic id handed to each subscriber connection.
static NEXT_SUB: AtomicU64 = AtomicU64::new(1);

/// Push `events` to every subscriber of `view`. The registry mutex is held for
/// the whole send so two op threads can never interleave partial frames on the
/// same socket; events are small and infrequent, so this global serialization
/// is cheap. Unreachable sinks are pruned opportunistically.
fn broadcast(subs: &Subscribers, view: &str, events: &[Event]) {
    if events.is_empty() {
        return;
    }
    let mut reg = subs.lock();
    if let Some(map) = reg.get_mut(view) {
        let mut dead: Vec<u64> = Vec::new();
        for (id, s) in map.iter_mut() {
            for ev in events {
                if send_event(s, ev).is_err() {
                    dead.push(*id);
                    break;
                }
            }
        }
        for id in dead {
            map.remove(&id);
        }
    }
}

fn send_event(s: &mut TcpStream, ev: &Event) -> io::Result<()> {
    match ev {
        Event::Entry { parent, name } => {
            let mut d = Vec::new();
            proto::put_key(&mut d, name);
            proto::send_response(s, EV_ENTRY, *parent as u64, &d)
        }
        Event::Inode { ino } => proto::send_response(s, EV_INODE, *ino as u64, &[]),
    }
}

/// Serialize an event batch for the primary->standby `M_FORWARD` relay. Wire
/// shape mirrors [`send_event`]: a u32 count then, per event, a kind byte and
/// its payload (Entry: parent + name; Inode: ino).
fn put_events(buf: &mut Vec<u8>, events: &[Event]) {
    proto::put_u32(buf, events.len() as u32);
    for ev in events {
        match ev {
            Event::Entry { parent, name } => {
                buf.push(0);
                put_i64(buf, *parent);
                proto::put_key(buf, name);
            }
            Event::Inode { ino } => {
                buf.push(1);
                put_i64(buf, *ino);
            }
        }
    }
}

/// Inverse of [`put_events`]; cursor `i` starts just past the view key.
fn events_from(body: &[u8], i: &mut usize) -> io::Result<Vec<Event>> {
    let count = proto::read_u32(body, i)? as usize;
    let mut out = Vec::with_capacity(count.min(4096));
    for _ in 0..count {
        match get_u8(body, i)? {
            0 => {
                let parent = get_i64(body, i)?;
                let name = proto::get_key(body, i)?;
                out.push(Event::Entry { parent, name });
            }
            _ => {
                let ino = get_i64(body, i)?;
                out.push(Event::Inode { ino });
            }
        }
    }
    Ok(out)
}

/// Events to emit when an existing inode's *content or attributes* changed
/// (write / truncate / chmod / chown / utimes / touch).
///
/// We push BOTH an inode and an entry invalidation:
/// - `Event::Inode` -> `inval_inode(ino,0,-1)` drops the peer's cached pages and
///   marks its attrs stale.
/// - `Event::Entry` -> `inval_entry(parent,name)` drops the peer's dentry so the
///   next access re-runs `lookup`, which rebuilds the attr with the *live* size
///   (read from the data backend). Without the entry drop, some kernels keep
///   serving a read truncated to the OLD cached size after an overwrite grew the
///   file, so a peer sees a short prefix of the new content instead of all of it.
///
/// The parent/name are resolved server-side from the authoritative engine, so
/// the writer's `touch_mtime(ino)` call site needs no extra context.
fn inode_change_events(engine: &MetaStore, ino: i64) -> Vec<Event> {
    let mut evs = vec![Event::Inode { ino }];
    if let Some(n) = engine.getattr(ino) {
        if !n.name.is_empty() {
            evs.push(Event::Entry { parent: n.parent_id, name: n.name });
        }
    }
    evs
}

/// Turn a connection into a pure event sink: register its write half, ack, then
/// block on reads until the client disconnects and unregister. Other threads
/// deliver events to the registered handle via [`broadcast`].
fn serve_subscriber(
    mut reader: BufReader<TcpStream>,
    mut writer: TcpStream,
    view: &str,
    subs: &Subscribers,
) -> io::Result<()> {
    let id = NEXT_SUB.fetch_add(1, Ordering::Relaxed);
    let handle = writer.try_clone()?;
    subs.lock().entry(view.to_string()).or_default().insert(id, handle);
    proto::send_response(&mut writer, 0, 0, &[])?; // subscribe ack
    // The client sends no further requests here; drain until it hangs up.
    let result = loop {
        match proto::recv_request(&mut reader) {
            Ok(_) => continue,
            Err(e) => {
                break if e.kind() == io::ErrorKind::UnexpectedEof {
                    Ok(())
                } else {
                    Err(e)
                };
            }
        }
    };
    if let Some(map) = subs.lock().get_mut(view) {
        map.remove(&id);
    }
    result
}

fn handle_meta_conn(stream: TcpStream, rt: Arc<MetaRuntime>) -> io::Result<()> {
    stream.set_nodelay(true)?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;

    // First frame distinguishes a client from a peer's replication control
    // connection: clients always open with `M_OPEN { view, token }`; the peer
    // opens directly with `M_PING`/`M_PUSH` (never bound to a view).
    let (op, body) = proto::recv_request(&mut reader)?;
    if op == M_PING || op == M_PUSH {
        return handle_control_conn(&mut reader, &mut writer, rt, op, body);
    }
    if op != M_OPEN {
        proto::send_response(&mut writer, E_DB, 0, &[])?;
        return Ok(());
    }
    let mut i = 0usize;
    let view = proto::get_key(&body, &mut i).unwrap_or_default();
    let presented = proto::get_key(&body, &mut i).unwrap_or_default();
    // Optional presenting uid appended by the client handshake (older clients
    // omit it -> uid 0, which is simply denied on a whitelisted view).
    let uid = proto::read_u32(&body, &mut i).unwrap_or(0);
    // Enforce the shared secret when one is configured; an empty server token
    // accepts any client (backward-compatible, LAN-trusted).
    if !rt.token.is_empty() && presented != rt.token {
        proto::send_response(&mut writer, E_AUTH, 0, &[])?;
        return Ok(());
    }
    // View whitelist: an unrestricted view (no ACL rows) admits any uid; a
    // restricted one refuses the open outright unless `uid` holds a read grant.
    if !rt.acl.check_read(&view, uid) {
        proto::send_response(&mut writer, E_AUTH, 0, &[])?;
        return Ok(());
    }
    // Note: the write-gate is re-evaluated per-op inside the loop below (not
    // cached here), so `grove acl revoke --view V --uid U` immediately closes
    // established connections against further writes rather than only on the
    // next M_OPEN handshake (closes a TOCTOU where revoke had no effect on
    // any already-mounted client).
    // Validate the view db opens before acking. We deliberately do NOT
    // cache this engine for the whole connection: under HA a fresh replicated
    // snapshot evicts the cached engine, so live connections must re-resolve it
    // per op to observe the swap (see the loop below).
    if let Err(e) = engine_for(&rt, &view) {
        eprintln!("meta: M_OPEN view='{view}' engine open failed: {e}");
        proto::send_response(&mut writer, E_DB, 0, &[])?;
        return Ok(());
    }
    // Handshake ack. Echo this node's current fencing epoch in `val` (0 when HA
    // is not configured) so the client can stamp its data-plane writes with the
    // token its (primary) meta-server holds — the arbiter then accepts writes that
    // match the live token and refuses a stale primary's clients (see
    // `proto::is_data_write`). Re-fetched on every (re)handshake, so failover to a
    // promoted node refreshes the client's epoch automatically.
    let open_epoch = rt
        .ha
        .as_ref()
        .map(|h| h.epoch.load(Ordering::SeqCst) as u64)
        .unwrap_or(0);
    proto::send_response(&mut writer, 0, open_epoch, &[])?;

    // Distinguish a subscriber (becomes an event sink, sends no requests) from a
    // work connection (metadata ops) by peeking the next frame.
    let (op, body) = proto::recv_request(&mut reader)?;
    if op == M_SUBSCRIBE {
        return serve_subscriber(reader, writer, &view, &rt.subs);
    }
    let mut pending = Some((op, body));

    loop {
        let (op, body) = match pending.take() {
            Some(v) => v,
            None => match proto::recv_request(&mut reader) {
                Ok(v) => v,
                Err(e) => {
                    if e.kind() == io::ErrorKind::UnexpectedEof {
                        return Ok(());
                    }
                    return Err(e);
                }
            },
        };
        // View write-whitelist: a restricted view refuses mutating ops from a uid
        // without a write grant (reads still served). Unrestricted views grant all.
        // Re-evaluated per-op so `grove acl revoke` immediately closes established
        // connections (see the M_OPEN note above for the TOCTOU rationale).
        //
        // Uses E_NOTPERM (not E_ROFS) so the client's `err_from_code` maps to
        // `MetaError::OpNotPermitted` → `EPERM` at the FUSE reply, distinct
        // from an HA-standby's E_ROFS failover signal.
        if is_write_op(op) && !rt.acl.check_write(&view, uid) {
            proto::send_response(&mut writer, E_NOTPERM, 0, &[])?;
            continue;
        }
        // Read-only gate: a HA standby (or a demoted ex-primary) refuses
        // write-intent ops. Reads + subscriptions are still served from the
        // replicated snapshot; the client treats E_ROFS as a failover signal.
        if rt.role() == Role::Standby && is_write_op(op) {
            proto::send_response(&mut writer, E_ROFS, 0, &[])?;
            continue;
        }
        // Zero-window fencing (only with --witness): a write-intent op on a
        // primary synchronously re-proves exclusive authority at the arbiter, so
        // a just-fenced ex-primary refuses on its very NEXT write rather than
        // within one async replicator tick. No-op when no witness is configured.
        if is_write_op(op) && rt.fenced_for_write() {
            proto::send_response(&mut writer, E_ROFS, 0, &[])?;
            continue;
        }
        // Re-resolve the engine each op: cheap while cached, and picks up a
        // freshly installed snapshot after a replication push evicted the old one.
        let engine = match engine_for(&rt, &view) {
            Ok(e) => e,
            Err(_) => {
                proto::send_response(&mut writer, E_DB, 0, &[])?;
                continue;
            }
        };
        let (status, val, data, events) = dispatch_meta(&engine, op, &body);
        // Synchronous replication (L2): commit the op to the standby BEFORE
        // acking the client. If the push/ACK window is exceeded we fall through
        // and emit a warn so operators see the degraded RPO honestly. Non-HA
        // or standby roles skip this entirely (their `rt.sync_repl` is false).
        if rt.sync_repl && status == 0 && is_write_op(op) && rt.role() == Role::Primary {
            push_snapshot_now(&rt, &view, &engine);
        }
        proto::send_response(&mut writer, status, val, &data)?;
        if status == 0 && !events.is_empty() {
            broadcast(&rt.subs, &view, &events);
            // Under HA, also queue the batch for relay to the standby's own
            // subscribers so a client mounted to the standby converges too. The
            // replicator drains this each cycle (<= one REPL_INTERVAL lag).
            if rt.peer.is_some() && rt.role() == Role::Primary {
                rt.forward.lock().push((view.clone(), events));
            }
        }
    }
}

/// Serve a replication / heartbeat control connection initiated by the peer.
/// Frames are handled request/response on one socket: `M_PING { sender_epoch }`
/// refreshes the standby's heartbeat and returns our epoch (or [`E_EPOCH`] if
/// the sender is a stale ex-primary that must stand down); `M_PUSH { view,
/// snapshot }` installs a replicated view db atomically (standby only).
fn handle_control_conn(
    reader: &mut BufReader<TcpStream>,
    writer: &mut TcpStream,
    rt: Arc<MetaRuntime>,
    first_op: u8,
    first_body: Vec<u8>,
) -> io::Result<()> {
    let ha = match &rt.ha {
        // A non-HA node never expects control traffic; ignore it.
        None => return Ok(()),
        Some(h) => h.clone(),
    };
    let mut op = first_op;
    let mut body = first_body;
    loop {
        match op {
            M_PING => {
                let mut i = 0usize;
                let sender_epoch = get_i64(&body, &mut i).unwrap_or(0);
                let my_epoch = ha.epoch.load(Ordering::SeqCst);
                match ha.role() {
                    Role::Standby => {
                        // The primary is alive (it just pinged us); defer promotion.
                        *ha.last_peer.lock() = Instant::now();
                        if sender_epoch > my_epoch {
                            ha.epoch.store(sender_epoch, Ordering::SeqCst);
                        }
                        proto::send_response(writer, 0, ha.epoch.load(Ordering::SeqCst) as u64, &[])?;
                    }
                    Role::Primary => {
                        // We are the primary (perhaps promoted). A peer pinging with
                        // a lower epoch is a stale ex-primary: tell it to stand down.
                        let status = if sender_epoch < my_epoch { E_EPOCH } else { 0 };
                        proto::send_response(writer, status, my_epoch as u64, &[])?;
                    }
                }
            }
            M_PUSH => {
                let mut i = 0usize;
                let view = proto::get_key(&body, &mut i).unwrap_or_default();
                let snap = body[i..].to_vec();
                // Only a standby installs pushes; a node that has promoted (or a
                // plain primary) is authoritative and rejects the stale copy.
                if ha.role() == Role::Standby
                    && install_snapshot(&rt.base, &rt.cache, &view, &snap).is_ok()
                {
                    proto::send_response(writer, 0, 0, &[])?;
                } else {
                    proto::send_response(writer, E_EPOCH, ha.epoch.load(Ordering::SeqCst) as u64, &[])?;
                }
            }
            M_FORWARD => {
                // Relay from the primary: deliver to this node's own subscribers
                // so standby-mounted clients converge (batched, <= one tick).
                let mut i = 0usize;
                let view = proto::get_key(&body, &mut i).unwrap_or_default();
                if let Ok(evs) = events_from(&body, &mut i) {
                    broadcast(&rt.subs, &view, &evs);
                }
                proto::send_response(writer, 0, 0, &[])?;
            }
            _ => {
                proto::send_response(writer, E_DB, 0, &[])?;
                return Ok(());
            }
        }
        match proto::recv_request(reader) {
            Ok((o, b)) => {
                op = o;
                body = b;
            }
            Err(e) => {
                return if e.kind() == io::ErrorKind::UnexpectedEof {
                    Ok(())
                } else {
                    Err(e)
                };
            }
        }
    }
}

/// Atomically replace `<base>/meta/<view>.db` with a replicated snapshot and
/// drop the cached engine so the next read opens the fresh copy. The snapshot is
/// a complete `VACUUM INTO` image (self-contained, rollback-journal), so the old
/// WAL/shm side files are removed rather than left to shadow it. `view` is
/// validated to keep the peer from steering writes outside the meta dir.
fn install_snapshot(
    base: &Path,
    cache: &Mutex<HashMap<String, Arc<MetaStore>>>,
    view: &str,
    snap: &[u8],
) -> io::Result<()> {
    if view.is_empty() || view.contains('/') || view.contains('\\') || view.contains("..") {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "bad view name"));
    }
    let dir = base.join("meta");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{view}.db"));
    let tmp = dir.join(format!("{view}.db.push.tmp"));
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(snap)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, &path)?; // same-fs rename is atomic
    cache.lock().remove(view); // force a reopen of the fresh copy on next access
    let _ = std::fs::remove_file(dir.join(format!("{view}.db-wal")));
    let _ = std::fs::remove_file(dir.join(format!("{view}.db-shm")));
    Ok(())
}

/// Names of the view dbs currently on disk (stem of each `<base>/meta/*.db`).
fn list_users(base: &Path) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(base.join("meta")) {
        for ent in rd.flatten() {
            let p = ent.path();
            if p.extension().and_then(|e| e.to_str()) == Some("db") {
                if let Some(name) = p.file_stem().and_then(|s| s.to_str()) {
                    out.push(name.to_string());
                }
            }
        }
    }
    out
}

/// Highest epoch persisted across this node's view dbs (0 if none). Seeds the
/// in-memory fencing epoch at startup so a restart never moves it backwards.
fn max_epoch_on_disk(rt: &MetaRuntime) -> i64 {
    let mut max = 0i64;
    for name in list_users(&rt.base) {
        let p = rt.base.join("meta").join(format!("{name}.db"));
        if let Ok(eng) = MetaEngine::open_ex(&p, true) {
            max = max.max(eng.get_epoch());
        }
    }
    max
}

/// Synchronous replication push (L2). Called from the primary's dispatch
/// thread AFTER `dispatch_meta` committed a mutating op and BEFORE the client
/// sees its response. Serializes on `rt.standby_push` (concurrent writers
/// queue), takes a consistent snapshot via `VACUUM INTO`, and issues a
/// dedicated `M_PUSH` on a fresh control socket; the standby's existing
/// install-snapshot-then-ACK path (`handle_control_conn` -> `M_PUSH`) does
/// fsync + atomic rename before it replies, so a status-0 response *is* the
/// durability witness (RPO=0 for this op).
///
/// Failure modes degrade to async on purpose, never to false success:
/// * peer down / connection refused -> warn, return, the periodic replicator
///   will retry on its next tick (behavior identical to no `--sync-repl`).
/// * ACK timeout / transport error -> warn, return.
/// * Standby demoted (E_EPOCH) -> demote ourselves, mirroring the async path.
///
/// The client's write still succeeded locally; the warn is the honest signal
/// that RPO for this op has fallen back to `REPL_INTERVAL`.
fn push_snapshot_now(rt: &MetaRuntime, view: &str, engine: &MetaStore) {
    let peer = match &rt.peer {
        Some(p) => p.clone(),
        None => return,
    };
    let ha = match &rt.ha {
        Some(h) => h.clone(),
        None => return,
    };
    let snap = match engine.backup_to_bytes() {
        Ok(s) => s,
        Err(_) => return,
    };
    // Serialize pushes: the standby must see a coherent snapshot sequence and
    // the control protocol is a strict request/response stream.
    let _g = rt.standby_push.lock();
    let mut conn = match TcpStream::connect(&peer) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("meta sync-repl: peer {peer} unreachable ({e}); degraded to async for this op");
            return;
        }
    };
    let _ = conn.set_read_timeout(Some(SYNC_REPL_ACK_TIMEOUT));
    let _ = conn.set_write_timeout(Some(SYNC_REPL_ACK_TIMEOUT));
    let mut body = Vec::with_capacity(view.len() + 2 + snap.len());
    proto::put_key(&mut body, view);
    body.extend_from_slice(&snap);
    if proto::send_request(&mut conn, M_PUSH, &body).is_err() {
        eprintln!("meta sync-repl: M_PUSH send to {peer} failed; degraded to async for this op");
        return;
    }
    match proto::recv_response(&mut conn) {
        Ok((0, _, _)) => {} // durable ACK
        Ok((E_EPOCH, peer_epoch, _)) => {
            // Standby has already promoted past us: stand down (same path as
            // the periodic replicator's heartbeat handling).
            ha.set_role(Role::Standby);
            ha.epoch.store((peer_epoch as i64).max(ha.epoch.load(Ordering::SeqCst)), Ordering::SeqCst);
            rt.cache.lock().clear();
            *ha.last_peer.lock() = Instant::now();
            eprintln!("meta sync-repl: demoted to standby (peer epoch {peer_epoch})");
        }
        Ok((st, _, _)) => {
            eprintln!("meta sync-repl: standby ACK status={st}; degraded to async for this op");
        }
        Err(_) => {
            eprintln!("meta sync-repl: standby ACK timeout after {SYNC_REPL_ACK_TIMEOUT:?}; degraded to async for this op");
        }
    }
}

/// Consult ONE fencing arbiter (a `data-server` at `addr` `host:port`). This is
/// the single-node probe; [`witness_quorum`] fans it out over the configured
/// arbiter set and applies majority logic.
///
/// Protocol: open a TCP connection, complete the `OP_OPEN` token handshake, then
/// send a single `OP_FENCE` frame carrying `my_epoch`. The data-server folds it
/// into a monotonic token (`fetch_max`) and echoes back the value it held
/// *before* the fold — `prev`. One call is simultaneously a *claim* (our epoch is
/// recorded at the witness) and a *verdict read*. The caller interprets `prev`:
///   * To **win** leadership at `X` (become/maintain the top): `prev < X` — our
///     claim raised the token, nobody was at-or-above us.
///   * **Fenced** at `X`: `prev >= X` — an equal-or-higher epoch already lives at
///     the witness (a peer won, or a same-value tie), so we must stand down.
///
/// Using `prev` (not the post-fold max) is what makes a tie on `X` resolve
/// against the later claimer instead of letting both nodes think they hold it.
///
/// Errors (unreachable witness, auth failure, timeout) are surfaced to the
/// caller. With a witness configured the caller treats *unreachable* as "cannot
/// prove exclusive authority" and fails safe (primary self-demotes after a grace,
/// standby refuses promotion) — see [`WITNESS_FENCE_GRACE`].
///
/// Returns `(prev, granted)`: `prev` is the token the arbiter held before this
/// call (the fence verdict — `prev > my_epoch` means a peer is already ahead),
/// and `granted` says whether the arbiter issued us a live lease for `my_epoch`
/// after the fold. A promotion is granted only once no higher-or-live incumbent
/// lease blocks it, so `granted` is the arbiter's own non-overlap guarantee.
fn witness_probe_one(
    addr: &str,
    token: &str,
    my_epoch: i64,
    ttl: Duration,
    timeout: Duration,
) -> io::Result<(i64, bool)> {
    // Bound `connect` too: bare `TcpStream::connect` has no timeout and can hang
    // on a blackholed arbiter for the OS default, so resolve first then use
    // `connect_timeout`. All three timeouts share `timeout` so one probe is
    // hard-capped no matter where it stalls.
    let sock = addr
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("arbiter {addr} did not resolve")))?;
    let mut conn = TcpStream::connect_timeout(&sock, timeout)?;
    conn.set_read_timeout(Some(timeout))?;
    conn.set_write_timeout(Some(timeout))?;
    let mut reader = BufReader::new(conn.try_clone()?);
    // Handshake: present our access token (empty server token accepts any).
    let mut hb = Vec::new();
    proto::put_key(&mut hb, token);
    proto::send_request(&mut conn, proto::OP_OPEN, &hb)?;
    let (st, _, _) = proto::recv_response(&mut reader)?;
    if st != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("arbiter {addr} rejected token (status {st})"),
        ));
    }
    // Fence claim + lease: fold our epoch, ask for a `ttl` lease, and get back
    // the previously-held token plus whether the arbiter granted the lease and
    // when it expires (the caller only needs the granted bit; the expiry is the
    // arbiter's own clock used to enforce non-overlap).
    let mut fb = Vec::new();
    proto::put_u64(&mut fb, my_epoch as u64);
    proto::put_u64(&mut fb, ttl.as_millis() as u64);
    proto::send_request(&mut conn, proto::OP_FENCE, &fb)?;
    let (_, prev, data) = proto::recv_response(&mut reader)?;
    // Response data = [u64 granted_until_ms][u8 granted]; a truncated payload is
    // read as not-granted so an unexpected arbiter reply fails safe (closed).
    let granted = data.len() >= 9 && data[8] == 1;
    Ok((prev as i64, granted))
}

/// Number of independent arbiter grants needed to hold write authority: a strict
/// majority of the `n` configured arbiters. `n == 1` yields `1`, so a single
/// `--witness` behaves exactly as before — the quorum path is a strict superset,
/// never a regression. Recommend an odd `n` (1, 3, 5) so no tie is possible.
fn quorum_size(n: usize) -> usize {
    n / 2 + 1
}

/// Aggregated verdict from probing every arbiter once, classified against the
/// caller's epoch. Kept free of I/O (built by [`classify`]) so the majority logic
/// is deterministically unit-testable.
#[derive(Debug, Default, PartialEq, Eq)]
struct Quorum {
    /// Arbiters we personally lifted to `epoch` this round (`prev < epoch` AND
    /// granted). Reaching a majority of these is the EXCLUSIVE right to promote:
    /// two nodes cannot both do so, because their majority sets intersect and
    /// only the first claimer advances the shared arbiter (the later one sees
    /// `prev == epoch` — a renew, not an advance).
    advances: usize,
    /// Arbiters already at `epoch` that renewed our lease (`prev == epoch`).
    held: usize,
    /// Arbiters holding a strictly higher token (`prev > epoch`): a peer is ahead
    /// there. A majority of these proves we have been fenced.
    ahead: usize,
    /// Highest `prev` seen across reachable arbiters (never below our own epoch),
    /// used to adopt the leader's epoch so our future candidates stay monotonic.
    prev_max: i64,
    /// Arbiters we could talk to at all this round.
    reachable: usize,
}

impl Quorum {
    /// We may serve as primary at `epoch`: a majority holds our (renewed or just
    /// advanced) lease.
    fn leased(&self, q: usize) -> bool {
        self.advances + self.held >= q
    }
    /// We newly won leadership at `epoch`. Promotion requires ADVANCING a
    /// majority, not merely holding one — this is what fences same-value ties so
    /// exactly one node seizes a fresh epoch.
    fn won(&self, q: usize) -> bool {
        self.advances >= q
    }
    /// A peer provably holds a majority at a strictly higher epoch: stand down.
    fn fenced(&self, q: usize) -> bool {
        self.ahead >= q
    }
}

/// Classify reachable arbiter replies `(prev, granted)` against `epoch` into a
/// [`Quorum`]. Pure, for deterministic testing. `reachable` is the reply count;
/// arbiters that errored are simply absent (a vote we did not get).
fn classify(replies: &[(i64, bool)], epoch: i64) -> Quorum {
    let mut q = Quorum {
        prev_max: epoch,
        reachable: replies.len(),
        ..Default::default()
    };
    for &(prev, granted) in replies {
        if prev > epoch {
            q.ahead += 1;
        } else if granted {
            if prev < epoch {
                q.advances += 1;
            } else {
                q.held += 1;
            }
        }
        // `granted == false && prev <= epoch`: a live incumbent lease at an equal
        // or lower token blocks us at this arbiter — counts as neither.
        q.prev_max = q.prev_max.max(prev);
    }
    q
}

/// Probe every configured arbiter CONCURRENTLY and aggregate a [`Quorum`]. This
/// is what turns the single witness into a majority-fencing quorum: authority
/// needs `quorum_size(n)` grants, so losing a minority of arbiters — or their
/// token stores — neither freezes writes (B) nor lets a stale primary resurrect
/// (C). `n == 1` short-circuits to a single in-line probe (no threads, identical
/// to the pre-quorum behavior).
fn witness_quorum(addrs: &[String], token: &str, epoch: i64, ttl: Duration, timeout: Duration) -> Quorum {
    if addrs.len() == 1 {
        return match witness_probe_one(&addrs[0], token, epoch, ttl, timeout) {
            Ok((prev, granted)) => classify(&[(prev, granted)], epoch),
            Err(_) => classify(&[], epoch),
        };
    }
    let replies: Vec<(i64, bool)> = std::thread::scope(|s| {
        let handles: Vec<_> = addrs
            .iter()
            .map(|a| s.spawn(|| witness_probe_one(a, token, epoch, ttl, timeout).ok()))
            .collect();
        handles
            .into_iter()
            .filter_map(|h| h.join().ok().flatten())
            .collect()
    });
    classify(&replies, epoch)
}

/// Primary-side replication sender. Every [`REPL_INTERVAL`] it heartbeats its
/// epoch to the peer and ships a consistent snapshot of every view db.
///
/// Honest limits: this is an ASYNC archival copy, so `RPO ~= REPL_INTERVAL` —
/// writes committed just before a crash may not have been pushed and are lost on
/// failover. If the peer reports a higher epoch ([`E_EPOCH`]) we are a stale
/// ex-primary and demote ourselves to standby.
fn start_replicator(rt: Arc<MetaRuntime>) {
    thread::spawn(move || {
        let peer = match &rt.peer {
            Some(p) => p.clone(),
            None => return,
        };
        let ha = match &rt.ha {
            Some(h) => h.clone(),
            None => return,
        };
        // Optional third arbiter: re-claim our epoch at the witness every tick,
        // so a meta<->meta partition (where the peer M_PING path is blind) still
        // fences a stale primary that a peer has promoted past.
        let witness = rt.witness.clone();
        let token = rt.token.clone();
        // When we first failed to reach a configured witness; drives the
        // fail-safe grace so a sustained arbiter outage (not a blip) costs
        // availability instead of risking a double-write.
        let mut witness_fail: Option<Instant> = None;
        loop {
            thread::sleep(REPL_INTERVAL);
            if ha.role() != Role::Primary {
                witness_fail = None; // the promotion watchdog owns us while standby
                continue;
            }
            // Witness fencing (independent of peer reachability). We re-claim our
            // epoch at the arbiter quorum every tick: a majority reporting
            // `prev > mine` means a peer promoted past us and we stand down; a
            // majority holding our lease means we still lead. If a witness set is
            // configured but we cannot form a lease quorum for WITNESS_FENCE_GRACE
            // we can no longer *prove* exclusive authority, and a partitioned peer
            // may have promoted without us seeing it — so we FAIL SAFE and self-demote
            // rather than keep writing into a possible double-primary. Losing a
            // minority of arbiters is tolerated (the quorum still renews).
            if let Some(w) = &witness {
                let mine = ha.epoch.load(Ordering::SeqCst);
                let addrs = split_addrs(w);
                let q = quorum_size(addrs.len());
                let res = witness_quorum(&addrs, &token, mine, WITNESS_LEASE, CONTROL_TIMEOUT);
                if res.fenced(q) {
                    witness_fail = None;
                    ha.set_role(Role::Standby);
                    ha.epoch.store(res.prev_max, Ordering::SeqCst);
                    ha.lease_clear();
                    rt.cache.lock().clear();
                    *ha.last_peer.lock() = Instant::now();
                    eprintln!("meta HA: demoted to standby (a quorum of {q}/{} arbiters holds {} > our {mine})", addrs.len(), res.prev_max);
                    continue;
                }
                if res.leased(q) {
                    // Still the majority holder: renew the lease so writes keep
                    // taking the local fast path instead of paying a round-trip.
                    witness_fail = None;
                    ha.lease_renew();
                } else {
                    // Cannot reach or hold a lease quorum: a partitioned peer may
                    // have promoted without us seeing it, so FAIL SAFE after the
                    // grace — a blip costs nothing, a sustained outage costs
                    // availability instead of risking a double-write.
                    let now = Instant::now();
                    let fenced = match witness_fail {
                        Some(t) => t.elapsed() >= WITNESS_FENCE_GRACE,
                        None => {
                            witness_fail = Some(now);
                            false
                        }
                    };
                    if fenced {
                        ha.set_role(Role::Standby);
                        ha.lease_clear();
                        rt.cache.lock().clear();
                        *ha.last_peer.lock() = now;
                        eprintln!("meta HA: cannot form lease quorum ({}/{} reachable, need {q}) >{WITNESS_FENCE_GRACE:?}; fail-safe self-demote to standby (cannot prove exclusive authority; refusing to keep writing)", res.reachable, addrs.len());
                        continue;
                    }
                    eprintln!("meta HA: cannot form lease quorum ({}/{} reachable, need {q}); within grace, holding primary", res.reachable, addrs.len());
                }
            }
            let mut conn = match TcpStream::connect(&peer) {
                Ok(c) => c,
                Err(_) => continue, // peer down: nothing to ship this cycle
            };
            let _ = conn.set_read_timeout(Some(CONTROL_TIMEOUT));
            let _ = conn.set_write_timeout(Some(CONTROL_TIMEOUT));
            let mut hb = Vec::new();
            put_i64(&mut hb, ha.epoch.load(Ordering::SeqCst));
            if proto::send_request(&mut conn, M_PING, &hb).is_err() {
                continue;
            }
            let (st, peer_epoch, _) = match proto::recv_response(&mut conn) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if st == E_EPOCH {
                // Peer is authoritative at a higher epoch: stand down to standby.
                ha.set_role(Role::Standby);
                ha.epoch.store((peer_epoch as i64).max(ha.epoch.load(Ordering::SeqCst)), Ordering::SeqCst);
                rt.cache.lock().clear(); // reopen views read-only for serving
                *ha.last_peer.lock() = Instant::now(); // don't instantly re-promote
                eprintln!("meta HA: demoted to standby (peer epoch {peer_epoch})");
                continue;
            }
            // Under --sync-repl every mutating op already pushed its own
            // snapshot; the periodic bulk push would race with it on the
            // standby's install path. Skip the M_PUSH loop but keep heartbeat
            // (M_PING) and event relay (M_FORWARD) running.
            if !rt.sync_repl {
                for view in list_users(&rt.base) {
                    let engine = match engine_for(&rt, &view) {
                        Ok(e) => e,
                        Err(_) => continue,
                    };
                    let snap = match engine.backup_to_bytes() {
                        Ok(s) => s,
                        Err(_) => continue,
                    };
                    let mut body = Vec::with_capacity(view.len() + 2 + snap.len());
                    proto::put_key(&mut body, &view);
                    body.extend_from_slice(&snap);
                    if proto::send_request(&mut conn, M_PUSH, &body).is_err() {
                        break;
                    }
                    if proto::recv_response(&mut conn).is_err() {
                        break;
                    }
                }
            }
            // Relay any invalidation events produced since the last tick so
            // standby-mounted subscribers converge too (best-effort: dropped if
            // the peer link is already broken this cycle).
            let pending: Vec<(String, Vec<Event>)> = std::mem::take(&mut *rt.forward.lock());
            for (view, events) in pending {
                let mut body = Vec::new();
                proto::put_key(&mut body, &view);
                put_events(&mut body, &events);
                if proto::send_request(&mut conn, M_FORWARD, &body).is_err() {
                    break;
                }
                if proto::recv_response(&mut conn).is_err() {
                    break;
                }
            }
        }
    });
}

/// Standby-side promotion watchdog. If the primary's heartbeats stop for longer
/// than [`PROMOTE_TIMEOUT`], presume the primary dead and promote this node:
/// bump the fencing epoch (so a recovered old primary stands down), reopen views
/// read-write, and persist the new epoch into every replicated db.
///
/// Honest limits: `RPO ~= REPL_INTERVAL` (data the primary committed after its
/// last push is lost). Without a witness, the two-node pair has a bounded
/// double-write window (up to `PROMOTE_TIMEOUT`) in which a partitioned old
/// primary can still accept writes before its own replicator learns it was
/// demoted. With `--witness` (one or more shared `data-server` arbiters) that
/// window is closed from *both* sides, fail-safe, by MAJORITY: the would-be
/// promoter advances its candidate epoch at a strict quorum (`quorum_size`) of the
/// arbiters and refuses to seize primary unless it wins there (`prev < candidate`
/// granted at a majority), and the stale primary re-claims every tick and
/// self-demotes when a quorum reports `prev > mine`. Because two majorities always
/// intersect, exactly one node can advance a quorum, so a same-value promotion race
/// yields one primary. If a configured witness set cannot form a lease quorum we do
/// NOT fall back to blind promotion — we cannot prove exclusive authority, so the
/// standby refuses to promote and a primary self-demotes after
/// [`WITNESS_FENCE_GRACE`]. This trades availability for safety: losing a quorum of
/// arbiters freezes writes instead of risking a double-primary (losing a minority
/// does not — the quorum still holds).
fn start_promotion_watchdog(rt: Arc<MetaRuntime>, ha: Arc<HaState>) {
    let witness = rt.witness.clone();
    let token = rt.token.clone();
    thread::spawn(move || loop {
        thread::sleep(WATCHDOG_INTERVAL);
        if ha.role() != Role::Standby {
            continue;
        }
        // A node with no peer to listen for is never heartbeated; only promote
        // when we do expect a primary (peer configured).
        if rt.peer.is_none() {
            continue;
        }
        if ha.last_peer.lock().elapsed() < PROMOTE_TIMEOUT {
            continue;
        }
        let candidate = ha.epoch.load(Ordering::SeqCst) + 1;
        // Third-arbiter fencing: take a LEASE on `candidate` at the witness
        // before seizing primary. The arbiter grants it only when no live lease
        // for an incumbent blocks it, so a promotion here is non-overlapping by
        // construction: `granted` with `prev < candidate` means we now exclusively
        // hold the token; a refusal (`granted = false`) means a peer still owns
        // that epoch or its lease has not yet lapsed, so we back off and retry
        // only after that lease expires — never promoting into a live lease. If
        // the witness is configured but unreachable we cannot prove the claim, so
        // we FAIL SAFE and refuse: promoting blind on peer-only evidence is
        // exactly the split-brain the witness exists to prevent.
        if let Some(w) = &witness {
            let addrs = split_addrs(w);
            let q = quorum_size(addrs.len());
            let res = witness_quorum(&addrs, &token, candidate, WITNESS_LEASE, CONTROL_TIMEOUT);
            // Won outright: we RAISED the token at a MAJORITY of arbiters
            // (`prev < candidate` AND granted). Quorum intersection means two
            // nodes cannot both advance a majority this round — their arbiter
            // sets share at least one node, and only the first claimer raises it
            // (the later sees `prev == candidate`, a renew, not an advance) — so
            // exactly ONE promoter seizes `candidate`. Fall through to promote.
            if !res.won(q) {
                // Every other arbiter verdict keeps us standby:
                //  - a majority holds `>= candidate`: a peer is ahead or a
                //    same-value tie already raised it — only ONE node can advance
                //    a majority, so the tie is fenced (no double-primary).
                //  - below a reachable/incumbent quorum: an incumbent lease is
                //    still live, or too few arbiters are reachable, so we cannot
                //    PROVE an exclusive claim and refuse to promote blind.
                ha.epoch.store(res.prev_max.max(candidate), Ordering::SeqCst);
                *ha.last_peer.lock() = Instant::now(); // back off, retry next window
                if res.fenced(q) {
                    eprintln!("meta HA: promotion refused (a quorum of {q}/{} arbiters holds {} >= candidate {candidate}); fail-safe refusing to promote (a peer is provably ahead)", addrs.len(), res.prev_max);
                } else {
                    eprintln!("meta HA: promotion deferred (advanced only {}/{} arbiter(s), need quorum {q}); fail-safe refusing to promote (cannot prove exclusive arbiter authority), will retry after any incumbent lease expires", res.advances, addrs.len());
                }
                continue;
            }
            ha.epoch.store(candidate, Ordering::SeqCst);
            ha.lease_renew(); // we hold the granted lease: first writes take the fast path
        } else {
            ha.epoch.fetch_add(1, Ordering::SeqCst);
        }
        let new_epoch = candidate;
        ha.set_role(Role::Primary);
        rt.cache.lock().clear(); // reopen views read-write on next access
        for view in list_users(&rt.base) {
            if let Ok(eng) = engine_for(&rt, &view) {
                let _ = eng.set_epoch(new_epoch);
            }
        }
        *ha.last_peer.lock() = Instant::now(); // avoid re-firing every tick
        eprintln!("meta HA: promoted to primary at epoch {new_epoch}");
    });
}

/// Execute one metadata operation against the view's engine.
fn dispatch_meta(engine: &MetaStore, op: u8, body: &[u8]) -> (i32, u64, Vec<u8>, Vec<Event>) {
    let mut i = 0usize;
    macro_rules! bail {
        ($e:expr) => {
            (err_code(&$e), 0, Vec::new(), Vec::new())
        };
    }
    match op {
        M_LOOKUP => {
            let parent = match get_i64(body, &mut i) { Ok(v) => v, Err(_) => return (E_DB, 0, Vec::new(), Vec::new()) };
            let name = match proto::get_key(body, &mut i) { Ok(v) => v, Err(_) => return (E_DB, 0, Vec::new(), Vec::new()) };
            match engine.lookup(parent, &name) {
                Some(n) => { let mut d = Vec::new(); put_node(&mut d, &n); (0, 0, d, Vec::new()) }
                None => (E_NOTFOUND, 0, Vec::new(), Vec::new()),
            }
        }
        M_GETATTR => {
            let ino = match get_i64(body, &mut i) { Ok(v) => v, Err(_) => return (E_DB, 0, Vec::new(), Vec::new()) };
            match engine.getattr(ino) {
                Some(n) => { let mut d = Vec::new(); put_node(&mut d, &n); (0, 0, d, Vec::new()) }
                None => (E_NOTFOUND, 0, Vec::new(), Vec::new()),
            }
        }
        M_READDIR => {
            let parent = match get_i64(body, &mut i) { Ok(v) => v, Err(_) => return (E_DB, 0, Vec::new(), Vec::new()) };
            let nodes = engine.readdir(parent);
            let mut d = Vec::new();
            put_nodes(&mut d, &nodes);
            (0, 0, d, Vec::new())
        }
        M_CREATE => {
            let parent = match get_i64(body, &mut i) { Ok(v) => v, Err(_) => return (E_DB, 0, Vec::new(), Vec::new()) };
            let is_dir = get_u8(body, &mut i).unwrap_or(0) != 0;
            let data_ref = match get_opt(body, &mut i) { Ok(v) => v, Err(_) => return (E_DB, 0, Vec::new(), Vec::new()) };
            let mode = proto::read_u32(body, &mut i).unwrap_or(0);
            let uid = proto::read_u32(body, &mut i).unwrap_or(0);
            let gid = proto::read_u32(body, &mut i).unwrap_or(0);
            let name = match proto::get_key(body, &mut i) { Ok(v) => v, Err(_) => return (E_DB, 0, Vec::new(), Vec::new()) };
            match engine.create_node(parent, &name, is_dir, data_ref.as_deref(), mode, uid, gid) {
                Ok(id) => (0, id as u64, Vec::new(), vec![Event::Entry { parent, name }]),
                Err(e) => bail!(e),
            }
        }
        M_SYMLINK => {
            let parent = match get_i64(body, &mut i) { Ok(v) => v, Err(_) => return (E_DB, 0, Vec::new(), Vec::new()) };
            let target = match proto::get_key(body, &mut i) { Ok(v) => v, Err(_) => return (E_DB, 0, Vec::new(), Vec::new()) };
            let uid = proto::read_u32(body, &mut i).unwrap_or(0);
            let gid = proto::read_u32(body, &mut i).unwrap_or(0);
            let name = match proto::get_key(body, &mut i) { Ok(v) => v, Err(_) => return (E_DB, 0, Vec::new(), Vec::new()) };
            match engine.create_symlink(parent, &name, &target, uid, gid) {
                Ok(id) => (0, id as u64, Vec::new(), vec![Event::Entry { parent, name }]),
                Err(e) => bail!(e),
            }
        }
        M_LINK => {
            let src_ino = match get_i64(body, &mut i) { Ok(v) => v, Err(_) => return (E_DB, 0, Vec::new(), Vec::new()) };
            let new_parent = match get_i64(body, &mut i) { Ok(v) => v, Err(_) => return (E_DB, 0, Vec::new(), Vec::new()) };
            let new_name = match proto::get_key(body, &mut i) { Ok(v) => v, Err(_) => return (E_DB, 0, Vec::new(), Vec::new()) };
            match engine.link_node(src_ino, new_parent, &new_name) {
                Ok(id) => (
                    0,
                    id as u64,
                    Vec::new(),
                    vec![Event::Entry { parent: new_parent, name: new_name }, Event::Inode { ino: src_ino }],
                ),
                Err(e) => bail!(e),
            }
        }
        M_UNLINK => {
            let parent = match get_i64(body, &mut i) { Ok(v) => v, Err(_) => return (E_DB, 0, Vec::new(), Vec::new()) };
            let name = match proto::get_key(body, &mut i) { Ok(v) => v, Err(_) => return (E_DB, 0, Vec::new(), Vec::new()) };
            match engine.unlink(parent, &name) {
                Ok(gc) => {
                    let mut d = Vec::new();
                    if let Some(k) = gc {
                        proto::put_key(&mut d, &k);
                    }
                    (0, 0, d, vec![Event::Entry { parent, name }])
                }
                Err(e) => bail!(e),
            }
        }
        M_RENAME => {
            let parent = match get_i64(body, &mut i) { Ok(v) => v, Err(_) => return (E_DB, 0, Vec::new(), Vec::new()) };
            let name = match proto::get_key(body, &mut i) { Ok(v) => v, Err(_) => return (E_DB, 0, Vec::new(), Vec::new()) };
            let new_parent = match get_i64(body, &mut i) { Ok(v) => v, Err(_) => return (E_DB, 0, Vec::new(), Vec::new()) };
            let new_name = match proto::get_key(body, &mut i) { Ok(v) => v, Err(_) => return (E_DB, 0, Vec::new(), Vec::new()) };
            // Trailing RENAME_NOREPLACE flag byte; absent (older client) => 0.
            let noreplace = body.get(i).copied().unwrap_or(0) != 0;
            match engine.rename(parent, &name, new_parent, &new_name, noreplace) {
                Ok(gc) => {
                    let mut d = Vec::new();
                    if let Some(k) = gc {
                        proto::put_key(&mut d, &k);
                    }
                    (
                        0,
                        0,
                        d,
                        vec![Event::Entry { parent, name }, Event::Entry { parent: new_parent, name: new_name }],
                    )
                }
                Err(e) => bail!(e),
            }
        }
        M_SETMODE => {
            let ino = match get_i64(body, &mut i) { Ok(v) => v, Err(_) => return (E_DB, 0, Vec::new(), Vec::new()) };
            let mode = proto::read_u32(body, &mut i).unwrap_or(0);
            engine.set_mode(ino, mode);
            (0, 0, Vec::new(), inode_change_events(engine, ino))
        }
        M_SETUIDGID => {
            let ino = match get_i64(body, &mut i) { Ok(v) => v, Err(_) => return (E_DB, 0, Vec::new(), Vec::new()) };
            let uid = proto::read_u32(body, &mut i).unwrap_or(0);
            let gid = proto::read_u32(body, &mut i).unwrap_or(0);
            engine.set_uid_gid(ino, uid, gid);
            (0, 0, Vec::new(), inode_change_events(engine, ino))
        }
        M_SETTIMES => {
            let ino = match get_i64(body, &mut i) { Ok(v) => v, Err(_) => return (E_DB, 0, Vec::new(), Vec::new()) };
            let atime = get_opt_i64(body, &mut i).unwrap_or(None);
            let mtime = get_opt_i64(body, &mut i).unwrap_or(None);
            engine.set_times(ino, atime, mtime);
            (0, 0, Vec::new(), inode_change_events(engine, ino))
        }
        M_TOUCH => {
            let ino = match get_i64(body, &mut i) { Ok(v) => v, Err(_) => return (E_DB, 0, Vec::new(), Vec::new()) };
            engine.touch_mtime(ino);
            (0, 0, Vec::new(), inode_change_events(engine, ino))
        }
        M_SETLK => {
            let ino = match get_i64(body, &mut i) { Ok(v) => v, Err(_) => return (E_DB, 0, Vec::new(), Vec::new()) };
            let owner = proto::read_u64(body, &mut i).unwrap_or(0);
            let start = proto::read_u64(body, &mut i).unwrap_or(0);
            let end = proto::read_u64(body, &mut i).unwrap_or(u64::MAX);
            let ty = LockType::from_u8(get_u8(body, &mut i).unwrap_or(2)).unwrap_or(LockType::Unlock);
            let pid = proto::read_u32(body, &mut i).unwrap_or(0);
            let sid = proto::read_u64(body, &mut i).unwrap_or(0);
            match engine.lock_op(ino, owner, start, end, ty, pid, sid) {
                Ok(None) => (0, 0, Vec::new(), Vec::new()),
                Ok(Some(c)) => {
                    let mut d = Vec::new();
                    encode_lock_conflict(&c, &mut d);
                    (0, 1, d, Vec::new())
                }
                Err(e) => (err_code(&e), 0, Vec::new(), Vec::new()),
            }
        }
        M_GETLK => {
            let ino = match get_i64(body, &mut i) { Ok(v) => v, Err(_) => return (E_DB, 0, Vec::new(), Vec::new()) };
            let owner = proto::read_u64(body, &mut i).unwrap_or(0);
            let start = proto::read_u64(body, &mut i).unwrap_or(0);
            let end = proto::read_u64(body, &mut i).unwrap_or(u64::MAX);
            let ty = LockType::from_u8(get_u8(body, &mut i).unwrap_or(0)).unwrap_or(LockType::Read);
            match engine.get_lock(ino, owner, start, end, ty) {
                None => (0, 0, Vec::new(), Vec::new()),
                Some(c) => {
                    let mut d = Vec::new();
                    encode_lock_conflict(&c, &mut d);
                    (0, 1, d, Vec::new())
                }
            }
        }
        M_RELEASE_LOCKS => {
            let owner = proto::read_u64(body, &mut i).unwrap_or(0);
            engine.release_locks_of_owner(owner);
            (0, 0, Vec::new(), Vec::new())
        }
        M_RENEW => {
            let sid = proto::read_u64(body, &mut i).unwrap_or(0);
            engine.renew_locks(sid);
            (0, 0, Vec::new(), Vec::new())
        }
        _ => (E_DB, 0, Vec::new(), Vec::new()),
    }
}

// ===================== client: invalidation listener =======================

/// Open a dedicated event-subscription connection: run `M_OPEN` + `M_SUBSCRIBE`
/// and return a socket the caller only *reads* — the server pushes invalidation
/// frames for every mutating op another client of this view performs. Public so
/// a test can attach a raw subscriber without a FUSE session.
///
/// `addr` may be a comma-separated failover list; each address is tried in order
/// and the first successful subscription is returned (a token rejection is fatal
/// and not retried against a backup). Used by the mount's listener thread so
/// invalidation pushes keep flowing after a failover.
pub fn open_event_stream(addr: &str, view: &str, token: &str, uid: u32) -> io::Result<TcpStream> {
    let mut last = io::Error::new(io::ErrorKind::ConnectionRefused, "no meta addr");
    for a in split_addrs(addr) {
        match try_open_event_stream(&a, view, token, uid) {
            Ok(s) => return Ok(s),
            // A rejected token is a config error on every node; don't rotate.
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => return Err(e),
            Err(e) => last = e,
        }
    }
    Err(last)
}

fn try_open_event_stream(addr: &str, view: &str, token: &str, uid: u32) -> io::Result<TcpStream> {
    let mut s = TcpStream::connect(addr)?;
    s.set_read_timeout(None)?; // block waiting for pushed events
    s.set_write_timeout(Some(Duration::from_secs(5)))?;
    s.set_nodelay(true)?;
    let mut body = Vec::new();
    proto::put_key(&mut body, view);
    proto::put_key(&mut body, token);
    proto::put_u32(&mut body, uid);
    proto::send_request(&mut s, M_OPEN, &body)?;
    let (st, _, _) = proto::recv_response(&mut s)?;
    if st == E_AUTH {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "meta-server rejected the access token",
        ));
    }
    if st != 0 {
        return Err(io::Error::from_raw_os_error(EIO));
    }
    proto::send_request(&mut s, M_SUBSCRIBE, &[])?;
    let (ack, _, _) = proto::recv_response(&mut s)?;
    if ack != 0 {
        return Err(io::Error::from_raw_os_error(EIO));
    }
    Ok(s)
}

/// Split a `--meta` value into a failover address list. Empty/whitespace-only
/// inputs collapse to the original string so behavior never silently vanishes.
fn split_addrs(s: &str) -> Vec<String> {
    let v: Vec<String> = s
        .split(',')
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .collect();
    if v.is_empty() {
        vec![s.to_string()]
    } else {
        v
    }
}

/// Keep this mount's kernel cache coherent with peers WITHOUT `--strict`: connect
/// an event stream to the meta-server and translate each pushed frame into a
/// [`Notifier`] call (`inval_entry` / `inval_inode`), so a peer's create/delete/
/// rename and even an overwrite of an already-cached file become visible right
/// away. Reconnects with a small bounded backoff across a server restart; stops
/// permanently only on a fatal token rejection. Spawned by the mount process for
/// `--meta` mounts; abandoned harmlessly when the process exits at unmount.
pub fn spawn_invalidation_listener(
    addr: &str,
    view: &str,
    token: &str,
    uid: u32,
    notifier: Notifier,
) -> thread::JoinHandle<()> {
    let addr = addr.to_string();
    let view = view.to_string();
    let token = token.to_string();
    thread::spawn(move || loop {
        match run_event_stream(&addr, &view, &token, uid, &notifier) {
            // A rejected token is a config error; retrying can't fix it.
            Err(ref e) if e.kind() == io::ErrorKind::PermissionDenied => {
                eprintln!("invalidation listener stopped: token rejected");
                return;
            }
            // EOF / transport error: back off briefly, then reconnect fresh.
            _ => thread::sleep(Duration::from_millis(300)),
        }
    })
}

fn run_event_stream(addr: &str, view: &str, token: &str, uid: u32, notifier: &Notifier) -> io::Result<()> {
    let mut s = open_event_stream(addr, view, token, uid)?;
    loop {
        let (kind, val, data) = proto::recv_response(&mut s)?;
        match kind {
            EV_ENTRY => {
                let mut i = 0usize;
                if let Ok(name) = proto::get_key(&data, &mut i) {
                    // A missing/already-evicted entry is harmless; ignore errors.
                    let _ = notifier.inval_entry(INodeNo(val), OsStr::new(&name));
                }
            }
            EV_INODE => {
                // offset 0, len -1 => invalidate the whole inode (attrs + pages),
                // so a peer's write to a file we have open is seen on next read.
                let _ = notifier.inval_inode(INodeNo(val), 0, -1);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod ha_tests {
    use super::*;

    #[test]
    fn split_addrs_parses_failover_list() {
        // The client's `--meta a,b` parsing that drives failover.
        assert_eq!(split_addrs("10.0.0.1:1,10.0.0.2:1"), vec!["10.0.0.1:1", "10.0.0.2:1"]);
        // whitespace tolerated, empties dropped
        assert_eq!(split_addrs(" a , , b "), vec!["a", "b"]);
        // a single address stays a single-element list (legacy behavior intact)
        assert_eq!(split_addrs("only:1"), vec!["only:1"]);
        // a degenerate all-empty input collapses to the original so nothing vanishes
        assert_eq!(split_addrs(""), vec![""]);
    }

    #[test]
    fn forward_event_codec_roundtrips() {
        // A forwarded batch must decode to the identical events on the standby,
        // preserving Entry(parent,name) and Inode(ino) payloads.
        let events = vec![
            Event::Entry { parent: 1, name: "alpha.txt".to_string() },
            Event::Inode { ino: 42 },
            Event::Entry { parent: 7, name: "dir/sub".to_string() },
        ];
        let mut body = Vec::new();
        proto::put_key(&mut body, "alice");
        put_events(&mut body, &events);
        let mut i = 0usize;
        let view = proto::get_key(&body, &mut i).unwrap();
        let back = events_from(&body, &mut i).unwrap();
        assert_eq!(view, "alice");
        assert_eq!(back.len(), 3);
        match (&back[0], &back[2]) {
            (Event::Entry { parent: p1, name: n1 }, Event::Entry { parent: p2, name: n2 }) => {
                assert_eq!((*p1, n1.as_str()), (1, "alpha.txt"));
                assert_eq!((*p2, n2.as_str()), (7, "dir/sub"));
            }
            _ => panic!("entry events did not round-trip"),
        }
        match &back[1] {
            Event::Inode { ino } => assert_eq!(*ino, 42),
            _ => panic!("inode event did not round-trip"),
        }
    }

    #[test]
    fn write_ops_are_gated_reads_are_not() {
        // The standby's read-only gate keys off exactly this classification.
        for op in [M_CREATE, M_SYMLINK, M_LINK, M_UNLINK, M_RENAME, M_SETMODE, M_SETUIDGID, M_SETTIMES, M_TOUCH] {
            assert!(is_write_op(op), "op {op} must be write-intent");
        }
        for op in [M_OPEN, M_LOOKUP, M_GETATTR, M_READDIR, M_SUBSCRIBE, M_PING, M_PUSH, M_FORWARD] {
            assert!(!is_write_op(op), "op {op} must not be write-intent");
        }
    }

    #[test]
    fn fenced_for_write_demotes_on_higher_witness_epoch() {
        // Zero-window fencing: a primary that has been fenced at the arbiter (a
        // peer promoted past it) must refuse its very NEXT write instead of
        // serving writes until the async replicator notices (<= REPL_INTERVAL).
        use crate::backend::{self, StorageBackend};
        use crate::remote::server::{serve_on_auth, Fence};

        let dir = tempfile::Builder::new().prefix("yf_fencewt_").tempdir().unwrap();
        let backend: Arc<dyn StorageBackend> = Arc::from(backend::open_local(dir.path()).unwrap());
        let fence = Arc::new(Fence::new_in_memory());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let waddr = listener.local_addr().unwrap().to_string();
        let f = fence.clone();
        std::thread::spawn(move || serve_on_auth(listener, backend, "", f));
        // Wait for the witness to accept (single-connect client, no retry inside).
        for _ in 0..50 {
            if TcpStream::connect(&waddr).is_ok() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }

        let ha = HaState::new(Role::Primary);
        ha.epoch.store(5, Ordering::SeqCst);
        let rt = Arc::new(MetaRuntime {
            base: dir.path().to_path_buf(),
            token: String::new(),
            cache: Arc::new(Mutex::new(HashMap::new())),
            subs: Arc::new(Mutex::new(HashMap::new())),
            ha: Some(ha.clone()),
            peer: Some("127.0.0.1:1".to_string()),
            forward: Mutex::new(Vec::new()),
            sync_repl: false,
            standby_push: Mutex::new(()),
            witness: Some(waddr),
            acl: Arc::new(crate::acl::AclStore::open(dir.path()).unwrap()),
            store: StoreKind::Sqlite,
        });

        // Witness holds nothing above our epoch: the first proof grants a lease
        // and we keep authority, staying Primary.
        assert!(!rt.fenced_for_write(), "a still-authoritative primary must not self-fence");
        assert_eq!(ha.role(), Role::Primary);
        assert!(ha.lease_valid(), "a successful proof establishes a live lease");

        // With a live lease the next write is admitted from the LOCAL fast path —
        // no arbiter round-trip, because the witness cannot have handed the lease
        // to a peer while ours is live (non-overlap is the arbiter's guarantee).
        assert!(
            !rt.fenced_for_write(),
            "a live lease admits writes without re-proving at the arbiter"
        );

        // Model a real failover: a peer can only take the token AFTER our lease
        // lapses. Expire our lease, then fold a higher epoch at the arbiter as a
        // promoted peer would on the now-vacant field.
        ha.lease_clear();
        fence.claim(9);

        // The very next write must re-prove, see the higher epoch, demote us, and
        // adopt it so our own future candidate claims stay monotonic.
        assert!(
            rt.fenced_for_write(),
            "an expired lease forces re-proof, which must fence a higher witness epoch"
        );
        assert_eq!(ha.role(), Role::Standby, "fence must demote to standby immediately");
        assert_eq!(ha.epoch.load(Ordering::SeqCst), 9, "must adopt the witness epoch");
    }

    #[test]
    fn quorum_size_is_a_strict_majority() {
        // A single arbiter (the legacy `--witness` shape) needs exactly 1 grant,
        // so the quorum path is a strict superset of the old single-witness one.
        assert_eq!(quorum_size(1), 1);
        assert_eq!(quorum_size(2), 2, "2 nodes need BOTH (no majority shortcut)");
        assert_eq!(quorum_size(3), 2);
        assert_eq!(quorum_size(4), 3);
        assert_eq!(quorum_size(5), 3);
    }

    #[test]
    fn classify_single_arbiter_matches_legacy_verdicts() {
        // N=1: quorum_size==1, so a single reply drives the verdict exactly as the
        // old single witness did — advancing wins+leases, being ahead fences.
        let q = quorum_size(1);
        let ahead_of_us = classify(&[(7, true)], 5);
        assert!(ahead_of_us.fenced(q), "a higher token at the lone arbiter fences");
        assert!(!ahead_of_us.leased(q));
        let we_lead = classify(&[(3, true)], 5);
        assert!(we_lead.won(q) && we_lead.leased(q), "raising the lone arbiter wins");
        assert!(!we_lead.fenced(q));
        let unreachable = classify(&[], 5); // the probe errored: no reply
        assert_eq!(unreachable.reachable, 0);
        assert!(!unreachable.won(q) && !unreachable.leased(q) && !unreachable.fenced(q));
    }

    #[test]
    fn classify_requires_a_majority_to_lease_or_fence() {
        // 3 arbiters, quorum 2: a lone grant is not enough to lease, a lone
        // higher token is not enough to fence — losing a minority neither
        // resurrects a stale primary (C) nor freezes writes (B).
        let q = quorum_size(3);
        let one_grant = classify(&[(3, true), (3, false), (3, false)], 5);
        assert_eq!(one_grant.advances, 1);
        assert!(!one_grant.leased(q) && !one_grant.won(q), "1/3 is below quorum");
        let two_grants = classify(&[(3, true), (3, true), (3, false)], 5);
        assert!(two_grants.won(q) && two_grants.leased(q), "2/3 grants win the quorum");
        let one_ahead = classify(&[(7, false), (3, true), (3, true)], 5);
        assert!(!one_ahead.fenced(q), "one higher arbiter does not fence");
        let two_ahead = classify(&[(7, false), (8, false), (3, true)], 5);
        assert!(two_ahead.fenced(q), "a majority holding higher epochs fences us");
        assert_eq!(two_ahead.prev_max, 8, "adopt the highest witness token we saw");
    }

    #[test]
    fn same_value_tie_advances_only_a_majority_once() {
        // Quorum intersection: two nodes both claiming epoch 6 cannot each win.
        // The one that actually raised the arbiters sees `prev < 6` (advance);
        // the loser sees `prev == 6` (a renew = `held`, never `advances`), so
        // `won` (advances >= q) holds for exactly one of them — no double-primary.
        let q = quorum_size(3);
        let winner = classify(&[(4, true), (5, true), (5, true)], 6);
        assert_eq!(winner.advances, 3);
        assert!(winner.won(q));
        let loser = classify(&[(6, true), (6, true), (4, true)], 6);
        // Two arbiters already at 6 renew us (held), only one still advances:
        assert_eq!(loser.held, 2);
        assert_eq!(loser.advances, 1);
        assert!(!loser.won(q), "a same-value collision must NOT re-win the quorum");
    }
}

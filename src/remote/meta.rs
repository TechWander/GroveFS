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
//! `M_OPEN` handshake that binds a socket to a view and admits the caller: the
//! body carries `view`, the static cluster `token`, the self-reported `uid` and an
//! optional per-caller `api_key` (a `grovefs apikey --for mount|any` credential,
//! required only when the server runs `--require-api-key`). Responses reuse the
//! `(status, val, data)` triple: for metadata `status` is 0 on success or a small
//! [`MetaError`] code (1..7) — *not* an errno — and `data` carries serialized
//! [`NodeInfo`]. On an [`E_AUTH`] refusal `data` instead spells out the cause, so a
//! client can show the real reason (wrong token, unknown/revoked/scoped key,
//! view-binding refusal) rather than guessing from the status code alone.

use super::proto;
use crate::layout::{LayoutMeta, LayoutRules, ViewDef};
use crate::meta::{
    LockConflict, LockType, MetaEngine, MetaError, MetaProvider, MetaStore, NodeInfo, NodeKind,
};
use crate::stats;
#[cfg(feature = "native")]
use fuser::{INodeNo, Notifier};
use crate::platform::EIO;
use parking_lot::Mutex;
use std::collections::HashMap;
#[cfg(feature = "native")]
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
/// Paged variant of `M_READDIR`: `{parent, offset, limit}` -> one page of the
/// `ORDER BY name` listing. Sent only when the `M_OPEN` ack advertised that the
/// server pages in its own store. Read op.
const M_READDIRP: u8 = 37;
/// One-shot flat tree dump for server-side `du` aggregation. Request body is
/// empty (the view is already bound from the session's M_OPEN). Response DATA
/// is `[count: u32][id(i64) parent_id(i64) name(key) is_dir(u8) data_ref(key)]*`.
const M_TREE_DUMP: u8 = 38;

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
// Returned by the server on the M_OPEN handshake (and mid-session, on a revoke
// recheck) when the caller is not admitted: a token that does not match the
// server's configured one, a missing/unknown/revoked/wrong-scope api key when
// `--require-api-key` is on, or a view-bound key opening a view it is not
// bound to. Not a
// MetaError code — it only ever appears during connect (where the client maps it
// to a PermissionDenied error) or as a refusal on an established session. The
// `data` payload states the cause in plain text; an empty body carries no cause,
// so the client prints its generic "rejected the access token" wording.
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
        // A mid-session auth refusal (revoked key). Reported as a permission
        // failure, not a db fault: the mount is already being torn down by
        // [`RemoteMeta::note_refusal`], and `EPERM` is the honest answer for the
        // op that lost its credential.
        E_AUTH => MetaError::OpNotPermitted,
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
    Some(LockConflict {
        start,
        end,
        ty,
        pid,
    })
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
    /// API key (`<id>:<secret>`) presented on `M_OPEN` to a `meta-server`
    /// started with `--require-api-key`; empty when the server is gated by the
    /// static token (or by nothing).
    api_key: String,
    /// Set once the first `M_OPEN` has been accepted, so a later refusal is
    /// recognized as an auth LOSS on a live mount (a revoke landing
    /// mid-session) rather than a mount-setup configuration error.
    opened: std::sync::atomic::AtomicBool,
    /// Mount process uid presented on the `M_OPEN` handshake and recorded in
    /// the server's call audit as the presenting identity. Self-reported
    /// (advisory), not a hard identity — access control is the token / API-key gate.
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
    /// Set from the `M_OPEN` handshake ack when the mounted view name resolves
    /// to a server-side DERIVED view: virtual views are presentation-only and
    /// mount read-only. Refreshed on every (re)handshake so a failover re-reads
    /// the flag from the node actually serving us.
    readonly: AtomicU8,
    /// Nonzero when the `M_OPEN` handshake ack reported that the server's store
    /// can page a directory listing itself (SQLite or PostgreSQL backend). Only
    /// then does [`RemoteMeta::readdir_page`] issue the small `M_READDIRP` RPC
    /// and only then is READDIRPLUS offered. A zero flag leaves the client on
    /// the non-paged path: one full `M_READDIR` per kernel buffer, no
    /// READDIRPLUS. Refreshed on every (re)handshake so a failover re-reads the
    /// cap from the live node.
    paged: AtomicU8,
    /// Nonzero when the `M_OPEN` ack flagged a FORK view: its metadata tree is
    /// writable (mkdir/rename/unlink) but CONTENT writes are refused with
    /// EROFS, because the bytes are shared with the base view. Refreshed on
    /// every (re)handshake.
    content_ro: AtomicU8,
}

impl RemoteMeta {
    /// Connect to `addr` and complete the `M_OPEN` handshake for `view` with no
    /// token (matches a server started without one), so a bad address or an
    /// unknown-view failure surfaces at mount time.
    pub fn new(addr: &str, view: &str) -> io::Result<Self> {
        Self::new_with_token(addr, view, "")
    }

    /// The presenting uid: the mount process's real euid, reported to the
    /// server's call audit.
    fn self_uid() -> u32 {
        crate::platform::geteuid()
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
    /// a shared meta-server can be exercised/used under a chosen presenting uid.
    pub fn new_with_token_uid(addr: &str, view: &str, token: &str, uid: u32) -> io::Result<Self> {
        Self::new_with_creds(addr, view, token, uid, "")
    }

    /// Like [`RemoteMeta::new_with_token_uid`], but also presents `api_key`
    /// (`<id>:<secret>`, from `grovefs apikey create --for mount`) in the
    /// `M_OPEN` handshake — what a `meta-server --require-api-key` demands. The
    /// server keeps re-verifying that key while the mount lives, so revoking it
    /// closes established connections, not just new ones.
    pub fn new_with_creds(
        addr: &str,
        view: &str,
        token: &str,
        uid: u32,
        api_key: &str,
    ) -> io::Result<Self> {
        let this = Self {
            addrs: split_addrs(addr),
            active: AtomicUsize::new(0),
            view: view.to_string(),
            token: token.to_string(),
            api_key: api_key.to_string(),
            opened: std::sync::atomic::AtomicBool::new(false),
            uid,
            pool: Arc::new(Mutex::new(Vec::new())),
            max_conns: 32,
            connect_timeout: Duration::from_secs(5),
            connect_retries: 6,
            reconnect_backoff: Duration::from_millis(150),
            fence: Arc::new(AtomicU64::new(0)),
            readonly: AtomicU8::new(0),
            paged: AtomicU8::new(0),
            content_ro: AtomicU8::new(0),
        };
        // Probe once (connect + handshake) so mount fails fast on a dead server.
        let (s, _) = this.connect_fresh()?;
        this.opened.store(true, Ordering::SeqCst);
        this.put_conn(s);
        Ok(this)
    }

    /// True when the bound view name resolved to a server-side DERIVED view on
    /// the last `M_OPEN` handshake — the mount must be read-only (virtual views
    /// render a shape over the base tree but refuse every mutation; writes go
    /// to the original/physical view). Set from ack byte [0].
    pub fn is_readonly_view(&self) -> bool {
        self.readonly.load(Ordering::SeqCst) != 0
    }

    /// True when the bound view name is a FORK: its metadata tree is the
    /// client's to organize (mkdir/rename/unlink are served), but CONTENT
    /// writes (write/truncate) must be refused with EROFS because the bytes are
    /// shared with the base view. Signalled by `M_OPEN` ack byte [2]; false for
    /// physical/derived views.
    pub fn is_content_ro_view(&self) -> bool {
        self.content_ro.load(Ordering::SeqCst) != 0
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
                        proto::put_key(&mut body, &self.api_key);
                        let mut s = s;
                        match Self::call(&mut s, M_OPEN, &body) {
                            Ok((0, epoch, data)) => {
                                // Cache the authoritative fencing epoch so the
                                // data-plane client stamps writes with the live
                                // token (advances automatically on failover).
                                self.fence.store(epoch, Ordering::SeqCst);
                                // The ack body is exactly the three capability
                                // flags below; a short body means the peer is
                                // not this protocol. Refuse it instead of
                                // guessing a default (a silently "writable"
                                // mount of a read-only view is far worse than
                                // a clear handshake error).
                                let Some(&[ro, paged, content_ro]) = data.get(..3) else {
                                    return Err(io::Error::new(
                                        io::ErrorKind::InvalidData,
                                        "meta-server handshake ack is missing the capability flags — client and server builds must match",
                                    ));
                                };
                                // [0] view is DERIVED (read-only) or the key is
                                //     read-only; [1] the store pages listings
                                //     itself (M_READDIRP + READDIRPLUS); [2] the
                                //     view is a FORK (structure writable, content
                                //     read-only).
                                self.readonly.store(ro, Ordering::SeqCst);
                                self.paged.store(paged, Ordering::SeqCst);
                                self.content_ro.store(content_ro, Ordering::SeqCst);
                                self.active.store(idx, Ordering::Relaxed);
                                return Ok((s, false));
                            }
                            // A rejected credential is a configuration error, not
                            // a transient one — retrying or failing over can't fix
                            // it. The server's payload names the actual cause
                            // (missing key / wrong secret / revoked / view not
                            // admitted), so the operator sees that instead of E_AUTH.
                            Ok((E_AUTH, _, reason)) => {
                                let why = if reason.is_empty() {
                                    "meta-server rejected the access token".to_string()
                                } else {
                                    String::from_utf8_lossy(&reason).into_owned()
                                };
                                // Accepted at setup, refused now: the credential
                                // died mid-session. Ask for a clean unmount rather
                                // than serving a mount that can never succeed.
                                if self.opened.load(Ordering::SeqCst) {
                                    super::signal_auth_loss(&why);
                                }
                                return Err(io::Error::new(io::ErrorKind::PermissionDenied, why));
                            }
                            Ok((_, _, reason)) => {
                                // The server refused the open with a non-auth
                                // status (e.g. the explicit-creation gate saying the view
                                // does not exist, or a resolve failure). It
                                // carries a readable cause in the payload; print
                                // that instead of a bare EIO, which at the FUSE
                                // layer is indistinguishable from a dead disk.
                                // An empty payload (a refusal without a reason)
                                // keeps the pre-existing EIO.
                                last_err = if reason.is_empty() {
                                    io::Error::from_raw_os_error(EIO)
                                } else {
                                    io::Error::other(String::from_utf8_lossy(&reason).into_owned())
                                };
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
        // One metadata RPC end-to-end (checkout + send + recv + any stale-pool
        // reconnect). Pairs with the data-plane io.remote.* RTTs so an operator
        // can separate meta-server round-trip cost from data-server cost.
        let _rpc = stats::start(stats::Stage::RemoteMetaRpc);
        let (mut s, reused) = self.get_conn()?;
        match Self::call(&mut s, op, body) {
            Ok(r) => {
                self.note_refusal(&r);
                self.put_conn(s);
                Ok(r)
            }
            Err(first_err) => {
                drop(s);
                if !reused {
                    return Err(first_err);
                }
                // A failure on a *reused* socket means the peer died between
                // handshake and this op — count it as a reconnect (the HA /
                // restart signal) before re-establishing.
                stats::count(stats::Counter::RemoteReconnect, 1);
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

    /// An `E_AUTH` on a metadata op (as opposed to on the `M_OPEN` handshake,
    /// which [`RemoteMeta::connect_fresh`] handles itself) can only be the
    /// server's live-credential re-check: the alias gate fires at open, and the
    /// per-op read-only-key gate answers `E_NOTPERM`. So it means one
    /// thing — this connection's api key was just revoked — and the mount must
    /// not go on serving a view its credential no longer opens. The op itself
    /// still fails normally for the caller (see [`err_from_code`]).
    fn note_refusal(&self, r: &(i32, u64, Vec<u8>)) {
        if r.0 != E_AUTH || !self.opened.load(Ordering::SeqCst) {
            return;
        }
        let why = if r.2.is_empty() {
            "meta-server revoked this connection's api key".to_string()
        } else {
            String::from_utf8_lossy(&r.2).into_owned()
        };
        super::signal_auth_loss(&why);
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

    fn readdir_page(&self, parent: i64, offset: usize, limit: usize) -> Vec<NodeInfo> {
        // Only page over the wire when the handshake said the server's store can
        // page. Otherwise slice a single full listing (the pre-paging behaviour).
        if self.paged.load(Ordering::SeqCst) == 0 {
            return self
                .readdir(parent)
                .into_iter()
                .skip(offset)
                .take(limit)
                .collect();
        }
        let mut b = Vec::new();
        put_i64(&mut b, parent);
        proto::put_u64(&mut b, offset as u64);
        proto::put_u64(&mut b, limit as u64);
        match self.round_trip(M_READDIRP, &b) {
            Ok((0, _, data)) => nodes_from(&data).unwrap_or_default(),
            // A failed page RPC (a transient transport error) must not
            // truncate the directory: fall back to slicing one full listing.
            // `round_trip` already re-handshakes on a stale socket, so `paged`
            // reflects the live node before the next page call.
            _ => self
                .readdir(parent)
                .into_iter()
                .skip(offset)
                .take(limit)
                .collect(),
        }
    }

    fn supports_paged_readdir(&self) -> bool {
        self.paged.load(Ordering::SeqCst) != 0
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

    fn create_symlink(
        &self,
        parent: i64,
        name: &str,
        target: &str,
        uid: u32,
        gid: u32,
    ) -> Result<i64, MetaError> {
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

    fn rename(
        &self,
        parent: i64,
        name: &str,
        new_parent: i64,
        new_name: &str,
        noreplace: bool,
    ) -> Result<Option<String>, MetaError> {
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

    fn get_lock(
        &self,
        ino: i64,
        owner: u64,
        start: u64,
        end: u64,
        ty: LockType,
    ) -> Option<LockConflict> {
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
    /// Per-caller credential gate for `M_OPEN` and live sessions; `None` keeps
    /// the static-token/LAN-trusted behavior byte-for-byte.
    keys: Option<Arc<crate::apikeys::ApiKeyStore>>,
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
    /// Which metadata backend `engine_for` should open against (see
    /// [`StoreKind`]). `Pg(_)` disables the GroveFS-native HA paths by
    /// construction: `serve()` rejects the `--role standby`/`--peer`/`--witness`
    /// flags for a PG store, and sync-repl / replicator ticks never fire.
    store: StoreKind,
    /// The CONTROL plane this node reads view definitions, aliases and forks
    /// through — the SAME store `grovefs view`/`mcp`/`apikey` write to, so a
    /// mount resolves against one system of record. Under
    /// [`StoreKind::Sqlite`] it is the local `<base>/meta/views/*.layout` file
    /// directory; under [`StoreKind::Pg`] it is the shared `grovefs_control`
    /// schema (definitions live in PG, not on any node's disk). Physical trees
    /// still open through [`store_for`] per [`StoreKind`]; only the
    /// definition/alias/fork/hot-reload-ticket reads are routed here.
    control: Arc<dyn crate::mcp::ControlStore>,
    /// Server-side **derived views**: client-visible name → resolved provider
    /// (the physical `MetaStore` itself, or a [`LayoutMeta`] lens over another
    /// store's tree, defined by the control store's `<name>` definition). The
    /// cache the wire loop dispatches against.
    views: Arc<Mutex<HashMap<String, Arc<dyn MetaProvider>>>>,
    /// Derived view name → physical base view name. Physical views map to
    /// themselves, so subscribers and snapshot pushes can always resolve the
    /// tree a name actually reads. Populated by [`view_provider`].
    bases: Arc<Mutex<HashMap<String, String>>>,
    /// Derived view name → the control store's content stamp (file
    /// `size:mtime`, PG row write-version `xmin`) of the definition the cached
    /// provider was built from. Presence marks a cache entry as DERIVED, which
    /// lets `view_provider` hot-reload it: an edited definition (different
    /// stamp) evicts the cached lens so live mounts converge on the new shape
    /// without a remount. Physical entries are absent here and never re-checked.
    defs: Arc<Mutex<HashMap<String, String>>>,
}

impl MetaRuntime {
    /// Effective role: always Primary for a plain (non-HA) node.
    fn role(&self) -> Role {
        self.ha.as_ref().map(|h| h.role()).unwrap_or(Role::Primary)
    }

    /// Drop every cached engine: physical stores, derived-view providers and
    /// the name→base map. Called whenever the on-disk trees or the role change
    /// underneath the cache (HA promote/demote, witness fence, replicated
    /// snapshot install) so the next op re-opens against fresh state.
    fn evict(&self) {
        self.cache.lock().clear();
        self.views.lock().clear();
        self.bases.lock().clear();
        self.defs.lock().clear();
    }

    /// Physical tree a client-visible view name resolves to. Derived views
    /// return their `base`; plain/physical names return themselves. Only
    /// meaningful after [`view_provider`] has resolved the name once.
    fn physical_base(&self, view: &str) -> String {
        self.bases
            .lock()
            .get(view)
            .cloned()
            .unwrap_or_else(|| view.to_string())
    }

    /// Is `view` a DERIVED (virtual) view name — i.e. served through a
    /// `meta/views/<view>.layout` definition rather than its own physical db?
    /// Such views mount read-only: they are a centrally defined presentation,
    /// every write belongs to the original (base) view. Fresh per op because
    /// the wire loop re-resolves [`view_provider`] (which repopulates `bases`)
    /// before consulting this.
    fn is_derived(&self, view: &str) -> bool {
        self.defs.lock().contains_key(view)
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
        let res = witness_quorum(
            &addrs,
            &self.token,
            mine,
            WITNESS_LEASE,
            WITNESS_PROBE_TIMEOUT,
        );
        if res.fenced(q) {
            ha.set_role(Role::Standby);
            ha.epoch.store(res.prev_max, Ordering::SeqCst);
            ha.lease_clear();
            self.evict();
            stats::count(stats::Counter::HaFencing, 1);
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

/// Central metadata authority. Holds one [`MetaEngine`] per view, opened on
/// demand against an EXISTING `<base>/meta/<view>.db` (a missing one is
/// refused, never created), shared across all connection threads, so
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
    /// Optional per-caller credential gate (`--require-api-key`), backed by the
    /// same `<base>/meta/api_keys.db` `grovefs apikey` / `grovefs mcp` use.
    keys: Option<Arc<crate::apikeys::ApiKeyStore>>,
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
            keys: None,
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
            keys: None,
            role: Role::Primary,
            peer: None,
            sync_repl: false,
            witness: None,
            store: StoreKind::Sqlite,
        }
    }

    /// Additionally demand a `grovefs apikey` credential (scope `mount` or
    /// `any`) on every `M_OPEN`, so mount admission becomes per-caller and
    /// revocable rather than one static cluster secret. Independent of the
    /// token: with both configured, a client must satisfy both. The store is
    /// re-consulted for LIVE connections (`ApiKeyStore::is_live`, once per
    /// `KEY_RECHECK` window per session), so `grovefs apikey revoke` closes an
    /// established mount instead of only refusing the next one.
    pub fn with_api_keys(mut self, store: crate::apikeys::ApiKeyStore) -> Self {
        self.keys = Some(Arc::new(store));
        self
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
        // SQLite trees live under `<base>/meta`; a PG store writes nothing
        // locally, so creating that directory would be a misleading footprint
        // (an empty `meta/` on a host that has no local metadata at all).
        if matches!(self.store, StoreKind::Sqlite) {
            std::fs::create_dir_all(self.base.join("meta"))?;
        }
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
        if self.keys.is_some() {
            // Same honesty line as the data plane: an operator reading the log
            // must know why the next mount got refused.
            println!("meta-server: api-key auth REQUIRED (present `mount --api-key`, minted by `grovefs apikey create --for mount|any`); a revoke closes live mounts on their next op");
        }
        // State the gate explicitly: a new base view must be created first
        // (`view create` / `init` / `view import`), and a typo is refused
        // instead of silently becoming an empty tree.
        println!("meta-server: views are created explicitly — M_OPEN refuses a name with no existing tree or definition (`grovefs view create --view <name>`, or `--home <dir> init --view <name>`); no empty tree is ever auto-created");
        // HA coordination is live only when a peer was configured or we start as
        // a standby; otherwise `ha = None` preserves single-node semantics.
        let ha = if self.peer.is_some() || self.role == Role::Standby {
            Some(HaState::new(self.role))
        } else {
            None
        };
        // The control plane FOLLOWS the metadata backend: a PG store reads view
        // definitions/aliases/forks from the shared `grovefs_control` schema, a
        // SQLite store from the local `meta/views` dir — one system of record per
        // backend, never a local file the PG deployment would ignore.
        let control = crate::mcp::control_store_for(
            &self.base,
            match &self.store {
                StoreKind::Pg(uri) => Some(uri.as_str()),
                StoreKind::Sqlite => None,
            },
        )
        .map_err(io::Error::other)?;
        let rt = Arc::new(MetaRuntime {
            base: self.base.clone(),
            token: self.token.clone(),
            keys: self.keys.clone(),
            cache: Arc::new(Mutex::new(HashMap::new())),
            subs: Arc::new(Mutex::new(HashMap::new())),
            ha: ha.clone(),
            peer: self.peer.clone(),
            forward: Mutex::new(Vec::new()),
            sync_repl: self.sync_repl && self.peer.is_some(),
            standby_push: Mutex::new(()),
            witness: self.witness.clone(),
            store: self.store.clone(),
            control,
            views: Arc::new(Mutex::new(HashMap::new())),
            bases: Arc::new(Mutex::new(HashMap::new())),
            defs: Arc::new(Mutex::new(HashMap::new())),
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
                    let res =
                        witness_quorum(&addrs, &rt.token, mine, WITNESS_LEASE, CONTROL_TIMEOUT);
                    if res.fenced(q) {
                        ha.set_role(Role::Standby);
                        ha.epoch.store(res.prev_max, Ordering::SeqCst);
                        rt.evict();
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
                            rt.evict();
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

/// Accept loop with an access token (`""` disables the check). Single-node
/// (no HA), the shape every test server uses.
pub fn serve_on_meta_auth(listener: TcpListener, base: PathBuf, token: &str) {
    serve_on_meta_full(listener, base, token);
}

fn serve_on_meta_full(listener: TcpListener, base: PathBuf, token: &str) {
    // SQLite-only helper (single-node path): the control plane is the local
    // `meta/views` dir, matching `store: StoreKind::Sqlite`.
    let control =
        crate::mcp::control_store_for(&base, None).expect("file control store is infallible");
    let rt = Arc::new(MetaRuntime {
        base,
        token: token.to_string(),
        keys: None,
        cache: Arc::new(Mutex::new(HashMap::new())),
        subs: Arc::new(Mutex::new(HashMap::new())),
        ha: None,
        peer: None,
        forward: Mutex::new(Vec::new()),
        sync_repl: false,
        standby_push: Mutex::new(()),
        witness: None,
        store: StoreKind::Sqlite,
        control,
        views: Arc::new(Mutex::new(HashMap::new())),
        bases: Arc::new(Mutex::new(HashMap::new())),
        defs: Arc::new(Mutex::new(HashMap::new())),
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

/// Open (or reuse) the authoritative PHYSICAL engine for a base view name.
/// A HA standby opens its replicated copy strictly read-only (so a stray
/// write is refused at the SQLite layer, not merely discouraged); a primary
/// opens read-write. Under [`StoreKind::Pg`] the standby/readonly path never
/// fires (`serve()` refuses the combination), so `readonly` is passed through
/// as `false`. Keyed by the on-disk tree name — derived display views live in
/// [`MetaRuntime::views`] and are resolved by [`view_provider`].
fn store_for(rt: &MetaRuntime, view: &str) -> io::Result<Arc<MetaStore>> {
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

/// Scan the control plane once for PURE ALIAS definitions, returning
/// alias-name → real-view-name. Malformed and non-alias entries are ignored
/// here — derived/physical resolution goes through [`view_provider`], which
/// parses each definition it needs. Called on every M_OPEN: mounts are rare
/// and the definition set is small control-plane config, so a full read is
/// cheaper than maintaining an invalidation protocol for it. The scan hits
/// whichever system of record this node is bound to (local `meta/views` dir
/// under SQLite, `grovefs_control.view_defs` under PG) — never a path-specific
/// file the backend would ignore.
fn load_aliases(store: &dyn crate::mcp::ControlStore) -> io::Result<HashMap<String, String>> {
    store
        .load_alias_map()
        .map_err(|e| io::Error::other(format!("cannot read view definitions: {e}")))
}

/// Does `view` already exist here — as a governed definition (derived / fork /
/// alias) or as a physical tree — WITHOUT creating anything? This is the probe
/// `M_OPEN` gates with, and it must stay non-creating: calling
/// [`view_provider`] first would already have materialized `meta/<view>.db`
/// (or a PG schema) via [`store_for`].
///
/// Backend-agnostic by construction: both questions go to the control store
/// that follows the metadata backend (a `meta/views/*.layout` file / a
/// `<name>.db` under SQLite, a `grovefs_control` definition row /
/// `information_schema.schemata` row under PG).
fn view_exists(rt: &MetaRuntime, view: &str) -> io::Result<bool> {
    if !crate::layout::valid_view_name(view) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("bad view name '{view}'"),
        ));
    }
    let control = &*rt.control;
    if control.has_def(view).map_err(io::Error::other)? {
        return Ok(true);
    }
    control.has_physical(view).map_err(io::Error::other)
}

/// Resolve the provider a client-visible `view` name is served from. Two
/// shapes:
/// - **physical view** (no definition for `view` in the control store): the
///   authoritative store itself, exactly as before — one db, one tree.
/// - **derived view** (a definition is present): `LayoutRules::parse_def`
///   yields `(base, rules)`; the provider is a server-side [`LayoutMeta`] lens
///   over the base tree's store. One copy of bytes and metadata, a different
///   presentation per view name — clients mount `--view <name>` with no local
///   rules file, and the SAME derived name from any machine shows the SAME
///   shape (the lens is one shared server-side instance, so virtual inodes and
///   display paths are consistent across its subscribers).
///
/// Deliberate limits (documented honestly): no nesting — a derived view's
/// `base` must name a physical tree, and a derived name cannot shadow a
/// physical db (checked at define time and here). Derived views are display
/// customization, NOT a security boundary; hard isolation stays view + API-key/token.
/// They are, however, READ-ONLY by contract: the M_OPEN ack flags them so the
/// client mounts kernel read-only, and the per-op gate above refuses every
/// write-intent op for a derived name — mutations belong to the original
/// (physical) view, so a virtual shape can never be half-edited through.
fn view_provider(rt: &MetaRuntime, view: &str) -> io::Result<Arc<dyn MetaProvider>> {
    let control = &*rt.control;
    // Bind the cached clone FIRST: an `if let Some(p) = rt.views.lock()...`
    // would keep the guard alive through the whole block (temporary lifetime
    // rules), and the eviction path below locks `views` again — a non-reentrant
    // mutex would self-deadlock the connection.
    let cached = rt.views.lock().get(view).cloned();
    if let Some(p) = cached {
        // Hot reload: a cached DERIVED entry is only served while its
        // definition is unchanged in the control store (a file `size:mtime` or
        // a PG row `xmin` ticket; a cheap probe per op is negligible next to a
        // FUSE round trip). An edited definition evicts the lens and every live
        // mount of the name converges on the next op — bounded by the client's
        // attr/dentry TTL (≤2s), no remount needed. A *removed* definition
        // intentionally does NOT yank the shape out from under established
        // mounts: they keep the lens they opened with (names only change meaning
        // at M_OPEN for deletes). Physical entries (no `defs` row) are never
        // re-checked — a definition appearing out-of-band for an already-
        // resolved physical name would ambiguously shadow its db, so it is
        // deliberately ignored.
        let stale = rt
            .defs
            .lock()
            .get(view)
            .map_or(false, |seen| control.defs_changed(view, seen));
        if !stale {
            return Ok(p);
        }
        rt.views.lock().remove(view);
        rt.defs.lock().remove(view);
    }
    if !crate::layout::valid_view_name(view) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "bad view name"));
    }
    let def_text = control.get_def_raw(view).map_err(io::Error::other)?;
    let def = def_text
        .as_deref()
        .map(LayoutRules::parse_def)
        .transpose()
        .map_err(io::Error::other)?;
    let (provider, base): (Arc<dyn MetaProvider>, String) = match def {
        Some(ViewDef::Fork(_)) => {
            // A FORK owns an independent metadata tree (a local `<name>.db`
            // under SQLite; PG has no fork backend and refuses creation, so this
            // branch only runs on the file store). The definition only records
            // which physical view seeded it. It is resolved like a physical
            // store (same cache, same subscriptions) but deliberately NOT
            // recorded in `defs`: `is_derived` stays false, so structure writes
            // are admitted while the client-side ack flag (below) keeps content
            // writes out.
            if !control.has_physical(view).map_err(io::Error::other)? {
                return Err(io::Error::other(format!(
                    "fork view '{view}' has no tree in {} — removed out-of-band?",
                    control.describe()
                )));
            }
            let store = store_for(rt, view)?;
            (
                Arc::clone(&store) as Arc<dyn MetaProvider>,
                view.to_string(),
            )
        }
        Some(ViewDef::Alias(_)) => {
            return Err(io::Error::other(
                "alias definitions are resolved before view_provider and cannot be served as a tree",
            ));
        }
        Some(ViewDef::Derived(b, rules)) => {
            if b == view {
                return Err(io::Error::other("view definition base equals its own name"));
            }
            // The base must be a physical tree: no lens over a lens, which would
            // also make eviction/subscription resolution ambiguous.
            if control.has_def(&b).map_err(io::Error::other)? {
                return Err(io::Error::other(
                    "derived view cannot build on another derived view",
                ));
            }
            // Never create the base as a side effect of mounting the lens: a
            // published definition whose base was never created is a
            // provisioning mistake, and `store_for` below would otherwise leave
            // an empty `meta/<base>.db` (or PG schema) behind.
            if !control.has_physical(&b).map_err(io::Error::other)? {
                return Err(io::Error::other(format!(
                    "derived view '{view}': its base '{b}' has no physical tree in {} — create the base first (`grovefs view create --view {b}`), then mount `{view}` again",
                    control.describe()
                )));
            }
            if control.has_physical(view).map_err(io::Error::other)? {
                // a physical tree named `view` exists — the name would be ambiguous
                return Err(io::Error::other("view name shadows a physical db"));
            }
            let store = store_for(rt, &b)?;
            // Record the exact content stamp the lens was built from AFTER
            // parsing, so a concurrent edit shows up as stale on the next check
            // (eventually convergent, never serves a shape newer than its
            // recorded stamp).
            if let Some(stamp) = control.stamp(view) {
                rt.defs.lock().insert(view.to_string(), stamp);
            }
            (Arc::new(LayoutMeta::new(store, rules)), b)
        }
        None => {
            let store = store_for(rt, view)?;
            (
                Arc::clone(&store) as Arc<dyn MetaProvider>,
                view.to_string(),
            )
        }
    };
    rt.bases.lock().insert(view.to_string(), base);
    rt.views.lock().insert(view.to_string(), provider.clone());
    Ok(provider)
}

/// Does `view` carry a `fork <base>` definition? Read per `M_OPEN` from the
/// control store (mounts are rare and the definition set is small
/// control-plane config). A fork's own metadata tree is writable, but its
/// CONTENT is not — the bytes are shared with the base view through the
/// cross-view ledger, so uploading belongs to a base view. The server cannot
/// gate bytes (they bypass the meta plane), so it tells the client via ack byte
/// [2] and the FUSE layer refuses `write` / truncate with EROFS. Honest
/// boundary: a raw protocol client that skips the FUSE layer can still write to
/// the data plane. (PG has no fork backend, so no fork definition can exist
/// there and this always resolves false.)
fn is_fork_def(store: &dyn crate::mcp::ControlStore, view: &str) -> io::Result<bool> {
    Ok(matches!(
        store.get_def(view).map_err(io::Error::other)?,
        Some(ViewDef::Fork(_))
    ))
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
/// Each sink is a per-subscriber `Arc<Mutex<TcpStream>>` shared across every
/// bucket it registered under (its own view + physical base), so concurrent
/// broadcasts serialize only against the SAME socket (preserving frame
/// atomicity), never against the whole registry.
type Subscribers = Arc<Mutex<HashMap<String, HashMap<u64, Arc<Mutex<TcpStream>>>>>>;

/// Monotonic id handed to each subscriber connection.
static NEXT_SUB: AtomicU64 = AtomicU64::new(1);

/// Upper bound on how long a push to one subscriber may block the broadcasting
/// op thread. A client that stops draining its subscribe socket is dropped
/// (pruned) after this rather than stalling event delivery — it then converges
/// via its attribute TTL. Bounded so a single laggy peer can't wedge a writer.
const BROADCAST_SEND_TIMEOUT: Duration = Duration::from_secs(2);

/// Push `events` to every subscriber of `view`.
///
/// The registry lock is held ONLY to snapshot the current sinks (cheap map
/// clone of `Arc` handles); the actual socket writes happen outside it, each
/// under that subscriber's own mutex. So a slow or wedged subscriber can no
/// longer block broadcasts to other views, new subscribe/unsubscribe, or the
/// whole event bus — the failure mode the previous single global lock-across-
/// send created. Dead/timed-out sinks are pruned opportunistically.
fn broadcast(subs: &Subscribers, view: &str, events: &[Event]) {
    if events.is_empty() {
        return;
    }
    let sinks: Vec<(u64, Arc<Mutex<TcpStream>>)> = {
        let reg = subs.lock();
        match reg.get(view) {
            Some(map) => map.iter().map(|(id, s)| (*id, Arc::clone(s))).collect(),
            None => return,
        }
    };
    let mut dead: Vec<u64> = Vec::new();
    for (id, sink) in sinks {
        let mut s = sink.lock();
        for ev in events {
            if send_event(&mut s, ev).is_err() {
                // A sink that can't be drained (write-timeout on a subscriber
                // that stopped reading, or a dropped socket) is pruned; count it
                // so a slow/leaky subscriber shows up as a growing signal.
                stats::count(stats::Counter::BroadcastTimeout, 1);
                dead.push(id);
                break;
            }
        }
    }
    if !dead.is_empty() {
        let mut reg = subs.lock();
        if let Some(map) = reg.get_mut(view) {
            for id in dead {
                map.remove(&id);
            }
        }
    }
}

/// Enrich source-coordinate entry events into display-coordinate events for
/// every derived view that uses `phys` as its base. A derived subscriber
/// registers under BOTH its own name and the base, so the existing broadcast
/// to `phys` hits it; however the event's `(parent, name)` is a SOURCE
/// coordinate. For a relocated entry rendered under a SYNTHESIZED dir the
/// source coordinate addresses no dentry, leaving a ≤2s stale window. This
/// helper calls `display_targets` on each derived lens and pushes the extra
/// display-coordinate events (Entry + Inode) into the derived bucket.
fn broadcast_derived_display(rt: &MetaRuntime, phys: &str, events: &[Event]) {
    let derived: Vec<String> = rt
        .bases
        .lock()
        .iter()
        .filter(|(_, b)| b.as_str() == phys)
        .map(|(n, _)| n.clone())
        .filter(|n| n.as_str() != phys)
        .collect();
    for dname in derived {
        // Only enrich if this bucket has actual subscribers.
        let has_subs = rt.subs.lock().get(&dname).map_or(false, |m| !m.is_empty());
        if !has_subs {
            continue;
        }
        let prov = rt.views.lock().get(&dname).cloned();
        let Some(prov) = prov else { continue };
        let mut extra: Vec<Event> = Vec::new();
        for ev in events {
            if let Event::Entry { parent, name } = ev {
                for (p, dn) in prov.display_targets(*parent, name) {
                    extra.push(Event::Entry {
                        parent: p,
                        name: dn,
                    });
                    // Flush the display parent's cached listing (synthesized
                    // dirs have fixed mtime → pages never self-invalidate).
                    extra.push(Event::Inode { ino: p });
                    prov.flush_listing(p);
                }
                // This very server renders the lens for its derived
                // subscribers, materializing each shaped listing (see
                // `LayoutMeta::listing`). The mutation reached the kernel
                // caches above, but nothing told the provider it renders with,
                // so without this the subscriber's next READDIR would be served
                // the pre-event rows straight out of the lens's own cache.
                prov.flush_listing(*parent);
            }
        }
        broadcast(&rt.subs, &dname, &extra);
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
fn inode_change_events(engine: &dyn MetaProvider, ino: i64) -> Vec<Event> {
    let mut evs = vec![Event::Inode { ino }];
    if let Some(n) = engine.getattr(ino) {
        if !n.name.is_empty() {
            evs.push(Event::Entry {
                parent: n.parent_id,
                name: n.name,
            });
        }
    }
    evs
}

/// Serve a subscriber connection: register its write half under every bucket
/// name (`view` itself plus, for derived views, its physical base), ack, then
/// block on reads until the client disconnects and unregister from all of
/// them. Other threads deliver events to the registered handle via [`broadcast`].
fn serve_subscriber(
    mut reader: BufReader<TcpStream>,
    mut writer: TcpStream,
    buckets: &[String],
    subs: &Subscribers,
) -> io::Result<()> {
    let id = NEXT_SUB.fetch_add(1, Ordering::Relaxed);
    let handle = writer.try_clone()?;
    // Bound the push so a subscriber that stops draining can't wedge the
    // broadcasting thread (see BROADCAST_SEND_TIMEOUT); a past-timeout send
    // errors and the sink is pruned by broadcast().
    handle.set_write_timeout(Some(BROADCAST_SEND_TIMEOUT))?;
    // One shared per-subscriber sink across every bucket it registers under, so
    // concurrent broadcasts to THIS socket serialize (frame atomicity) while
    // other views' delivery stays independent of the global registry lock.
    let sink = Arc::new(Mutex::new(handle));
    for b in buckets {
        subs.lock()
            .entry(b.clone())
            .or_default()
            .insert(id, Arc::clone(&sink));
    }
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
    for b in buckets {
        if let Some(map) = subs.lock().get_mut(b) {
            map.remove(&id);
        }
    }
    result
}

fn handle_meta_conn(stream: TcpStream, rt: Arc<MetaRuntime>) -> io::Result<()> {
    stream.set_nodelay(true)?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;

    // First frame distinguishes a client from a peer's replication control
    // connection: clients always open with `M_OPEN { view, token, uid, api_key }`;
    // the peer opens directly with `M_PING`/`M_PUSH` (never bound to a view).
    let (op, body) = proto::recv_request(&mut reader)?;
    if op == M_PING || op == M_PUSH {
        return handle_control_conn(&mut reader, &mut writer, rt, op, body);
    }
    if op != M_OPEN {
        proto::send_response(&mut writer, E_DB, 0, &[])?;
        return Ok(());
    }
    let mut i = 0usize;
    let mut view = proto::get_key(&body, &mut i).unwrap_or_default();
    let presented = proto::get_key(&body, &mut i).unwrap_or_default();
    // Optional presenting uid appended by the client handshake (older clients
    // omit it -> uid 0, recorded as such in the call audit).
    let uid = proto::read_u32(&body, &mut i).unwrap_or(0);
    // Optional api key appended by newer clients (an old client omits it, so it
    // only fails when this server actually demands one).
    let api_key = proto::get_key(&body, &mut i).unwrap_or_default();
    // The name the client mounted, kept alongside the post-alias real name so a
    // view-bound key can be checked against either (bind to the tree or to its
    // public alias).
    let presented_name = view.clone();
    // Enforce the shared secret when one is configured; an empty server token
    // means no auth at all (trusted LAN).
    if !rt.token.is_empty() && presented != rt.token {
        proto::send_response(
            &mut writer,
            E_AUTH,
            0,
            b"meta-server rejected the access token",
        )?;
        return Ok(());
    }
    // Per-caller credential gate (`--require-api-key`), checked right after the
    // cluster token and BEFORE any view/alias resolution: it decides WHO may
    // connect, never WHICH view. A refusal carries its cause in the payload so
    // the client prints the reason (missing key / wrong secret / revoked /
    // wrong scope) instead of a bare E_AUTH.
    let mut authed: Option<crate::apikeys::AuthedKey> = None;
    if let Some(store) = &rt.keys {
        match store.authenticate_mount(&api_key) {
            Ok(a) => {
                let _ = store.log_call(
                    Some(&a),
                    "meta",
                    "M_OPEN",
                    true,
                    &format!("view='{}' uid={}", view, uid),
                );
                authed = Some(a);
            }
            Err(e) => {
                let _ = store.log_call(
                    None,
                    "meta",
                    "M_OPEN",
                    false,
                    &format!("view='{view}': {e}"),
                );
                proto::send_response(&mut writer, E_AUTH, 0, e.as_bytes())?;
                return Ok(());
            }
        }
    }
    // PURE ALIAS resolution (name governance): an `alias <real>` definition
    // in meta/views/ makes the alias the public mount surface for a physical
    // view. The swap happens BEFORE the view-binding check and every downstream
    // engine/bucket decision, so the alias simply IS the real view — a key bound
    // to the real name keeps working through it, and the full
    // tree stays writable (no rules, so no read-only flag). Conversely, once
    // a real name is governed by ANY alias, mounting it directly is refused:
    // the mapping is the entry point — which is the whole point, letting an
    // operator rotate public names freely while the storage identity stays
    // unexposed. Rescanned per M_OPEN (a small control-plane read; mount
    // establishment is rare); established connections are untouched — deleting
    // an alias reopens the real name only for new mounts.
    let aliases = load_aliases(&*rt.control)?;
    if let Some(real) = aliases.get(&view) {
        view = real.clone();
    } else if aliases.values().any(|r| r == &view) {
        proto::send_response(
            &mut writer,
            E_AUTH,
            0,
            format!(
                "view '{view}' is governed by an alias — mount the alias name instead (`grovefs view list` shows it)"
            )
            .as_bytes(),
        )?;
        return Ok(());
    }
    // View binding (`apikey create --view NAME`): a bound key may only open the
    // physical view it names — or an alias that resolves to it, and equally the
    // alias name itself, so an operator can bind to whichever name is the mount
    // surface. Checked AFTER alias resolution so it holds against the real tree
    // (grants written for the physical name keep working through a rotated
    // alias). An unbound key (`view: None`, a system-level credential) admits any
    // view and skips this entirely. Refusal is E_AUTH (it is an access-decision
    // failure, not a data error).
    if let Some(a) = &authed {
        if let Some(bound) = &a.view {
            if bound != &view && bound != &presented_name {
                if let Some(store) = &rt.keys {
                    let _ = store.log_call(
                        authed.as_ref(),
                        "meta",
                        "M_OPEN",
                        false,
                        &format!("view='{view}': key bound to view '{bound}'"),
                    );
                }
                proto::send_response(
                    &mut writer,
                    E_AUTH,
                    0,
                    format!(
                        "permission denied: this api key is bound to view '{bound}', not '{view}'"
                    )
                    .as_bytes(),
                )?;
                return Ok(());
            }
        }
    }
    // Explicit-creation gate: refuse a name that does not ALREADY exist. This
    // must run BEFORE `view_provider`, because that call is what materializes
    // the tree: by the time it fails (or succeeds with an empty tree),
    // `meta/<view>.db` (or a PG schema) is already on disk — the one side
    // effect this gate exists to prevent. The probe is non-creating and
    // backend-agnostic (`has_def` / `has_physical` on the control store).
    //
    // Placement after the binding check is deliberate: an out-of-scope key is
    // still reported as an access decision first (`E_AUTH`, with the reason),
    // and only a caller entitled to this view learns whether it exists. The
    // refusal is `E_DB` (a data-plane fact, not a credential one) WITH a
    // readable payload, which the client prints instead of a bare EIO; it is
    // also recorded in the API-key audit log when a key store is configured.
    let why = match view_exists(&rt, &view) {
        Ok(true) => String::new(),
        Ok(false) => {
            let hint = if rt.control.backend() == "postgres" {
                "a PG base view is a schema: create it first with `grovefs view --store postgres://… --db-path <db-path> create --view <name>`, or restore the schema from a dump"
            } else {
                "create it first: `grovefs view --db-path <db-path> create --view <name>` (or `grovefs --home <db-path> init --view <name>`, or seed it with `grovefs view import`)"
            };
            // `view` is the POST-ALIAS real name; name the alias the caller
            // actually mounted too, so "pub does not exist" never reads as
            // if the alias itself were unknown.
            let backend = rt.control.backend();
            let where_ = if presented_name != view {
                format!("{backend}; mounted as alias '{presented_name}' -> '{view}'")
            } else {
                backend.to_string()
            };
            format!(
                "view '{view}' does not exist on this meta-server ({where_}): no tree and no definition — views are created explicitly, so unknown names are refused instead of becoming an empty tree; {hint}"
            )
        }
        Err(e) => format!("view '{view}': cannot verify existence: {e}"),
    };
    if !why.is_empty() {
        eprintln!("meta: M_OPEN view='{view}' refused: {why}");
        if let Some(store) = &rt.keys {
            let _ = store.log_call(
                authed.as_ref(),
                "meta",
                "M_OPEN",
                false,
                &format!("view='{view}': {why}"),
            );
        }
        proto::send_response(&mut writer, E_DB, 0, why.as_bytes())?;
        return Ok(());
    }
    // Validate the view resolves (physical db or a parseable derived-view
    // definition) before acking. We deliberately do NOT hold this provider for
    // the whole connection: under HA a fresh replicated snapshot evicts the
    // cached engine, so live connections must re-resolve it per op to observe
    // the swap (see the loop below). We only keep the paging capability it
    // reports (a stable property of the store kind — SQLite/PG page, a derived
    // lens does not) as one ack byte, dropping the provider Arc immediately.
    let paged_cap = match view_provider(&rt, &view) {
        Ok(e) => {
            let c = e.supports_paged_readdir() as u8;
            drop(e);
            c
        }
        Err(e) => {
            eprintln!("meta: M_OPEN view='{view}' resolve failed: {e}");
            // Carry the cause so the client can print it (a bare E_DB looks
            // like EIO at the FUSE layer). An empty payload would be the old
            // behavior; there is no reason to hide a server-side reason from
            // the operator who just tried to mount.
            let why = format!("view '{view}' cannot be opened on this meta-server: {e}");
            proto::send_response(&mut writer, E_DB, 0, why.as_bytes())?;
            return Ok(());
        }
    };
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
    // The ack body carries three flag bytes:
    //   [0] nonzero when `view` is a DERIVED (virtual) view OR the presenting
    //       key is read-only (`apikey create --ro`) — the client mounts
    //       read-only so writes fail locally with EROFS instead of round-tripping
    //       to a gate (the per-op gate below stays authoritative regardless);
    //   [1] nonzero when the server pages directory listings in its own store,
    //       unlocking the small M_READDIRP RPC + READDIRPLUS on the client;
    //   [2] nonzero when `view` is a FORK: the metadata tree is writable but
    //       CONTENT writes are refused client-side with EROFS (bytes are shared
    //       with the base view).
    let client_ro = rt.is_derived(&view) || authed.as_ref().map(|a| a.ro).unwrap_or(false);
    let content_ro = is_fork_def(&*rt.control, &view)?;
    let ack_flags = [client_ro as u8, paged_cap, content_ro as u8];
    proto::send_response(&mut writer, 0, open_epoch, &ack_flags)?;

    // Distinguish a subscriber (becomes an event sink, sends no requests) from a
    // work connection (metadata ops) by peeking the next frame.
    let (op, body) = proto::recv_request(&mut reader)?;
    if op == M_SUBSCRIBE {
        // A derived-view subscriber registers BOTH under its own name (to get
        // display-coordinate events from lens writes) and under its physical
        // base (to see plain writes other clients make to the same tree; those
        // events may name coordinates the lens does not show — a no-op miss at
        // the kernel, never a stale hit).
        let base = rt.physical_base(&view);
        let mut buckets = vec![view.clone()];
        if base != view {
            buckets.push(base);
        }
        return serve_subscriber(reader, writer, &buckets, &rt.subs);
    }
    let mut pending = Some((op, body));

    // Live-credential re-check (`--require-api-key`): at most one store lookup
    // per `KEY_RECHECK` window for this session, so a revoke reaches an
    // ESTABLISHED mount, bounded by the re-check window rather than the next
    // handshake. The same tick refreshes the key's CURRENT `ro` flag, so
    // `apikey set-ro` hot-swaps this mount's write verdict without a reconnect.
    let mut next_key_check = Instant::now() + crate::apikeys::KEY_RECHECK;
    // A read-only key (`apikey create --ro`) is refused every write-intent op
    // here, server-side, so "read-only" holds even against a client that mounted
    // without `-o ro` (the ack flag above already nudges honest clients to a
    // read-only mount, but this gate is the authoritative one). Mutable
    // because the re-check tick above re-reads it from the store.
    let mut key_ro = authed.as_ref().map(|a| a.ro).unwrap_or(false);

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
        if let (Some(store), Some(a)) = (&rt.keys, &authed) {
            if Instant::now() >= next_key_check {
                let live = store.live_state(&a.id, crate::apikeys::Scope::Mount);
                match live {
                    None => {
                        let why = "authentication failed: this api key has been revoked";
                        let _ = store.log_call(
                            Some(a),
                            "meta",
                            "M_OPEN",
                            false,
                            &format!("view='{view}': {why}"),
                        );
                        let _ = proto::send_response(&mut writer, E_AUTH, 0, why.as_bytes());
                        return Ok(());
                    }
                    // Still live: adopt the CURRENT ro flag (an `apikey set-ro`
                    // lands here, on the same ≤1s tick that carries a revoke).
                    Some(state) => key_ro = state.ro,
                }
                next_key_check = Instant::now() + crate::apikeys::KEY_RECHECK;
            }
        }
        // Read-only credential: refuse a write-intent op from an `--ro` key.
        // Uses E_NOTPERM (not E_ROFS) so the client's `err_from_code` maps to
        // `MetaError::OpNotPermitted` → `EPERM` at the FUSE reply, distinct
        // from an HA-standby's E_ROFS failover signal.
        if is_write_op(op) && key_ro {
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
        // Re-resolve the provider each op: cheap while cached, and picks up a
        // freshly installed snapshot after a replication push evicted the old one.
        let engine = match view_provider(&rt, &view) {
            Ok(e) => e,
            Err(_) => {
                proto::send_response(&mut writer, E_DB, 0, &[])?;
                continue;
            }
        };
        // Derived-view read-only gate: a virtual view is a server-defined
        // PRESENTATION over the base tree — writes belong to the original
        // (physical) view only. Evaluated after the per-op re-resolve so the
        // name→base map is fresh (a `view define`/`rm` in flight converges
        // within this same ≤TTL window as the shape itself). Current clients
        // never reach it (the mount is kernel read-only from the handshake
        // flag); it stands as the authoritative gate for old or rogue clients.
        // Deliberately E_NOTPERM (→ EPERM at FUSE), never E_ROFS, which the
        // client reads as an HA failover signal.
        if is_write_op(op) && rt.is_derived(&view) {
            proto::send_response(&mut writer, E_NOTPERM, 0, &[])?;
            continue;
        }
        let (status, val, data, events) = dispatch_meta(&*engine, op, &body);
        // Synchronous replication (L2): commit the op to the standby BEFORE
        // acking the client. If the push/ACK window is exceeded we fall through
        // and emit a warn so operators see the degraded RPO honestly. Non-HA
        // or standby roles skip this entirely (their `rt.sync_repl` is false).
        if rt.sync_repl && status == 0 && is_write_op(op) && rt.role() == Role::Primary {
            push_snapshot_now(&rt, &view);
        }
        proto::send_response(&mut writer, status, val, &data)?;
        if status == 0 && !events.is_empty() {
            broadcast(&rt.subs, &view, &events);
            // A lens write changed the PHYSICAL tree too; plain subscribers of
            // the base view converge on the same push (coordinate mismatches
            // in relocated spots simply miss, which is harmless).
            let base = rt.physical_base(&view);
            if base != view {
                broadcast(&rt.subs, &base, &events);
            }
            // Push display-coordinate invalidations to derived subscribers
            // whose lens renders relocated entries under synthesized dirs.
            broadcast_derived_display(&rt, &base, &events);
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
                        proto::send_response(
                            writer,
                            0,
                            ha.epoch.load(Ordering::SeqCst) as u64,
                            &[],
                        )?;
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
                if ha.role() == Role::Standby && install_snapshot(&rt, &view, &snap).is_ok() {
                    proto::send_response(writer, 0, 0, &[])?;
                } else {
                    proto::send_response(
                        writer,
                        E_EPOCH,
                        ha.epoch.load(Ordering::SeqCst) as u64,
                        &[],
                    )?;
                }
            }
            M_FORWARD => {
                // Relay from the primary: deliver to this node's own subscribers
                // so standby-mounted clients converge (batched, <= one tick).
                let mut i = 0usize;
                let view = proto::get_key(&body, &mut i).unwrap_or_default();
                if let Ok(evs) = events_from(&body, &mut i) {
                    broadcast(&rt.subs, &view, &evs);
                    broadcast_derived_display(&rt, &view, &evs);
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
/// drop every cached engine so the next read opens the fresh copy (derived
/// views built on any base are re-resolved too — a snapshot swap is rare and
/// a full evict is the cheap, correct hammer). The snapshot is
/// a complete `VACUUM INTO` image (self-contained, rollback-journal), so the old
/// WAL/shm side files are removed rather than left to shadow it. `view` is
/// validated to keep the peer from steering writes outside the meta dir.
fn install_snapshot(rt: &MetaRuntime, view: &str, snap: &[u8]) -> io::Result<()> {
    if view.is_empty() || view.contains('/') || view.contains('\\') || view.contains("..") {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "bad view name"));
    }
    let base = &rt.base;
    let dir = base.join("meta");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{view}.db"));
    let tmp = dir.join(format!("{view}.db.push.tmp"));
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(snap)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, &path)?; // same-fs rename is atomic
    rt.evict(); // force a reopen of the fresh copy (and any derived lens) on next access
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
fn push_snapshot_now(rt: &MetaRuntime, view: &str) {
    let peer = match &rt.peer {
        Some(p) => p.clone(),
        None => return,
    };
    let ha = match &rt.ha {
        Some(h) => h.clone(),
        None => return,
    };
    // A mutating op on a DERIVED view must ship the snapshot of the PHYSICAL
    // tree it reads — and be named as such, or the standby would install a db
    // file under the derived label.
    let base = rt.physical_base(view);
    let snap =
        match store_for(rt, &base).and_then(|s| s.backup_to_bytes().map_err(io::Error::other)) {
            Ok(s) => s,
            Err(_) => return,
        };
    // Serialize pushes: the standby must see a coherent snapshot sequence and
    // the control protocol is a strict request/response stream.
    let _g = rt.standby_push.lock();
    let mut conn = match TcpStream::connect(&peer) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "meta sync-repl: peer {peer} unreachable ({e}); degraded to async for this op"
            );
            return;
        }
    };
    let _ = conn.set_read_timeout(Some(SYNC_REPL_ACK_TIMEOUT));
    let _ = conn.set_write_timeout(Some(SYNC_REPL_ACK_TIMEOUT));
    let mut body = Vec::with_capacity(base.len() + 2 + snap.len());
    proto::put_key(&mut body, &base);
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
            ha.epoch.store(
                (peer_epoch as i64).max(ha.epoch.load(Ordering::SeqCst)),
                Ordering::SeqCst,
            );
            rt.evict();
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
    let sock = addr.to_socket_addrs()?.next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("arbiter {addr} did not resolve"),
        )
    })?;
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
fn witness_quorum(
    addrs: &[String],
    token: &str,
    epoch: i64,
    ttl: Duration,
    timeout: Duration,
) -> Quorum {
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
                    rt.evict();
                    *ha.last_peer.lock() = Instant::now();
                    stats::count(stats::Counter::HaFencing, 1);
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
                        rt.evict();
                        *ha.last_peer.lock() = now;
                        stats::count(stats::Counter::HaQuorumRefuse, 1);
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
                ha.epoch.store(
                    (peer_epoch as i64).max(ha.epoch.load(Ordering::SeqCst)),
                    Ordering::SeqCst,
                );
                rt.evict(); // reopen views read-only for serving
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
                    let engine = match store_for(&rt, &view) {
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
                ha.epoch
                    .store(res.prev_max.max(candidate), Ordering::SeqCst);
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
        rt.evict(); // reopen views read-write on next access
        for view in list_users(&rt.base) {
            if let Ok(eng) = store_for(&rt, &view) {
                let _ = eng.set_epoch(new_epoch);
            }
        }
        *ha.last_peer.lock() = Instant::now(); // avoid re-firing every tick
        eprintln!("meta HA: promoted to primary at epoch {new_epoch}");
    });
}

/// Execute one metadata operation against the view's engine.
fn dispatch_meta(
    engine: &dyn MetaProvider,
    op: u8,
    body: &[u8],
) -> (i32, u64, Vec<u8>, Vec<Event>) {
    let mut i = 0usize;
    macro_rules! bail {
        ($e:expr) => {
            (err_code(&$e), 0, Vec::new(), Vec::new())
        };
    }
    match op {
        M_LOOKUP => {
            let parent = match get_i64(body, &mut i) {
                Ok(v) => v,
                Err(_) => return (E_DB, 0, Vec::new(), Vec::new()),
            };
            let name = match proto::get_key(body, &mut i) {
                Ok(v) => v,
                Err(_) => return (E_DB, 0, Vec::new(), Vec::new()),
            };
            match engine.lookup(parent, &name) {
                Some(n) => {
                    let mut d = Vec::new();
                    put_node(&mut d, &n);
                    (0, 0, d, Vec::new())
                }
                None => (E_NOTFOUND, 0, Vec::new(), Vec::new()),
            }
        }
        M_GETATTR => {
            let ino = match get_i64(body, &mut i) {
                Ok(v) => v,
                Err(_) => return (E_DB, 0, Vec::new(), Vec::new()),
            };
            match engine.getattr(ino) {
                Some(n) => {
                    let mut d = Vec::new();
                    put_node(&mut d, &n);
                    (0, 0, d, Vec::new())
                }
                None => (E_NOTFOUND, 0, Vec::new(), Vec::new()),
            }
        }
        M_READDIR => {
            let parent = match get_i64(body, &mut i) {
                Ok(v) => v,
                Err(_) => return (E_DB, 0, Vec::new(), Vec::new()),
            };
            let nodes = engine.readdir(parent);
            let mut d = Vec::new();
            put_nodes(&mut d, &nodes);
            (0, 0, d, Vec::new())
        }
        M_READDIRP => {
            let parent = match get_i64(body, &mut i) {
                Ok(v) => v,
                Err(_) => return (E_DB, 0, Vec::new(), Vec::new()),
            };
            let offset = match proto::read_u64(body, &mut i) {
                Ok(v) => v,
                Err(_) => return (E_DB, 0, Vec::new(), Vec::new()),
            };
            let limit = match proto::read_u64(body, &mut i) {
                Ok(v) => v,
                Err(_) => return (E_DB, 0, Vec::new(), Vec::new()),
            };
            // Page inside the store (LIMIT/OFFSET) rather than shipping the whole
            // directory per kernel buffer. Reached only for a server whose store
            // advertises paging (the M_READDIRP cap), so `engine` is a physical
            // MetaStore, never a non-paging derived lens.
            let nodes = engine.readdir_page(parent, offset as usize, limit as usize);
            let mut d = Vec::new();
            put_nodes(&mut d, &nodes);
            (0, 0, d, Vec::new())
        }
        M_CREATE => {
            let parent = match get_i64(body, &mut i) {
                Ok(v) => v,
                Err(_) => return (E_DB, 0, Vec::new(), Vec::new()),
            };
            let is_dir = get_u8(body, &mut i).unwrap_or(0) != 0;
            let data_ref = match get_opt(body, &mut i) {
                Ok(v) => v,
                Err(_) => return (E_DB, 0, Vec::new(), Vec::new()),
            };
            let mode = proto::read_u32(body, &mut i).unwrap_or(0);
            let uid = proto::read_u32(body, &mut i).unwrap_or(0);
            let gid = proto::read_u32(body, &mut i).unwrap_or(0);
            let name = match proto::get_key(body, &mut i) {
                Ok(v) => v,
                Err(_) => return (E_DB, 0, Vec::new(), Vec::new()),
            };
            match engine.create_node(parent, &name, is_dir, data_ref.as_deref(), mode, uid, gid) {
                Ok(id) => (
                    0,
                    id as u64,
                    Vec::new(),
                    vec![Event::Entry { parent, name }],
                ),
                Err(e) => bail!(e),
            }
        }
        M_SYMLINK => {
            let parent = match get_i64(body, &mut i) {
                Ok(v) => v,
                Err(_) => return (E_DB, 0, Vec::new(), Vec::new()),
            };
            let target = match proto::get_key(body, &mut i) {
                Ok(v) => v,
                Err(_) => return (E_DB, 0, Vec::new(), Vec::new()),
            };
            let uid = proto::read_u32(body, &mut i).unwrap_or(0);
            let gid = proto::read_u32(body, &mut i).unwrap_or(0);
            let name = match proto::get_key(body, &mut i) {
                Ok(v) => v,
                Err(_) => return (E_DB, 0, Vec::new(), Vec::new()),
            };
            match engine.create_symlink(parent, &name, &target, uid, gid) {
                Ok(id) => (
                    0,
                    id as u64,
                    Vec::new(),
                    vec![Event::Entry { parent, name }],
                ),
                Err(e) => bail!(e),
            }
        }
        M_LINK => {
            let src_ino = match get_i64(body, &mut i) {
                Ok(v) => v,
                Err(_) => return (E_DB, 0, Vec::new(), Vec::new()),
            };
            let new_parent = match get_i64(body, &mut i) {
                Ok(v) => v,
                Err(_) => return (E_DB, 0, Vec::new(), Vec::new()),
            };
            let new_name = match proto::get_key(body, &mut i) {
                Ok(v) => v,
                Err(_) => return (E_DB, 0, Vec::new(), Vec::new()),
            };
            match engine.link_node(src_ino, new_parent, &new_name) {
                Ok(id) => (
                    0,
                    id as u64,
                    Vec::new(),
                    vec![
                        Event::Entry {
                            parent: new_parent,
                            name: new_name,
                        },
                        Event::Inode { ino: src_ino },
                    ],
                ),
                Err(e) => bail!(e),
            }
        }
        M_UNLINK => {
            let parent = match get_i64(body, &mut i) {
                Ok(v) => v,
                Err(_) => return (E_DB, 0, Vec::new(), Vec::new()),
            };
            let name = match proto::get_key(body, &mut i) {
                Ok(v) => v,
                Err(_) => return (E_DB, 0, Vec::new(), Vec::new()),
            };
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
            let parent = match get_i64(body, &mut i) {
                Ok(v) => v,
                Err(_) => return (E_DB, 0, Vec::new(), Vec::new()),
            };
            let name = match proto::get_key(body, &mut i) {
                Ok(v) => v,
                Err(_) => return (E_DB, 0, Vec::new(), Vec::new()),
            };
            let new_parent = match get_i64(body, &mut i) {
                Ok(v) => v,
                Err(_) => return (E_DB, 0, Vec::new(), Vec::new()),
            };
            let new_name = match proto::get_key(body, &mut i) {
                Ok(v) => v,
                Err(_) => return (E_DB, 0, Vec::new(), Vec::new()),
            };
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
                        vec![
                            Event::Entry { parent, name },
                            Event::Entry {
                                parent: new_parent,
                                name: new_name,
                            },
                        ],
                    )
                }
                Err(e) => bail!(e),
            }
        }
        M_SETMODE => {
            let ino = match get_i64(body, &mut i) {
                Ok(v) => v,
                Err(_) => return (E_DB, 0, Vec::new(), Vec::new()),
            };
            let mode = proto::read_u32(body, &mut i).unwrap_or(0);
            engine.set_mode(ino, mode);
            (0, 0, Vec::new(), inode_change_events(engine, ino))
        }
        M_SETUIDGID => {
            let ino = match get_i64(body, &mut i) {
                Ok(v) => v,
                Err(_) => return (E_DB, 0, Vec::new(), Vec::new()),
            };
            let uid = proto::read_u32(body, &mut i).unwrap_or(0);
            let gid = proto::read_u32(body, &mut i).unwrap_or(0);
            engine.set_uid_gid(ino, uid, gid);
            (0, 0, Vec::new(), inode_change_events(engine, ino))
        }
        M_SETTIMES => {
            let ino = match get_i64(body, &mut i) {
                Ok(v) => v,
                Err(_) => return (E_DB, 0, Vec::new(), Vec::new()),
            };
            let atime = get_opt_i64(body, &mut i).unwrap_or(None);
            let mtime = get_opt_i64(body, &mut i).unwrap_or(None);
            engine.set_times(ino, atime, mtime);
            (0, 0, Vec::new(), inode_change_events(engine, ino))
        }
        M_TOUCH => {
            let ino = match get_i64(body, &mut i) {
                Ok(v) => v,
                Err(_) => return (E_DB, 0, Vec::new(), Vec::new()),
            };
            engine.touch_mtime(ino);
            (0, 0, Vec::new(), inode_change_events(engine, ino))
        }
        M_SETLK => {
            let ino = match get_i64(body, &mut i) {
                Ok(v) => v,
                Err(_) => return (E_DB, 0, Vec::new(), Vec::new()),
            };
            let owner = proto::read_u64(body, &mut i).unwrap_or(0);
            let start = proto::read_u64(body, &mut i).unwrap_or(0);
            let end = proto::read_u64(body, &mut i).unwrap_or(u64::MAX);
            let ty =
                LockType::from_u8(get_u8(body, &mut i).unwrap_or(2)).unwrap_or(LockType::Unlock);
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
            let ino = match get_i64(body, &mut i) {
                Ok(v) => v,
                Err(_) => return (E_DB, 0, Vec::new(), Vec::new()),
            };
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
        M_TREE_DUMP => {
            let entries = engine.dump_tree();
            let mut d = Vec::with_capacity(4 + entries.len() * 60);
            proto::put_u32(&mut d, entries.len() as u32);
            for (id, pid, name, is_dir, data_ref) in &entries {
                d.extend_from_slice(&id.to_be_bytes());
                d.extend_from_slice(&pid.to_be_bytes());
                proto::put_key(&mut d, name);
                d.push(if *is_dir { 1 } else { 0 });
                proto::put_key(&mut d, data_ref.as_deref().unwrap_or(""));
            }
            (0, 0, d, Vec::new())
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
    open_event_stream_keyed(addr, view, token, uid, "")
}

/// Like [`open_event_stream`], but presents `api_key` on the `M_OPEN`
/// handshake — what a `meta-server --require-api-key` demands, so the mount's
/// invalidation subscription carries the same credential as its work sockets.
pub fn open_event_stream_keyed(
    addr: &str,
    view: &str,
    token: &str,
    uid: u32,
    api_key: &str,
) -> io::Result<TcpStream> {
    let mut last = io::Error::new(io::ErrorKind::ConnectionRefused, "no meta addr");
    for a in split_addrs(addr) {
        match try_open_event_stream(&a, view, token, uid, api_key) {
            Ok(s) => return Ok(s),
            // A rejected credential is a config error on every node; don't rotate.
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => return Err(e),
            Err(e) => last = e,
        }
    }
    Err(last)
}

fn try_open_event_stream(
    addr: &str,
    view: &str,
    token: &str,
    uid: u32,
    api_key: &str,
) -> io::Result<TcpStream> {
    let mut s = TcpStream::connect(addr)?;
    s.set_read_timeout(None)?; // block waiting for pushed events
    s.set_write_timeout(Some(Duration::from_secs(5)))?;
    s.set_nodelay(true)?;
    let mut body = Vec::new();
    proto::put_key(&mut body, view);
    proto::put_key(&mut body, token);
    proto::put_u32(&mut body, uid);
    proto::put_key(&mut body, api_key);
    proto::send_request(&mut s, M_OPEN, &body)?;
    let (st, _, reason) = proto::recv_response(&mut s)?;
    if st == E_AUTH {
        // The server states the cause in the payload (an old one sends none).
        let why = if reason.is_empty() {
            "meta-server rejected the access token".to_string()
        } else {
            String::from_utf8_lossy(&reason).into_owned()
        };
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, why));
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
///
/// `display`: when the client wraps its provider in a local `LayoutMeta` lens
/// (`--layout FILE`), passing it here lets the listener ALSO translate
/// source-coordinate events into display coordinates (inval_entry + inval_inode
/// at the synthesized parent), narrowing the staleness window for relocated
/// entries from the attr-TTL (~2s) to sub-ms.
#[cfg(feature = "native")]
pub fn spawn_invalidation_listener(
    addr: &str,
    view: &str,
    token: &str,
    api_key: &str,
    uid: u32,
    notifier: Notifier,
    display: Option<Arc<dyn MetaProvider>>,
) -> thread::JoinHandle<()> {
    let addr = addr.to_string();
    let view = view.to_string();
    let token = token.to_string();
    let api_key = api_key.to_string();
    thread::spawn(move || loop {
        match run_event_stream(
            &addr,
            &view,
            &token,
            &api_key,
            uid,
            &notifier,
            display.as_ref(),
        ) {
            // A rejected credential can't be fixed by retrying. If this mount was
            // already serving, the refusal means its key was revoked (or the
            // server was reconfigured under it) — the same auth LOSS the work
            // connections report, so ask for the same clean unmount instead of
            // silently keeping a keyless mount alive with no invalidations.
            Err(ref e) if e.kind() == io::ErrorKind::PermissionDenied => {
                super::signal_auth_loss(&e.to_string());
                eprintln!("invalidation listener stopped: {e}");
                return;
            }
            // EOF / transport error: back off briefly, then reconnect fresh.
            _ => thread::sleep(Duration::from_millis(300)),
        }
    })
}

#[cfg(feature = "native")]
fn run_event_stream(
    addr: &str,
    view: &str,
    token: &str,
    api_key: &str,
    uid: u32,
    notifier: &Notifier,
    display: Option<&Arc<dyn MetaProvider>>,
) -> io::Result<()> {
    let mut s = open_event_stream_keyed(addr, view, token, uid, api_key)?;
    loop {
        let (kind, val, data) = proto::recv_response(&mut s)?;
        match kind {
            EV_ENTRY => {
                let mut i = 0usize;
                if let Ok(name) = proto::get_key(&data, &mut i) {
                    // A missing/already-evicted entry is harmless; ignore errors.
                    let _ = notifier.inval_entry(INodeNo(val), OsStr::new(&name));
                    // Display-coordinate enrichment for local-lens mounts:
                    if let Some(prov) = display {
                        // Whoever's listing changed, the materialized rows for
                        // it are now wrong — the kernel is told, the provider
                        // must be told too (a lens caches its own shaping).
                        prov.flush_listing(val as i64);
                        for (p, dn) in prov.display_targets(val as i64, &name) {
                            let _ = notifier.inval_entry(INodeNo(p as u64), OsStr::new(&dn));
                            let _ = notifier.inval_inode(INodeNo(p as u64), 0, -1);
                            prov.flush_listing(p);
                        }
                    }
                }
            }
            EV_INODE => {
                // offset 0, len -1 => invalidate the whole inode (attrs + pages),
                // so a peer's write to a file we have open is seen on next read.
                let _ = notifier.inval_inode(INodeNo(val), 0, -1);
                // The server sends this for a display parent whose listing a
                // relocated entry just changed; a provider caching listings has
                // to drop the same rows the kernel just forgot.
                if let Some(prov) = display {
                    prov.flush_listing(val as i64);
                }
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
        assert_eq!(
            split_addrs("10.0.0.1:1,10.0.0.2:1"),
            vec!["10.0.0.1:1", "10.0.0.2:1"]
        );
        // whitespace tolerated, empties dropped
        assert_eq!(split_addrs(" a , , b "), vec!["a", "b"]);
        // a single address stays a single-element list (unchanged)
        assert_eq!(split_addrs("only:1"), vec!["only:1"]);
        // a degenerate all-empty input collapses to the original so nothing vanishes
        assert_eq!(split_addrs(""), vec![""]);
    }

    #[test]
    fn forward_event_codec_roundtrips() {
        // A forwarded batch must decode to the identical events on the standby,
        // preserving Entry(parent,name) and Inode(ino) payloads.
        let events = vec![
            Event::Entry {
                parent: 1,
                name: "alpha.txt".to_string(),
            },
            Event::Inode { ino: 42 },
            Event::Entry {
                parent: 7,
                name: "dir/sub".to_string(),
            },
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
            (
                Event::Entry {
                    parent: p1,
                    name: n1,
                },
                Event::Entry {
                    parent: p2,
                    name: n2,
                },
            ) => {
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
        for op in [
            M_CREATE,
            M_SYMLINK,
            M_LINK,
            M_UNLINK,
            M_RENAME,
            M_SETMODE,
            M_SETUIDGID,
            M_SETTIMES,
            M_TOUCH,
        ] {
            assert!(is_write_op(op), "op {op} must be write-intent");
        }
        for op in [
            M_OPEN,
            M_LOOKUP,
            M_GETATTR,
            M_READDIR,
            M_READDIRP,
            M_SUBSCRIBE,
            M_PING,
            M_PUSH,
            M_FORWARD,
        ] {
            assert!(!is_write_op(op), "op {op} must not be write-intent");
        }
    }

    #[test]
    fn remote_client_pages_through_m_readdirp_when_the_server_supports_it() {
        // End-to-end proof of the RPC paging path: a client mounting a
        // SQLite-backed server sees the M_READDIRP capability in the M_OPEN ack
        // (`supports_paged_readdir()` -> READDIRPLUS is then offered), and its
        // paged readdir stitches back into the exact full listing.
        use crate::meta::ROOT_INO;

        let dir = tempfile::Builder::new()
            .prefix("yf_pagedir_")
            .tempdir()
            .unwrap();
        let base = dir.path().to_path_buf();
        // Views are explicit: provision the one this test mounts.
        drop(MetaStore::open_sqlite(&base, "alice", false).unwrap());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || serve_on_meta_auth(listener, base, ""));

        let meta = RemoteMeta::new(&addr, "alice").expect("handshake with a live server");
        assert!(
            meta.supports_paged_readdir(),
            "a SQLite-backed server advertises store-level paging"
        );

        let n = 7;
        for i in 0..n {
            meta.create_node(ROOT_INO, &format!("f{i}"), false, None, 0o644, 0, 0)
                .expect("create through the RPC path");
        }
        let full: Vec<String> = meta.readdir(ROOT_INO).into_iter().map(|x| x.name).collect();
        assert_eq!(full.len(), n as usize, "all entries listed");

        let mut stitched = Vec::new();
        let mut off = 0usize;
        loop {
            let page = meta.readdir_page(ROOT_INO, off, 3);
            if page.is_empty() {
                break;
            }
            stitched.extend(page.into_iter().map(|x| x.name));
            off += 3;
        }
        assert_eq!(
            stitched, full,
            "M_READDIRP pages must equal the full readdir"
        );
    }

    #[test]
    fn fenced_for_write_demotes_on_higher_witness_epoch() {
        // Zero-window fencing: a primary that has been fenced at the arbiter (a
        // peer promoted past it) must refuse its very NEXT write instead of
        // serving writes until the async replicator notices (<= REPL_INTERVAL).
        use crate::backend::{self, StorageBackend};
        use crate::remote::server::{serve_on_auth, Fence};

        let dir = tempfile::Builder::new()
            .prefix("yf_fencewt_")
            .tempdir()
            .unwrap();
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
            keys: None,
            cache: Arc::new(Mutex::new(HashMap::new())),
            views: Arc::new(Mutex::new(HashMap::new())),
            bases: Arc::new(Mutex::new(HashMap::new())),
            defs: Arc::new(Mutex::new(HashMap::new())),
            control: crate::mcp::control_store_for(dir.path(), None)
                .expect("file control store is infallible"),
            subs: Arc::new(Mutex::new(HashMap::new())),
            ha: Some(ha.clone()),
            peer: Some("127.0.0.1:1".to_string()),
            forward: Mutex::new(Vec::new()),
            sync_repl: false,
            standby_push: Mutex::new(()),
            witness: Some(waddr),
            store: StoreKind::Sqlite,
        });

        // Witness holds nothing above our epoch: the first proof grants a lease
        // and we keep authority, staying Primary.
        assert!(
            !rt.fenced_for_write(),
            "a still-authoritative primary must not self-fence"
        );
        assert_eq!(ha.role(), Role::Primary);
        assert!(
            ha.lease_valid(),
            "a successful proof establishes a live lease"
        );

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
        assert_eq!(
            ha.role(),
            Role::Standby,
            "fence must demote to standby immediately"
        );
        assert_eq!(
            ha.epoch.load(Ordering::SeqCst),
            9,
            "must adopt the witness epoch"
        );
    }

    #[test]
    fn quorum_size_is_a_strict_majority() {
        // A single arbiter (the N=1 `--witness` shape) needs exactly 1 grant,
        // so the quorum path is a strict superset of the single-witness one.
        assert_eq!(quorum_size(1), 1);
        assert_eq!(
            quorum_size(2),
            2,
            "2 nodes need BOTH (no majority shortcut)"
        );
        assert_eq!(quorum_size(3), 2);
        assert_eq!(quorum_size(4), 3);
        assert_eq!(quorum_size(5), 3);
    }

    #[test]
    fn classify_single_arbiter_matches_single_witness_verdicts() {
        // N=1: quorum_size==1, so a single reply drives the verdict exactly as a
        // single witness does — advancing wins+leases, being ahead fences.
        let q = quorum_size(1);
        let ahead_of_us = classify(&[(7, true)], 5);
        assert!(
            ahead_of_us.fenced(q),
            "a higher token at the lone arbiter fences"
        );
        assert!(!ahead_of_us.leased(q));
        let we_lead = classify(&[(3, true)], 5);
        assert!(
            we_lead.won(q) && we_lead.leased(q),
            "raising the lone arbiter wins"
        );
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
        assert!(
            !one_grant.leased(q) && !one_grant.won(q),
            "1/3 is below quorum"
        );
        let two_grants = classify(&[(3, true), (3, true), (3, false)], 5);
        assert!(
            two_grants.won(q) && two_grants.leased(q),
            "2/3 grants win the quorum"
        );
        let one_ahead = classify(&[(7, false), (3, true), (3, true)], 5);
        assert!(!one_ahead.fenced(q), "one higher arbiter does not fence");
        let two_ahead = classify(&[(7, false), (8, false), (3, true)], 5);
        assert!(
            two_ahead.fenced(q),
            "a majority holding higher epochs fences us"
        );
        assert_eq!(
            two_ahead.prev_max, 8,
            "adopt the highest witness token we saw"
        );
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
        assert!(
            !loser.won(q),
            "a same-value collision must NOT re-win the quorum"
        );
    }
}

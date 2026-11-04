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
use crate::meta::{
    LockConflict, LockType, MetaError, MetaProvider, NodeInfo, NodeKind,
};
use crate::stats;
use crate::platform::EIO;
use parking_lot::Mutex;
use std::io;
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

// Imports only the `MetaServer` half (submodule [`server_ext`], native-gated)
// consumes through its `use super::*;`. They are excluded from the
// `remote-only` client build so the client half compiles warning-free.
#[cfg(feature = "native")]
use crate::layout::{LayoutMeta, LayoutRules, ViewDef};
#[cfg(feature = "native")]
use fuser::{INodeNo, Notifier};
#[cfg(feature = "native")]
use std::collections::HashMap;
#[cfg(feature = "native")]
use std::ffi::OsStr;
#[cfg(feature = "native")]
use std::io::{BufReader, Write};
#[cfg(feature = "native")]
use std::net::{TcpListener, ToSocketAddrs};
#[cfg(feature = "native")]
use std::path::{Path, PathBuf};
#[cfg(feature = "native")]
use std::sync::atomic::AtomicI64;
#[cfg(feature = "native")]
use std::time::Instant;

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
#[cfg(feature = "native")]
const M_SUBSCRIBE: u8 = 29;
/// HA replication (primary -> standby), never sent by mount clients. `M_PING`
/// is a role heartbeat carrying the sender's epoch and elicits the receiver's
/// epoch back; `M_PUSH` ships one whole `<view>.db` snapshot; `M_FORWARD`
/// relays a batch of invalidation events so clients subscribed to the standby
/// also see writes that happened on the primary (within one replication tick).
#[cfg(feature = "native")]
const M_PING: u8 = 30;
#[cfg(feature = "native")]
const M_PUSH: u8 = 31;
#[cfg(feature = "native")]
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
#[cfg(feature = "native")]
const M_TREE_DUMP: u8 = 38;

// --- pushed invalidation event kinds (carried in a response frame's status) --
// EV_ENTRY: val = parent inode, data = length-prefixed name -> kernel
// inval_entry (drop the cached dentry so the next lookup re-fetches).
// EV_INODE: val = inode -> kernel inval_inode over the whole file, which
// invalidates BOTH the cached attributes (size/mtime) and the page cache, so a
// peer's write to an already-open file becomes visible without waiting a TTL.
#[cfg(feature = "native")]
const EV_ENTRY: i32 = 0;
#[cfg(feature = "native")]
const EV_INODE: i32 = 1;

// --- MetaError <-> wire status code (1..7; 0 reserved for success) ---------
const E_EXISTS: i32 = 1;
const E_NOTFOUND: i32 = 2;
const E_NOTEMPTY: i32 = 3;
const E_TYPEMISMATCH: i32 = 4;
const E_NOTPERM: i32 = 5;
const E_CYCLE: i32 = 6;
#[cfg(feature = "native")]
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
#[cfg(feature = "native")]
const E_ROFS: i32 = 9;
// Returned on the HA control channel (node <-> node) when the peer carries a
// higher epoch than the sender: the sender is a stale ex-primary and must
// demote itself to standby. Node-level fencing — mount clients never see this
// status (they fail over via E_ROFS / transport errors, not epoch).
#[cfg(feature = "native")]
const E_EPOCH: i32 = 10;

/// Write-intent ops: everything from `M_CREATE` through `M_TOUCH`, plus the
/// lock-mutating ops `M_SETLK` / `M_RELEASE_LOCKS` (they INSERT/DELETE on the
/// `locks` table). A read-only HA node refuses these with `E_ROFS`; reads
/// (`LOOKUP`/`GETATTR`/`READDIR`/`M_GETLK`), `M_OPEN`, `M_SUBSCRIBE` and the
/// replication ops are always served.
#[cfg(feature = "native")]
fn is_write_op(op: u8) -> bool {
    (M_CREATE..=M_TOUCH).contains(&op) || op == M_SETLK || op == M_RELEASE_LOCKS || op == M_RENEW
}

// --- HA replication timing -------------------------------------------------
// A primary pushes a fresh snapshot of every view db to its standby every
// `REPL_INTERVAL`; the standby's watchdog promotes itself if the primary's
// heartbeats stop for `PROMOTE_TIMEOUT`. RPO ~= REPL_INTERVAL (see the docs on
// `start_replicator`/`start_promotion_watchdog` for the honest loss window).
#[cfg(feature = "native")]
const REPL_INTERVAL: Duration = Duration::from_millis(500);
#[cfg(feature = "native")]
const PROMOTE_TIMEOUT: Duration = Duration::from_secs(3);
#[cfg(feature = "native")]
const WATCHDOG_INTERVAL: Duration = Duration::from_millis(250);
#[cfg(feature = "native")]
const CONTROL_TIMEOUT: Duration = Duration::from_secs(2);

/// Fail-safe grace before a `--witness` node that cannot reach the arbiter
/// gives up its write authority. Set to `PROMOTE_TIMEOUT`: that is the window in
/// which a partitioned peer could already have promoted past us, so once we have
/// been unable to *prove* exclusive authority at the witness for that long we
/// stand down rather than risk a double-write. Kept at parity with promotion so
/// the two sides of the fence agree on the same upper bound.
#[cfg(feature = "native")]
const WITNESS_FENCE_GRACE: Duration = PROMOTE_TIMEOUT;

/// Arbiter lease length a node requests at the witness. A primary renews it in
/// the [`REPL_INTERVAL`] replicator tick, so in steady state it is always fresh
/// and a mutating op is admitted by a cheap LOCAL validity check instead of a
/// per-write arbiter round-trip. Equal to [`WITNESS_FENCE_GRACE`] so the arbiter
/// lease and the async fail-safe bound the same window.
#[cfg(feature = "native")]
const WITNESS_LEASE: Duration = WITNESS_FENCE_GRACE;

/// Safety margin subtracted from the lease when a node tracks its expiry on its
/// OWN clock: a holder stops trusting its cached lease [`WITNESS_LEASE_SLACK`]
/// before the arbiter would let a peer claim it, covering bounded host clock skew
/// plus one round-trip. Far shorter than the lease, so it only trims the tail.
#[cfg(feature = "native")]
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
#[cfg(feature = "native")]
const WITNESS_PROBE_TIMEOUT: Duration = Duration::from_millis(1000);

#[cfg(feature = "native")]
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
#[cfg(feature = "native")]
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
#[cfg(feature = "native")]
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
#[cfg(feature = "native")]
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

#[cfg(feature = "native")]
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

/// Split a `--meta` value into a failover address list. Empty/whitespace-only
/// inputs collapse to the original string so behavior never silently vanishes.
/// Shared by the client ([`RemoteMeta`] failover) and the native `MetaServer`
/// half (via `use super::*;`), so it lives in the always-compiled client file.
pub(crate) fn split_addrs(s: &str) -> Vec<String> {
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

#[cfg(test)]
mod client_tests {
    use super::split_addrs;

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
}

// ---- meta-server (central authoritative store) half, native only -----------
#[cfg(feature = "native")]
mod server_ext;
#[cfg(feature = "native")]
pub use server_ext::*;

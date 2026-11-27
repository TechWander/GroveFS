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
use crate::meta::{LockConflict, LockType, MetaError, MetaProvider, NodeInfo, NodeKind};
use crate::platform::EIO;
use crate::stats;
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

/// Which `--stats` row a metadata round trip belongs to, by opcode.
///
/// A `meta-server` books the engine's internals in ITS process (`db.sqlite_*`,
/// and `meta.*` around each store call); a mount's report can only honestly
/// name the client-perceived round trip. Without this mapping every metadata op
/// collapses into the single `remeta.rpc` row, so a mount that spends its time
/// in `LOOKUP` looks identical to one hammering `READDIR`. Ops with no row of
/// their own (session setup, HA, locks, lease renewal, the tree dump) stay in
/// `remeta.rpc` only.
fn meta_stage(op: u8) -> Option<stats::Stage> {
    Some(match op {
        M_LOOKUP => stats::Stage::MetaLookup,
        M_GETATTR => stats::Stage::MetaGetattr,
        M_READDIR => stats::Stage::MetaReaddir,
        M_READDIRP => stats::Stage::MetaReaddirPage,
        // One row for every op that materialises a directory entry: the plain
        // create, the symlink, the hard link, and the touch that re-creates.
        M_CREATE | M_SYMLINK | M_LINK | M_TOUCH => stats::Stage::MetaCreate,
        M_UNLINK => stats::Stage::MetaUnlink,
        M_RENAME => stats::Stage::MetaRename,
        M_SETMODE | M_SETUIDGID | M_SETTIMES => stats::Stage::MetaSetattr,
        _ => return None,
    })
}

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
// Returned by a SHARDED meta-server (`--shard-ring`) at `M_OPEN` when this
// process does not serve the requested view: `data` is the address of the
// instance that does, and the client dials it directly. Not a MetaError code and
// never a mid-session status — routing is decided once per connection, because
// `M_OPEN` binds the socket to the view, so no other op needs to be proxied or
// re-routed. A client that predates this status reads it as an ordinary refusal
// (the mount fails with the server's sentence), which is the documented
// upgrade requirement for a sharded deployment rather than a silent
// misroute.
const E_SHARD: i32 = 11;
/// How many redirects one `M_OPEN` may follow before the client gives up. Three
/// is one more than a correct ring ever needs (entry → home, plus a hop for an
/// operator mid-`view move`): it bounds a ring whose rows disagree, and the
/// already-tried-address check in [`RemoteMeta::connect_fresh`] bounds a cycle,
/// so no placement row can make a mount loop.
const SHARD_FOLLOW_MAX: usize = 3;

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

/// How long a write waits for someone else's in-flight arbiter re-proof before it
/// stops waiting ([`fenced_for_write`], taken only when the local lease has
/// already expired).
/// One probe window plus a spawn allowance: the leader is bounded by
/// [`WITNESS_PROBE_TIMEOUT`] itself, so a follower that exceeds this has hit a
/// stuck worker, not a slow arbiter, and must not queue up behind it. Timing out
/// here NEVER means "fenced" — it means "no fresh proof this round", the same
/// verdict a transient arbiter error already yields.
#[cfg(feature = "native")]
const PROOF_JOIN_WAIT: Duration = Duration::from_millis(1250);

/// Connect budget for the SYNCHRONOUS replication push (`--sync-repl`). A bare
/// `TcpStream::connect` runs the OS SYN-retry schedule (minutes) when a standby's
/// port is filtered rather than refused, and that call sits ON the write path
/// while the global push slot is held — so one blackholed peer used to be able to
/// occupy every meta worker at once. Same value as [`WITNESS_PROBE_TIMEOUT`]: the
/// failure mode is identical, and exceeding it degrades this op to async exactly
/// like every other push failure.
#[cfg(feature = "native")]
const SYNC_PUSH_CONNECT: Duration = WITNESS_PROBE_TIMEOUT;

/// How long a mutating op waits for the (single) in-flight snapshot push before
/// degrading itself to async. The push owns `rt.standby_push` across a whole
/// `VACUUM INTO` + transfer + ACK, so an unbounded `lock()` here serializes every
/// writer on the node behind one peer's round-trip. A short bounded wait keeps
/// RPO=0 for the common case (pushes are fast) while a slow peer costs other
/// views their push for ONE tick, which is what the async replicator is for.
#[cfg(feature = "native")]
const SYNC_PUSH_SLOT_WAIT: Duration = Duration::from_millis(200);

/// How long a peer/witness NAME LOOKUP may take before the caller treats the
/// address as unreachable. `std` hands `getaddrinfo` no deadline, and because the
/// lookup precedes `connect_timeout` it is NOT covered by any connect budget: a
/// hostname plus a dead resolver used to be an unbounded stall sitting on a meta
/// worker. One replication tick is generous for a real resolver (LAN answers land in the
/// tens of ms) and keeps the added worst case off the latency-critical path.
#[cfg(feature = "native")]
const DNS_RESOLVE_BUDGET: Duration = Duration::from_millis(500);

/// How many name lookups may sit in the OS resolver at once, process-wide. The
/// lookup runs on its own thread precisely because it cannot be interrupted; this
/// cap is what stops a permanently stuck resolver from minting one thread per
/// write. Two lets a peer and an arbiter list still make progress while anything
/// beyond that degrades like every other over-budget path ([`DNS_RESOLVE_BUDGET`]).
#[cfg(feature = "native")]
const DNS_MAX_INFLIGHT: usize = 2;

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
    /// What the mounted name resolved to on the server, and the other half of
    /// its definition — the BASE for a derived lens or fork, the public entry
    /// name for a pure alias (ack tail items [3] and [4]). A mount prints the
    /// same startup contract from these that a node reading its own store
    /// prints. Latched on the first accepted handshake like `dac`.
    resolved: std::sync::OnceLock<String>,
    def_base: std::sync::OnceLock<String>,
    /// The kind the server resolved the mounted name to (`physical`, `derived`,
    /// `alias`, `fork`) — ack tail item [5]. Kept separate from the `readonly`
    /// flag because a read-only API KEY also sets that byte, and a key gate is
    /// not a derived view.
    def_kind: std::sync::OnceLock<&'static str>,
    /// Data-Access Capability minted by the meta-server on this mount's
    /// `M_OPEN` (see [`crate::remote::dac`]); the client replays it to each
    /// data-server `OP_OPEN`. `None` when the server runs without
    /// `--data-access-key`. Set once on the first accepted handshake — the
    /// capability is stable for a mount, like the member id on the data side.
    dac: std::sync::OnceLock<String>,
    /// The address an `E_SHARD` redirect named for this view, learned on the
    /// first connection. Dialed FIRST on every later reconnect, so a sharded
    /// deployment pays the redirect once per mount rather than once per
    /// connection (an answer naming an HA pair pins both roles, in order), and a
    /// home that moves (`view move --to-meta`) is rediscovered on the next
    /// refusal instead of being cached forever. `None` whenever the deployment
    /// does not shard — nothing sets it, and the configured failover list stays
    /// the whole truth.
    shome: Mutex<Option<String>>,
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
    /// handshake (the `mount --run-as-uid` override) instead of the process euid, so
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
            resolved: std::sync::OnceLock::new(),
            def_base: std::sync::OnceLock::new(),
            def_kind: std::sync::OnceLock::new(),
            dac: std::sync::OnceLock::new(),
            shome: Mutex::new(None),
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

    /// The view name the server resolved the mount to: the mounted name itself,
    /// or the real tree behind a pure alias.
    pub fn resolved_view(&self) -> String {
        self.resolved.get().cloned().unwrap_or_default()
    }

    /// The BASE view of a derived or fork definition; for a pure alias, the
    /// public entry name the mount was addressed by. Empty for a bare physical
    /// view.
    pub fn def_base_view(&self) -> String {
        self.def_base.get().cloned().unwrap_or_default()
    }

    /// `physical` / `derived` / `alias` / `fork` as the server resolved the
    /// mounted name.
    pub fn def_kind(&self) -> &'static str {
        self.def_kind.get().copied().unwrap_or("physical")
    }

    /// The Data-Access Capability the meta-server minted for this mount's
    /// admission (see [`crate::remote::dac`]); empty when the server runs
    /// without `--data-access-key`. `mount` forwards it to every data-server
    /// `OP_OPEN`, where a server in verify mode requires it — so a rogue meta
    /// that lacks the shared key cannot arm a client against the honest data
    /// plane. Latched on the first accepted `M_OPEN` (stable for the mount).
    pub fn data_access_capability(&self) -> String {
        self.dac.get().cloned().unwrap_or_default()
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
    ///
    /// A sharded deployment adds one step: an `E_SHARD` answer names the
    /// instance that serves this view, so the candidate list gains that address
    /// at the front and the mount is established THERE — the entry point routes,
    /// it never proxies. The learned home is dialed first on later reconnects and
    /// dropped when a single-address answer stops answering at the transport
    /// level, so neither a moved view nor a dead node sticks.
    fn connect_fresh(&self) -> io::Result<(TcpStream, bool)> {
        let mut last_err = io::Error::new(io::ErrorKind::ConnectionRefused, "no connect attempt");
        let n = self.addrs.len();
        let start = self.active.load(Ordering::Relaxed) % n;
        // Candidates as (failover-list index, address): an index is present only
        // for an address the operator configured, because that is the list
        // `active` may be promoted within. A redirect target is not part of the
        // deployment's failover order and must never become its "preferred" node
        // — the next home is the placement table's business, not ours.
        let mut queue: Vec<(Option<usize>, String)> = (0..n)
            .map(|k| {
                let idx = (start + k) % n;
                (Some(idx), self.addrs[idx].clone())
            })
            .collect();
        // The remembered answer may itself be an HA PAIR, so seed every role in
        // the operator's order rather than the raw string — `"a+b"` is not a
        // dialable address, and a pin that never connects would be dropped.
        if let Some(home) = self.shome.lock().clone() {
            for (i, part) in home
                .split('+')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .enumerate()
            {
                if !queue.iter().any(|(_, a)| *a == part) {
                    queue.insert(i, (None, part.to_string()));
                }
            }
        }
        let mut tried: Vec<String> = Vec::new();
        let mut follows = 0usize;
        'hosts: while !queue.is_empty() {
            let (idx, addr) = queue.remove(0);
            // Cycle guard: a ring whose rows point at each other can name an
            // address this mount already dialed. Trying it twice would loop.
            if tried.contains(&addr) {
                // Naming it beats reporting the generic "no connect attempt"
                // that a redirect-only queue would otherwise leave behind.
                if follows > 0 {
                    last_err = io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "meta-server shard placement for view '{}' points back at {addr}, \
                             which this mount already asked — the placement rows and the ring \
                             disagree",
                            self.view
                        ),
                    );
                }
                continue 'hosts;
            }
            tried.push(addr.clone());
            for attempt in 0..self.connect_retries {
                match TcpStream::connect(addr.as_str()) {
                    Ok(s) => {
                        let _ = s.set_read_timeout(Some(self.connect_timeout));
                        let _ = s.set_write_timeout(Some(self.connect_timeout));
                        let _ = s.set_nodelay(true);
                        // Long-lived reuse sockets: a vanished meta-server
                        // must surface as a read error (not hang forever) so
                        // failover/retry paths can run.
                        crate::remote::enable_tcp_keepalive(&s);
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
                                // The ack body is the three capability flags
                                // plus the definition tail the server just
                                // assembled; a short body means the peer is not
                                // this protocol. Refuse it instead of guessing a
                                // default (a silently "writable" mount of a
                                // read-only view is far worse than a clear
                                // handshake error).
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
                                // Bytes [3..5]: the name the server resolved the
                                // mount to, the BASE of its definition, and the
                                // definition kind — what lets the mount print its
                                // startup contract at open instead of inferring the
                                // shape from a first failing write. Mandatory: a peer
                                // that stops after the flags is a build mismatch.
                                let mut di = 3usize;
                                let (resolved, base, kind) = match (
                                    proto::get_key(&data, &mut di),
                                    proto::get_key(&data, &mut di),
                                    proto::get_key(&data, &mut di),
                                ) {
                                    (Ok(r), Ok(b), Ok(k)) => (r, b, k),
                                    _ => {
                                        return Err(io::Error::new(
                                            io::ErrorKind::InvalidData,
                                            "meta-server handshake ack is missing the view-definition tail — client and server builds must match",
                                        ))
                                    }
                                };
                                let kind = match kind.as_str() {
                                    "fork" => "fork",
                                    "derived" => "derived",
                                    "alias" => "alias",
                                    _ => "physical",
                                };
                                // Latched for the life of the mount (the view name
                                // a connection re-handshakes for never changes),
                                // like the capability below.
                                let _ = self.resolved.set(resolved);
                                let _ = self.def_base.set(base);
                                let _ = self.def_kind.set(kind);
                                // Whatever follows is (optional): a Data-Access
                                // Capability appended by a `--data-access-key`
                                // meta-server. An absent tail means the server
                                // issues none.
                                if data.len() > di {
                                    if let Ok(d) = proto::get_key(&data, &mut di) {
                                        if !d.is_empty() {
                                            let _ = self.dac.set(d);
                                        }
                                    }
                                }
                                if let Some(i) = idx {
                                    self.active.store(i, Ordering::Relaxed);
                                }
                                return Ok((s, false));
                            }
                            // This instance does not serve the view and said so
                            // with the address that does: dial it, remember it for
                            // the next reconnect, and count the hop. Bounded by
                            // [`SHARD_FOLLOW_MAX`] plus the already-tried check
                            // above, so a placement table that contradicts its own
                            // ring fails the mount with a named cause instead of
                            // spinning between two hosts.
                            Ok((E_SHARD, _, target)) => {
                                let home = String::from_utf8_lossy(&target).trim().to_string();
                                if home.is_empty() || home.len() > 255 {
                                    last_err = io::Error::new(
                                        io::ErrorKind::InvalidData,
                                        format!(
                                            "meta-server {addr} redirected this mount to an \
                                             unreadable address"
                                        ),
                                    );
                                    continue 'hosts;
                                }
                                if follows >= SHARD_FOLLOW_MAX {
                                    last_err = io::Error::new(
                                        io::ErrorKind::InvalidData,
                                        format!(
                                            "meta-server {addr} redirected this mount to {home}, \
                                             but it had already followed {follows} redirects — \
                                             the shard placement rows disagree with the ring"
                                        ),
                                    );
                                    continue 'hosts;
                                }
                                follows += 1;
                                stats::count(stats::Counter::ShardRedirect, 1);
                                // A ring member may be an HA PAIR (`prim+stby`),
                                // in which case the redirect carries both roles
                                // in the operator's preferred order and the
                                // failover walk below serves them — sharding and
                                // `--peer` replication compose without a second
                                // mechanism.
                                let parts: Vec<String> = home
                                    .split('+')
                                    .map(str::trim)
                                    .filter(|s| !s.is_empty())
                                    .map(str::to_string)
                                    .collect();
                                if parts.is_empty() {
                                    continue 'hosts;
                                }
                                *self.shome.lock() = Some(home);
                                for (i, part) in parts.into_iter().enumerate() {
                                    if !queue.iter().any(|(_, a)| *a == part) {
                                        queue.insert(i, (None, part));
                                    }
                                }
                                continue 'hosts;
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
                    Err(e) => {
                        // Name the unreachable meta-server so a mount that fails
                        // fast on a dead primary reads as "cannot reach
                        // meta-server <addr>: Connection refused" instead of a
                        // bare errno (§5-12); ErrorKind is preserved for the
                        // failover/retry logic above.
                        last_err = io::Error::new(
                            e.kind(),
                            format!("cannot reach meta-server {addr}: {e}"),
                        );
                        // A redirect target that will not answer at the transport
                        // level is a stale answer, not a live refusal: forget it,
                        // so the next reconnect starts from the configured list
                        // instead of spending its retry budget on a host that
                        // moved away or died. A host that ANSWERED with an error
                        // status keeps the entry (its placement is real). The one
                        // exception is an HA pair: a row that names two roles is
                        // still the placement, and one of them being down is
                        // failover's business, not a reason to forget the row.
                        let pinned_pair = self
                            .shome
                            .lock()
                            .as_deref()
                            .map(|h| h.contains('+'))
                            .unwrap_or(false);
                        if idx.is_none() && !pinned_pair {
                            *self.shome.lock() = None;
                        }
                    }
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
        // ... and the same interval again under the row its opcode belongs to,
        // so a mount's report still names WHICH metadata op is slow.
        let _stage = meta_stage(op).map(stats::start);
        let (mut s, reused) = self.get_conn()?;
        match Self::call(&mut s, op, body) {
            Ok(r) => {
                // A sharded instance that no longer serves this view answers
                // `E_SHARD` and hangs up: its engine was dropped by the placement
                // sweep, and re-opening a tree the ring assigned to a peer would
                // split one view name across two stores. The payload names the new
                // home, so adopt it, throw away the WHOLE pool (every socket in it
                // belongs to the instance that just hung up) and replay the op
                // THERE. The refused op never ran, so the replay is not a second
                // write — and an operator's `view move --to-meta` needs no remount.
                if r.0 == E_SHARD {
                    let home = String::from_utf8_lossy(&r.2).trim().to_string();
                    if !home.is_empty() && home.len() <= 255 {
                        *self.shome.lock() = Some(home);
                    }
                    drop(s);
                    self.pool.lock().clear();
                    stats::count(stats::Counter::ShardRedirect, 1);
                    let (mut s2, _) = self.connect_fresh()?;
                    let r2 = Self::call(&mut s2, op, body)?;
                    self.note_refusal(&r2);
                    self.put_conn(s2);
                    return Ok(r2);
                }
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

    /// Like [`Self::call_id`], but keeps the node the reply carries.
    ///
    /// `M_CREATE`/`M_SYMLINK` replies append the created row, so the client can
    /// fill the FUSE reply without a second `M_GETATTR` round trip. The row is a
    /// best-effort read-back on the server (`node_reply`), so an empty tail is a
    /// live outcome, not a protocol mismatch: the caller then does that `getattr`
    /// itself, which is the same RPC the reply was meant to save.
    fn call_id_node(&self, op: u8, body: &[u8]) -> Result<(i64, Option<NodeInfo>), MetaError> {
        match self.round_trip(op, body) {
            Ok((0, val, data)) => {
                let node = if data.is_empty() {
                    None
                } else {
                    node_from(&data, &mut 0).ok()
                };
                Ok((val as i64, node))
            }
            Ok((c, _, _)) => Err(err_from_code(c)),
            Err(_) => Err(MetaError::Db("remote meta transport".to_string())),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn create_node_rpc(
        &self,
        parent: i64,
        name: &str,
        is_dir: bool,
        data_ref: Option<&str>,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<(i64, Option<NodeInfo>), MetaError> {
        let mut b = Vec::new();
        put_i64(&mut b, parent);
        put_u8(&mut b, is_dir as u8);
        put_opt(&mut b, data_ref);
        proto::put_u32(&mut b, mode);
        proto::put_u32(&mut b, uid);
        proto::put_u32(&mut b, gid);
        proto::put_key(&mut b, name);
        self.call_id_node(M_CREATE, &b)
    }

    fn create_symlink_rpc(
        &self,
        parent: i64,
        name: &str,
        target: &str,
        uid: u32,
        gid: u32,
    ) -> Result<(i64, Option<NodeInfo>), MetaError> {
        let mut b = Vec::new();
        put_i64(&mut b, parent);
        proto::put_key(&mut b, target);
        proto::put_u32(&mut b, uid);
        proto::put_u32(&mut b, gid);
        proto::put_key(&mut b, name);
        self.call_id_node(M_SYMLINK, &b)
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
        // page. Otherwise slice a single full listing here and pay for it per
        // kernel buffer.
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
        self.create_node_rpc(parent, name, is_dir, data_ref, mode, uid, gid)
            .map(|(id, _)| id)
    }

    fn create_node_with_attr(
        &self,
        parent: i64,
        name: &str,
        is_dir: bool,
        data_ref: Option<&str>,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<NodeInfo, MetaError> {
        match self.create_node_rpc(parent, name, is_dir, data_ref, mode, uid, gid)? {
            (_, Some(node)) => Ok(node),
            // Old server: its reply carries only the id, so read the row back.
            (id, None) => self.getattr(id).ok_or(MetaError::NotFound),
        }
    }

    fn create_symlink(
        &self,
        parent: i64,
        name: &str,
        target: &str,
        uid: u32,
        gid: u32,
    ) -> Result<i64, MetaError> {
        self.create_symlink_rpc(parent, name, target, uid, gid)
            .map(|(id, _)| id)
    }

    fn create_symlink_with_attr(
        &self,
        parent: i64,
        name: &str,
        target: &str,
        uid: u32,
        gid: u32,
    ) -> Result<NodeInfo, MetaError> {
        match self.create_symlink_rpc(parent, name, target, uid, gid)? {
            (_, Some(node)) => Ok(node),
            (id, None) => self.getattr(id).ok_or(MetaError::NotFound),
        }
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

/// Split a `--meta-server` value into a failover address list. Empty/whitespace-only
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
        // The client's `--meta-server a,b` parsing that drives failover.
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

// Client side of the sharded deployment: what an `E_SHARD` answer does to the
// connect walk. Native-gated because the stub is a `TcpListener` and
// `remote-only` builds ship the client half only.
#[cfg(all(test, feature = "native"))]
mod shard_follow_tests {
    use super::*;
    use std::net::{SocketAddr, TcpListener};
    use std::sync::atomic::AtomicUsize;

    /// A meta-server stand-in: answers every `M_OPEN` with `(status, payload)`
    /// and counts the handshakes it served. Serves connections one at a time on
    /// its own thread and never stops, so a test can dial it repeatedly; the
    /// thread and the port die with the test process.
    fn stub(
        hits: Arc<AtomicUsize>,
        reply: impl Fn() -> (i32, Vec<u8>) + Send + 'static,
    ) -> (SocketAddr, thread::JoinHandle<()>) {
        let lst = TcpListener::bind("127.0.0.1:0").expect("bind stub");
        let addr = lst.local_addr().expect("stub addr");
        let h = thread::spawn(move || {
            for conn in lst.incoming() {
                let Ok(mut s) = conn else { continue };
                let Ok((op, body)) = proto::recv_request(&mut s) else {
                    continue;
                };
                if op != M_OPEN || proto::get_key(&body, &mut 0).is_err() {
                    continue;
                }
                hits.fetch_add(1, Ordering::SeqCst);
                let (status, data) = reply();
                let _ = proto::send_response(&mut s, status, 0, &data);
            }
        });
        (addr, h)
    }

    /// A successful `M_OPEN` ack: the three capability flags plus the
    /// view-definition tail, exactly what a real server assembles.
    fn ok_ack(view: &str) -> Vec<u8> {
        let mut b = vec![0u8, 1u8, 0u8];
        proto::put_key(&mut b, view);
        proto::put_key(&mut b, view);
        proto::put_key(&mut b, "physical");
        b
    }

    #[test]
    fn e_shard_answer_is_followed_and_remembered() {
        // The entry point routes, it does not proxy: the mount ends up OPEN on
        // the instance the redirect named.
        let (a_hits, b_hits) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let (b, _bh) = stub(b_hits.clone(), || (0, ok_ack("v")));
        let b_addr = b.to_string();
        let redir = b_addr.clone();
        let (a, _ah) = stub(a_hits.clone(), move || (E_SHARD, redir.as_bytes().to_vec()));

        let rm = RemoteMeta::new_with_creds(&a.to_string(), "v", "", 0, "").expect("follows hop");
        assert_eq!(a_hits.load(Ordering::SeqCst), 1);
        assert_eq!(b_hits.load(Ordering::SeqCst), 1);
        assert!(rm.opened.load(Ordering::SeqCst));
        // The learned home is remembered for the next reconnect…
        assert_eq!(*rm.shome.lock(), Some(b_addr.clone()));
        // …and never becomes the failover list's "preferred" node: `active` may
        // only ever name an address the operator configured.
        assert_eq!(rm.active.load(Ordering::Relaxed), 0);
        assert!(!rm.is_readonly_view());
        assert_eq!(rm.paged.load(Ordering::SeqCst), 1);

        // A later reconnect dials the learned home FIRST, without asking the
        // entry point again — routing stays a per-handshake decision, so the
        // steady-state mount costs one address, not a proxy hop per op.
        rm.connect_fresh().expect("reconnect");
        assert_eq!(a_hits.load(Ordering::SeqCst), 1, "entry point re-asked");
        assert_eq!(b_hits.load(Ordering::SeqCst), 2, "home not dialed first");
    }

    #[test]
    fn e_shard_redirect_may_name_an_ha_pair() {
        // Sharding composes with `--peer`/`--witness` HA: one ring member is a
        // PAIR, so the redirect carries both roles and the ordinary failover
        // walk serves them in order. Here the first role is not listening.
        let dead = {
            let l = TcpListener::bind("127.0.0.1:0").expect("dead port");
            l.local_addr().expect("dead addr")
        };
        let (a_hits, live_hits) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let (live, _lh) = stub(live_hits.clone(), || (0, ok_ack("v")));
        let pair = format!("{}+{}", dead, live);
        let redir = pair.clone();
        let (a, _ah) = stub(a_hits.clone(), move || (E_SHARD, redir.as_bytes().to_vec()));

        let rm = RemoteMeta::new_with_creds(&a.to_string(), "v", "", 0, "")
            .expect("falls through the pair to the standby");
        assert_eq!(a_hits.load(Ordering::SeqCst), 1);
        assert_eq!(live_hits.load(Ordering::SeqCst), 1);
        // The PAIR row stays learned even though its first role was down — the
        // next reconnect walks both halves without asking the entry point again.
        assert_eq!(*rm.shome.lock(), Some(pair));
        rm.connect_fresh().expect("re-dials the pair");
        assert_eq!(a_hits.load(Ordering::SeqCst), 1, "entry point re-asked");
        assert_eq!(live_hits.load(Ordering::SeqCst), 2, "pair pin lost");
    }

    #[test]
    fn e_shard_cycle_fails_with_a_named_cause() {
        // Two rows that name each other must fail the mount, not spin: the
        // already-tried check ends the walk and the error says what disagreed.
        let hits = Arc::new(AtomicUsize::new(0));
        let counted = hits.clone();
        let lst1 = TcpListener::bind("127.0.0.1:0").expect("bind");
        let lst2 = TcpListener::bind("127.0.0.1:0").expect("bind");
        let (x, y) = (lst1.local_addr().unwrap(), lst2.local_addr().unwrap());
        let y_addr = y.to_string();
        let x_addr = x.to_string();
        let _h1 = thread::spawn(move || {
            for conn in lst1.incoming() {
                let Ok(mut s) = conn else { continue };
                let _ = proto::recv_request(&mut s);
                counted.fetch_add(1, Ordering::SeqCst);
                let _ = proto::send_response(&mut s, E_SHARD, 0, y_addr.as_bytes());
            }
        });
        let _h2 = thread::spawn(move || {
            for conn in lst2.incoming() {
                let Ok(mut s) = conn else { continue };
                let _ = proto::recv_request(&mut s);
                let _ = proto::send_response(&mut s, E_SHARD, 0, x_addr.as_bytes());
            }
        });
        let e = match RemoteMeta::new_with_creds(&x.to_string(), "v", "", 0, "") {
            Ok(_) => panic!("a cycle between two placement rows cannot open a mount"),
            Err(e) => e,
        };
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        assert!(
            e.to_string().contains("points back at"),
            "unhelpful error: {e}"
        );
        // Bounded: two rows pointing at each other cost two handshakes, not one
        // per retry tick.
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn e_shard_with_no_address_is_not_fatal_to_the_mount_walk() {
        // A refusal that names nothing is a broken answer, not a home: the walk
        // must move on to the rest of the failover list instead of dialing "".
        let (a_hits, b_hits) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let (b, _bh) = stub(b_hits.clone(), || (0, ok_ack("v")));
        let (a, _ah) = stub(a_hits, || (E_SHARD, Vec::new()));
        let rm = RemoteMeta::new_with_creds(&format!("{},{}", a, b), "v", "", 0, "")
            .expect("fails over to the next configured address");
        assert_eq!(b_hits.load(Ordering::SeqCst), 1);
        assert_eq!(rm.active.load(Ordering::Relaxed), 1, "promoted to b");
        assert_eq!(*rm.shome.lock(), None, "an empty redirect was learned");
    }
}

// ---- meta-server (central authoritative store) half, native only -----------
#[cfg(feature = "native")]
mod server_ext;
#[cfg(feature = "native")]
pub use server_ext::*;

// Multi-meta-server VIEW SHARDING (`--shard-ring`): the ring, its placement
// table in the shared control plane, and the `M_OPEN` routing verdict. Server-side
// only — a client needs the `E_SHARD` status and nothing else, so the `postgres`
// dependency this module uses stays behind `native`.
#[cfg(feature = "native")]
mod shard;

// The event-loop alternative to `server_ext`'s thread-per-connection accept loops.
// Linux-only and `native`-only: `libc` is an optional dependency that rides
// `fuse-bridge`/`native`, so a remote-only (Windows `grovefs-win`) client build must not
// see it — the same platform-layering constraint that keeps `server_ext` out of that
// build. `server_ext` falls back to its thread loops wherever this is absent.
#[cfg(all(feature = "native", target_os = "linux"))]
mod epoll;

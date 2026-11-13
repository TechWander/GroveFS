//! The remote data service: a small accept loop that decodes request frames
//! from [`super::client::RemoteBackend`] and executes them against an inner
//! [`StorageBackend`] (typically the on-disk [`crate::backend::local::LocalBackend`]
//! living on the cluster node).
//!
//! This is the server half of the "central cluster stores data, clients mount
//! over the network" topology. One thread per connection; the heavy lifting is
//! the shared `backend`, which is `Send + Sync`.

use super::proto;
use crate::backend::StorageBackend;
use libc::{EACCES, EIO, ENOSYS, EROFS};
use parking_lot::RwLock;
use std::io::{self, BufReader};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

/// Arbiter lease granted for a fence claim when the client does not specify one
/// (a bare `[u64 epoch]` OP_FENCE). Kept above the client's renew cadence so a
/// live primary renews comfortably before expiry; the meta-side fail-safe bound
/// (`WITNESS_FENCE_GRACE`) matches, so both sides of the fence agree on one
/// upper bound.
const DEFAULT_LEASE_MS: u64 = 3_000;

/// Arbiter wall clock in ms since the UNIX epoch. It is only ever compared
/// against itself (grant vs expiry, both computed here), so a wall-clock jump
/// shifts both sides together; cross-node safety rests on the standard bounded
/// lease-skew assumption, not on absolute time.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// This node's hostname for the startup banner: data roots carry the SAME
/// paths across machines in typical deployments, so the listen line alone
/// cannot tell which node a log came from. Best-effort: falls back to the
/// `HOSTNAME` env, then `<unknown>`, rather than refusing to start.
fn node_hostname() -> String {
    let mut buf = [0u8; 256];
    if unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) } == 0 {
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        let h = String::from_utf8_lossy(&buf[..end]).into_owned();
        if !h.is_empty() {
            return h;
        }
    }
    std::env::var("HOSTNAME").unwrap_or_else(|_| "<unknown>".to_string())
}

/// Monotonic fencing token held by the witness (the `data-server`).
///
/// A node folds its HA epoch in via [`Fence::claim`]; the arbiter hands back the
/// value it held *before* the fold. That `prev` is what makes the verdict
/// unambiguous: `prev < sent` means this node's claim raised the token, so it
/// now holds the highest epoch and may serve; `prev >= sent` means an equal-or-
/// higher epoch was already recorded (a peer won, or a tie on the same value),
/// so the caller is fenced and must stand down. Returning `prev` rather than the
/// post-fold `max` is deliberate: with `max` a claim of `E` against an existing
/// `E` is indistinguishable from winning a fresh `E`, which would let two nodes
/// both believe they hold the token.
///
/// When backed by a path the token is persisted (temp-file + fsync + rename) on
/// every *increase*, so a witness restart cannot drop the epoch back to 0 and
/// resurrect a long-deposed primary. Persistence is best-effort: an in-memory
/// max still guards the running process, and a failed write is logged, never
/// silently swallowed as "safe".
///
/// A reader-writer `gate` linearizes admission against promotion. Write-intent
/// ops hold it SHARED across their epoch check AND the backend write, so they
/// run concurrently with each other (no throughput loss); a promotion claim
/// takes it EXCLUSIVE, so it waits for every in-flight write to drain before
/// folding a higher epoch. Because the witness and the storage live in this one
/// process, that drain is exactly what makes a fenced-then-promoted primary's
/// writes never physically overlap a stale epoch's writes on the same object —
/// closing the torn-write window at its implementation root, not just bounding
/// it. The residual (an ex-primary still SERVING until it learns it was fenced)
/// is the promotion-waits-lease-expiry question, which needs a quorum clock and
/// is out of scope for this single-arbiter fence.
pub struct Fence {
    cur: AtomicU64,
    path: Option<PathBuf>,
    /// Serializes the rare persist-on-increase writes across conn threads.
    persist: Mutex<()>,
    /// Drain gate: shared for write-intent ops (check+write), exclusive for a
    /// promotion claim. See the type docs.
    gate: RwLock<()>,
    /// Arbiter-side monotonic clock (ms since UNIX epoch) at which the CURRENT
    /// token holder's lease expires. A promotion (strictly higher epoch) is
    /// granted only after this instant passes, so two nodes can never hold a
    /// live lease at once — the non-overlap that lets a primary trust a locally
    /// cached lease instead of re-proving at the arbiter before every write.
    /// Advanced/refreshed only under `gate`, so it is consistent with `cur`.
    lease_until_ms: AtomicU64,
}

impl Fence {
    /// Volatile token, resets to 0 on restart — used by tests and any data-server
    /// started without a fence store path.
    pub fn new_in_memory() -> Self {
        Self {
            cur: AtomicU64::new(0),
            path: None,
            persist: Mutex::new(()),
            gate: RwLock::new(()),
            lease_until_ms: AtomicU64::new(0),
        }
    }

    /// Persistent token backed by `path`. The stored value is loaded at startup
    /// so it survives a data-server restart; a missing/corrupt file reads as 0.
    pub fn with_store(path: PathBuf) -> Self {
        let init = std::fs::read(&path)
            .ok()
            .and_then(|b| String::from_utf8_lossy(&b).trim().parse::<u64>().ok())
            .unwrap_or(0);
        Self {
            cur: AtomicU64::new(init),
            path: Some(path),
            persist: Mutex::new(()),
            gate: RwLock::new(()),
            // A restarted witness starts with no live lease; the persisted token
            // still fences lower claims, and the first lease request re-grants
            // after the (now-past) zero deadline.
            lease_until_ms: AtomicU64::new(0),
        }
    }

    /// Fold `sent` into the monotonic max and return the value held *before*
    /// this claim (`prev`). See the type docs for the win/fence rule.
    ///
    /// Takes the gate EXCLUSIVELY for the whole fold+persist: a promotion is
    /// linearized AFTER every in-flight write-intent op (which holds the gate
    /// shared across its own check+write) has drained. So once this returns with
    /// a raised token, no lower-epoch write can still be landing on the backend —
    /// the new primary's subsequent writes cannot interleave a stale one.
    pub fn claim(&self, sent: u64) -> u64 {
        let _gate = self.gate.write();
        let prev = self.cur.fetch_max(sent, Ordering::SeqCst);
        if sent > prev {
            if let Some(p) = &self.path {
                self.persist_current(p);
            }
        }
        prev
    }

    /// Acquire the gate SHARED for the duration of a write-intent op's epoch
    /// check AND its backend dispatch. Concurrent writes stay parallel; a
    /// [`Fence::claim`] cannot slip between the check and the write.
    pub fn write_gate(&self) -> parking_lot::RwLockReadGuard<'_, ()> {
        self.gate.read()
    }

    /// Lease-aware claim: fold `sent` into the monotonic token AND hold a
    /// time-bounded, NON-OVERLAPPING lease of `ttl_ms` for the resulting holder.
    /// Returns `(prev, granted_until_ms, granted)`.
    ///
    /// Rules, all decided under the exclusive gate so the token and the lease
    /// move together (and a promotion still drains in-flight writes):
    /// - `sent > cur` (promotion): grant ONLY if no live incumbent lease blocks
    ///   it — `cur == 0` (cold start) or the arbiter clock is already at/past
    ///   `lease_until_ms`. Otherwise refuse (`granted = false`) and hand back the
    ///   incumbent's expiry so the caller knows how long to wait. This is the
    ///   non-overlap guarantee: the arbiter physically cannot hand a lease to a
    ///   second node while the first is live, so a holder can trust a locally
    ///   cached lease instead of re-proving per write.
    /// - `sent == cur` (renew, or the very first grant at epoch 0): extend.
    /// - `sent < cur` (stale): refuse, surface the incumbent token + expiry.
    pub fn lease(&self, sent: u64, ttl_ms: u64) -> (u64, u64, bool) {
        let _gate = self.gate.write();
        let now = now_ms();
        let cur = self.cur.load(Ordering::SeqCst);
        let held_until = self.lease_until_ms.load(Ordering::SeqCst);
        if sent < cur {
            return (cur, held_until, false);
        }
        if sent == cur {
            let until = now + ttl_ms;
            self.lease_until_ms.store(until, Ordering::SeqCst);
            return (cur, until, true);
        }
        // Promotion: enforce non-overlap against a still-live incumbent lease.
        if cur != 0 && now < held_until {
            return (cur, held_until, false);
        }
        let prev = self.cur.fetch_max(sent, Ordering::SeqCst);
        let until = now + ttl_ms;
        self.lease_until_ms.store(until, Ordering::SeqCst);
        if sent > prev {
            if let Some(p) = &self.path {
                self.persist_current(p);
            }
        }
        (prev, until, true)
    }

    /// Current held token (no fold). Read on the data-plane hot path to gate
    /// write-intent ops against a stale epoch, and by diagnostics/tests.
    pub fn peek(&self) -> u64 {
        self.cur.load(Ordering::SeqCst)
    }

    fn persist_current(&self, path: &PathBuf) {
        // Hold the lock AND sample `cur` under it: write the authoritative
        // monotonic max, not this call's `sent`. Persisting `sent` could let a
        // concurrent higher claim land first and then be overwritten by this
        // lower value, regressing the durable token below an epoch already
        // granted (a deposed primary could then reclaim after a restart).
        // `cur` only rises via fetch_max, so sampling it here yields a value
        // >= every fold that has taken effect.
        let _g = self.persist.lock();
        let v = self.cur.load(Ordering::SeqCst);
        let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
        let r = (|| -> io::Result<()> {
            std::fs::write(&tmp, v.to_string())?;
            std::fs::File::open(&tmp)?.sync_all()?;
            std::fs::rename(&tmp, path)
        })();
        if let Err(e) = r {
            // Honest degrade: the in-memory max still fences within this
            // process; only a restart could lose this last bump. Never claim
            // durability we did not achieve.
            eprintln!("data-server: fence token persist failed ({e}); in-memory max still authoritative for this process");
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

pub struct DataServer {
    listen: String,
    /// Display-only copy of the `--data-dir` list, echoed at startup so an operator
    /// reading one node's log can tell WHICH disks this node serves and on
    /// WHICH host they live — root paths repeat across machines (the same
    /// `/data/gfs1/grove` name exists on several nodes), so a bare listen line
    /// is ambiguous in a multi-node cluster log. Placement itself is decided
    /// by the backend, not here.
    roots: Vec<PathBuf>,
    backend: Arc<dyn StorageBackend>,
    token: String,
    /// Optional per-caller credential gate (`--require-api-key`). When set, every
    /// `OP_OPEN` must present a live key from this store whose scope admits the
    /// mount plane — turning data access into revocable, attributable
    /// credentials instead of one static cluster token. The store is the same
    /// `api_keys.db` the MCP server and `grovefs apikey` manage (see
    /// [`crate::apikeys`]); an empty presentation is simply refused.
    keys: Option<Arc<crate::apikeys::ApiKeyStore>>,
    /// Shared fencing-witness token (HA). Serves two gates: `OP_FENCE` folds a
    /// node's epoch in and echoes the previously-held value, so the data-server
    /// acts as the third arbiter that tells a stale ex-primary to stand down even
    /// when the two meta nodes cannot see each other; and every write-intent
    /// content frame carries the client's epoch, which this token gates directly
    /// — a frame whose epoch lags the held token is refused with `EROFS` before
    /// its bytes reach the backend (see [`handle_conn`]).
    fence: Arc<Fence>,
    /// This node's stable member id, echoed in every `OP_OPEN` ack's `VAL` so a
    /// [`crate::backend::ShardedBackend`] client can pin a logical chunk to WHO
    /// this server is rather than its position in the caller's `--data-servers`
    /// list. Defaults to [`proto::NO_MEMBER_ID`] (started without `--member-id`),
    /// which tells the client to keep placing by list position.
    member_id: u64,
}

impl DataServer {
    /// A server that accepts any `OP_OPEN` (empty token) — the original,
    /// LAN-trusted behavior for clients started without a token.
    pub fn new(listen: &str, backend: Arc<dyn StorageBackend>) -> Self {
        Self {
            listen: listen.to_string(),
            roots: Vec::new(),
            backend,
            token: String::new(),
            keys: None,
            fence: Arc::new(Fence::new_in_memory()),
            member_id: proto::NO_MEMBER_ID,
        }
    }

    /// A server that only serves clients presenting `token` on the `OP_OPEN`
    /// handshake. With a non-empty token, an unauthorized host on the LAN can
    /// no longer read/write arbitrary objects by key (previously the keyspace
    /// was only guarded by the randomness of the object names themselves).
    pub fn with_token(listen: &str, backend: Arc<dyn StorageBackend>, token: &str) -> Self {
        Self {
            listen: listen.to_string(),
            roots: Vec::new(),
            backend,
            token: token.to_string(),
            keys: None,
            fence: Arc::new(Fence::new_in_memory()),
            member_id: proto::NO_MEMBER_ID,
        }
    }

    /// Additionally require a `grovefs apikey` credential (scope `mount` or
    /// `any`) on every `OP_OPEN`. Independent of the static token: when both
    /// are configured a client must satisfy both, so a leaked cluster token
    /// alone no longer buys access to the keyspace.
    pub fn with_api_keys(mut self, store: crate::apikeys::ApiKeyStore) -> Self {
        self.keys = Some(Arc::new(store));
        self
    }

    /// Back the fencing token with a durable file at `path` (loaded if it
    /// already exists). Called by the data-server CLI so a witness restart does
    /// not zero the epoch and let a deposed primary sneak back in as writer.
    pub fn with_fence_store(mut self, path: PathBuf) -> Self {
        self.fence = Arc::new(Fence::with_store(path));
        self
    }

    /// Record the `--data-dir` list for the startup banner (display only; the
    /// backend already owns the real roots).
    pub fn with_roots(mut self, roots: &[PathBuf]) -> Self {
        self.roots = roots.to_vec();
        self
    }

    /// Set this node's stable `--member-id`, echoed in the `OP_OPEN` ack so a
    /// striping client places chunks by member identity instead of list order.
    /// Members of one striped cluster must use DISTINCT ids; a stripe with any
    /// duplicate (or an id left at the `NO_MEMBER_ID` default) is treated by the
    /// client as positional placement.
    pub fn with_member_id(mut self, id: u64) -> Self {
        self.member_id = id;
        self
    }

    /// Blocks forever serving connections.
    pub fn serve(&self) -> io::Result<()> {
        let listener = TcpListener::bind(&self.listen)?;
        println!(
            "data-server listening on {} (host {}, {} data root(s): {})",
            self.listen,
            node_hostname(),
            self.roots.len(),
            self.roots
                .iter()
                .map(|r| r.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
        if self.keys.is_some() {
            println!("data-server: api-key auth REQUIRED (present `mount --api-key`, minted by `grovefs apikey create --for mount|any`); a revoke closes live connections on the next op");
        }
        serve_auth_keys(
            listener,
            self.backend.clone(),
            &self.token,
            self.fence.clone(),
            self.keys.clone(),
            self.member_id,
        );
        Ok(())
    }
}

/// Accept loop over an already-bound listener (spawns a thread per connection),
/// no access token — the no-auth variant used by tests and single-host setups.
pub fn serve_on(listener: TcpListener, backend: Arc<dyn StorageBackend>) {
    serve_auth_keys(
        listener,
        backend,
        "",
        Arc::new(Fence::new_in_memory()),
        None,
        proto::NO_MEMBER_ID,
    )
}

/// Accept loop with an access token (`""` disables the check) and a shared
/// fencing-witness token.
pub fn serve_on_auth(
    listener: TcpListener,
    backend: Arc<dyn StorageBackend>,
    token: &str,
    fence: Arc<Fence>,
) {
    serve_auth_keys(
        listener,
        backend,
        token,
        fence,
        None,
        proto::NO_MEMBER_ID,
    )
}

/// Accept loop that advertises a stable `member_id` in every `OP_OPEN` ack, so
/// a striping client places chunks by identity. Mirrors [`serve_on`] (no auth,
/// in-memory fence) with the id plumbed through — the test/embedded counterpart
/// of `data-server --member-id`.
pub fn serve_on_member(listener: TcpListener, backend: Arc<dyn StorageBackend>, member_id: u64) {
    serve_auth_keys(
        listener,
        backend,
        "",
        Arc::new(Fence::new_in_memory()),
        None,
        member_id,
    )
}

/// Accept loop with the full auth surface: an optional static token and an
/// optional per-caller API-key store. `member_id` is echoed in every `OP_OPEN`
/// ack so a striping client can place chunks by identity (see
/// [`DataServer::with_member_id`]); pass [`proto::NO_MEMBER_ID`] for positional
/// placement.
fn serve_auth_keys(
    listener: TcpListener,
    backend: Arc<dyn StorageBackend>,
    token: &str,
    fence: Arc<Fence>,
    keys: Option<Arc<crate::apikeys::ApiKeyStore>>,
    member_id: u64,
) {
    let token = token.to_string();
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let backend = backend.clone();
                let token = token.clone();
                let fence = fence.clone();
                let keys = keys.clone();
                thread::spawn(move || {
                    if let Err(e) = handle_conn(s, backend, &token, fence, keys, member_id) {
                        // A client hanging up is normal; log only real protocol errors.
                        if e.kind() != io::ErrorKind::UnexpectedEof {
                            eprintln!("conn error: {e}");
                        }
                    }
                });
            }
            Err(e) => eprintln!("accept failed: {e}"),
        }
    }
}

fn handle_conn(
    stream: TcpStream,
    backend: Arc<dyn StorageBackend>,
    token: &str,
    fence: Arc<Fence>,
    keys: Option<Arc<crate::apikeys::ApiKeyStore>>,
    member_id: u64,
) -> io::Result<()> {
    stream.set_nodelay(true)?;
    let mut reader = BufReader::new(stream.try_clone()?);
    // Keep the stream for writing; reader owns a clone.
    let mut writer = stream;

    // Mandatory handshake before any data-plane op is served. Empty server token
    // accepts any presented one, so clients that never set a secret keep working.
    let (op, body) = proto::recv_request(&mut reader)?;
    if op != proto::OP_OPEN {
        proto::send_response(&mut writer, EIO, 0, &[])?;
        return Ok(());
    }
    let mut i = 0usize;
    let presented = proto::get_key(&body, &mut i).unwrap_or_default();
    // Optional api key appended by newer clients (an old client omits it, which
    // only fails when the server actually demands one — same layout rule as the
    // meta plane's uid field).
    let api_key = proto::get_key(&body, &mut i).unwrap_or_default();
    if !token.is_empty() && presented != token {
        proto::send_response(
            &mut writer,
            EACCES,
            0,
            b"data-server rejected the access token",
        )?;
        return Ok(());
    }
    let mut authed: Option<crate::apikeys::AuthedKey> = None;
    if let Some(store) = &keys {
        match store.authenticate_mount(&api_key) {
            Ok(a) => {
                let _ = store.log_call(Some(&a), "data", "OP_OPEN", true, "");
                authed = Some(a);
            }
            Err(e) => {
                // Refuse, and say why in the payload so `mount` prints the actual
                // cause (missing key vs wrong secret vs revoked) instead of EACCES.
                let _ = store.log_call(None, "data", "OP_OPEN", false, &e);
                proto::send_response(&mut writer, EACCES, 0, e.as_bytes())?;
                return Ok(());
            }
        }
    }
    // Handshake ack. `VAL` carries this node's stable member id (`NO_MEMBER_ID`
    // when unconfigured) so a striping client can pin chunk placement to identity
    // instead of the caller's list order.
    proto::send_response(&mut writer, 0, member_id, &[])?; // handshake ack

    // Live-credential re-check: one store lookup per connection per
    // `KEY_RECHECK` window (a control-plane query, off the per-byte hot path),
    // so `grovefs apikey revoke` closes an ESTABLISHED data connection instead
    // of only refusing the next mount. The same tick refreshes the key's
    // CURRENT `ro` flag, so `apikey set-ro` hot-swaps content-write verdicts
    // without a reconnect. Only checked when traffic arrives: an
    // idle socket holds no access. The next client op sees the refusal and,
    // because its credential was accepted at setup, tears its mount down
    // cleanly (see [`super::signal_auth_loss`]).
    let mut next_check = std::time::Instant::now() + crate::apikeys::KEY_RECHECK;
    // A read-only key (`apikey create --ro`) may not write content either. The
    // meta plane already gated the tree by view binding; here the object keys
    // are content-addressed and view-agnostic, so the one credential axis the
    // data plane can enforce is read-only — a hard cap that holds even if a
    // client mounted without `-o ro`. EROFS mirrors the fencing gate (a
    // read-only verdict, not an auth loss, so the client keeps its mount).
    // Mutable because the re-check tick above re-reads it from the store.
    let mut key_ro = authed.as_ref().map(|a| a.ro).unwrap_or(false);

    loop {
        let (op, body) = match proto::recv_request(&mut reader) {
            Ok(v) => v,
            Err(e) => {
                // UnexpectedEof = client closed the pool socket; clean exit.
                if e.kind() == io::ErrorKind::UnexpectedEof {
                    return Ok(());
                }
                return Err(e);
            }
        };
        if let (Some(store), Some(a)) = (&keys, &authed) {
            if std::time::Instant::now() >= next_check {
                match store.live_state(&a.id, crate::apikeys::Scope::Mount) {
                    None => {
                        let why = "authentication failed: this api key has been revoked";
                        let _ = store.log_call(Some(a), "data", "OP_OPEN", false, why);
                        let _ = proto::send_response(&mut writer, EACCES, 0, why.as_bytes());
                        return Ok(());
                    }
                    // Still live: adopt the CURRENT ro flag (an `apikey set-ro`
                    // lands here, on the same ≤1s tick that carries a revoke).
                    Some(state) => key_ro = state.ro,
                }
                next_check = std::time::Instant::now() + crate::apikeys::KEY_RECHECK;
            }
        }
        // Fencing witness: fold the sender's epoch into the monotonic token and
        // echo back the value held BEFORE this claim (`prev`). Never touches the
        // storage backend; a monotonic CAS (plus optional persist) is all the
        // arbitration needs. Handled before the key-parsing dispatch.
        if op == proto::OP_FENCE {
            let mut fi = 0usize;
            let sender = proto::read_u64(&body, &mut fi).unwrap_or(0);
            let ttl = proto::read_u64(&body, &mut fi).unwrap_or(DEFAULT_LEASE_MS);
            // val = prev (old held token: the fence verdict for the caller);
            // data = [u64 granted_until_ms][u8 granted] carries the lease window.
            let (prev, until, granted) = fence.lease(sender, ttl);
            let mut d = Vec::with_capacity(9);
            d.extend_from_slice(&until.to_be_bytes());
            d.push(u8::from(granted));
            proto::send_response(&mut writer, 0, prev, &d)?;
            continue;
        }
        // Data-plane fencing gate. A write-intent op carries a leading
        // `[u64 epoch]` (the fence token its client learned from the primary).
        // If the arbiter's token is HIGHER, this client is aligned with a
        // since-fenced stale primary, so refuse the content write itself —
        // closing the window a plain write() would otherwise slip through by
        // bypassing the meta-plane gate. Inert while the token is 0 (no witness
        // has claimed), so non-fenced deployments are unaffected. On accept we
        // strip the 8-byte prefix so `dispatch` sees the unchanged body layout.
        //
        // The shared gate is held across BOTH the token check and the backend
        // write, so a concurrent promotion (which takes the gate exclusively)
        // cannot fold a higher epoch between them: an admitted write is
        // linearized entirely before, or entirely after, any promotion. This is
        // what turns "bounded window" into "no torn write" on this node.
        if proto::is_data_write(op) {
            if key_ro {
                proto::send_response(&mut writer, EROFS, 0, &[])?;
                continue;
            }
            if body.len() < 8 {
                proto::send_response(&mut writer, EIO, 0, &[])?;
                continue;
            }
            let epoch = u64::from_be_bytes(body[0..8].try_into().unwrap());
            let _gate = fence.write_gate();
            if fence.peek() > epoch {
                proto::send_response(&mut writer, EROFS, 0, &[])?;
                continue;
            }
            let (status, val, data) = dispatch(&*backend, op, &body[8..]);
            proto::send_response(&mut writer, status, val, &data)?;
            continue;
        }
        let (status, val, data) = dispatch(&*backend, op, &body);
        proto::send_response(&mut writer, status, val, &data)?;
    }
}

/// Execute one operation, returning (errno_status, scalar_value, payload).
fn dispatch(backend: &dyn StorageBackend, op: u8, body: &[u8]) -> (i32, u64, Vec<u8>) {
    // statfs has no key operand, so handle it before the shared key parse.
    if op == proto::OP_STATFS {
        return match backend.statvfs() {
            Ok(s) => {
                let mut d = Vec::with_capacity(56);
                for v in [
                    s.tblocks,
                    s.bfree,
                    s.bavail,
                    s.files,
                    s.ffree,
                    s.bsize,
                    s.namemax as u64,
                ] {
                    d.extend_from_slice(&v.to_be_bytes());
                }
                (0, 0, d)
            }
            Err(e) => (errno(&e), 0, Vec::new()),
        };
    }
    // Batch stat reads N keys from the body itself, so handle before the
    // shared single-key parse (same pattern as OP_STATFS above).
    if op == proto::OP_BATCH_LEN {
        let mut bi = 0usize;
        let count = match proto::read_u32(body, &mut bi) {
            Ok(n) => n as usize,
            Err(_) => return (EIO, 0, Vec::new()),
        };
        let mut data = Vec::with_capacity(count * 8);
        for _ in 0..count {
            let k = match proto::get_key(body, &mut bi) {
                Ok(k) => k,
                Err(_) => return (EIO, 0, Vec::new()),
            };
            let sz = backend.len(&k).unwrap_or(0);
            data.extend_from_slice(&sz.to_be_bytes());
        }
        let total: u64 = data
            .as_chunks::<8>()
            .0
            .iter()
            .map(|c| u64::from_be_bytes(*c))
            .sum();
        return (0, total, data);
    }

    // Enumeration is the third op with no single-key operand: the request
    // carries its own cursor, and the response carries a whole page.
    if op == proto::OP_LIST {
        let mut li = 0usize;
        let limit = match proto::read_u32(body, &mut li) {
            Ok(n) => (n as usize).min(crate::backend::LIST_LIMIT_MAX),
            Err(_) => return (EIO, 0, Vec::new()),
        };
        let cursor = match proto::get_key(body, &mut li) {
            Ok(c) => c,
            Err(_) => return (EIO, 0, Vec::new()),
        };
        let cur = if cursor.is_empty() {
            None
        } else {
            Some(cursor.as_str())
        };
        return match backend.list_objects(cur, limit) {
            Ok(page) => {
                let mut d = Vec::with_capacity(4 + page.rows.len() * 72);
                proto::put_u32(&mut d, page.rows.len() as u32);
                for r in &page.rows {
                    proto::put_key(&mut d, &r.key);
                    proto::put_u64(&mut d, r.size);
                    proto::put_u64(&mut d, r.mtime_secs as u64);
                }
                proto::put_key(&mut d, page.next.as_deref().unwrap_or(""));
                (0, page.rows.len() as u64, d)
            }
            // A backend that cannot enumerate says so with a distinct errno
            // rather than a generic EIO: "this node's storage cannot be walked"
            // is a different fact from "the walk failed", and an auditor must
            // refuse to draw a verdict in either case.
            Err(e) if e.kind() == io::ErrorKind::Unsupported => (ENOSYS, 0, Vec::new()),
            Err(e) => (errno(&e), 0, Vec::new()),
        };
    }

    let mut i = 0usize;
    let key = match proto::get_key(body, &mut i) {
        Ok(k) => k,
        Err(_) => return (EIO, 0, Vec::new()),
    };
    match op {
        proto::OP_CREATE => err_only(backend.create(&key)),
        proto::OP_LEN => match backend.len(&key) {
            Ok(n) => (0, n, Vec::new()),
            Err(e) => (errno(&e), 0, Vec::new()),
        },
        proto::OP_FSYNC => err_only(backend.fsync(&key)),
        proto::OP_REMOVE => err_only(backend.remove(&key)),
        proto::OP_TRUNC => {
            let new_len = proto::read_u64(body, &mut i).unwrap_or(0);
            err_only(backend.truncate(&key, new_len))
        }
        proto::OP_READ => {
            let offset = proto::read_u64(body, &mut i).unwrap_or(0);
            let len = proto::read_u32(body, &mut i).unwrap_or(0) as usize;
            match backend.read(&key, offset, len) {
                Ok(data) => {
                    let n = data.len() as u64;
                    (0, n, data)
                }
                Err(e) => (errno(&e), 0, Vec::new()),
            }
        }
        proto::OP_WRITE => {
            let offset = match proto::read_u64(body, &mut i) {
                Ok(o) => o,
                Err(_) => return (EIO, 0, Vec::new()),
            };
            // Remaining bytes are the write payload (avoid one copy).
            let data = &body[i..];
            match backend.write(&key, offset, data) {
                Ok(n) => (0, n as u64, Vec::new()),
                Err(e) => (errno(&e), 0, Vec::new()),
            }
        }
        _ => (ENOSYS, 0, Vec::new()),
    }
}

fn err_only(r: io::Result<()>) -> (i32, u64, Vec<u8>) {
    match r {
        Ok(()) => (0, 0, Vec::new()),
        Err(e) => (errno(&e), 0, Vec::new()),
    }
}

fn errno(e: &io::Error) -> i32 {
    e.raw_os_error().unwrap_or(EIO)
}

#[cfg(test)]
mod tests {
    use super::Fence;

    /// A fresh claim wins: `prev` (0) < sender, and the token advances.
    #[test]
    fn claim_returns_prev_and_advances() {
        let f = Fence::new_in_memory();
        assert_eq!(f.claim(1), 0); // won: nobody was at/above 1
        assert_eq!(f.claim(2), 1); // won: prev 1 < 2
        assert_eq!(f.peek(), 2);
    }

    /// Re-claiming our own epoch is a monotonic no-op (prev == sent); a primary
    /// holds the top when `prev > mine` is false.
    #[test]
    fn reclaim_is_monotonic_noop() {
        let f = Fence::new_in_memory();
        assert_eq!(f.claim(5), 0);
        assert_eq!(f.claim(5), 5); // prev == 5, token unchanged
        assert_eq!(f.peek(), 5);
    }

    /// The tie the post-fold-max protocol could not distinguish: two nodes at
    /// epoch 4 both promote to candidate 5. The first wins (`prev 4 < 5`); the
    /// second sees `prev == 5` and is fenced. This is exactly what prevents a
    /// double-primary when an arbiter recovers and both halves race to promote.
    #[test]
    fn tie_fences_the_later_claimer() {
        let f = Fence::new_in_memory();
        assert_eq!(f.claim(4), 0); // node holds epoch 4
        let a = f.claim(5); // A promotes to 5
        let b = f.claim(5); // B tries the same 5
        assert!(a < 5, "first claimer of 5 wins (prev {a} < 5)");
        assert!(b >= 5, "second claimer of 5 is fenced (prev {b} >= 5)");
    }

    /// A stale/lower claim can never drag the token backwards.
    #[test]
    fn stale_claim_does_not_regress() {
        let f = Fence::new_in_memory();
        assert_eq!(f.claim(9), 0);
        assert_eq!(f.claim(3), 9); // stale node re-claims 3 -> sees prev 9 (fenced)
        assert_eq!(f.peek(), 9);
    }

    /// Durability: a path-backed token survives a reload (simulating a witness
    /// restart), so a deposed primary cannot reclaim the old low epoch, and the
    /// token never resets to 0.
    #[test]
    fn durable_token_survives_reload() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "grovefs_fence_test_{}_{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&path);
        {
            let f = Fence::with_store(path.clone());
            assert_eq!(f.claim(7), 0);
        }
        // "Restart": a brand-new Fence reads the persisted value, not 0.
        let f2 = Fence::with_store(path.clone());
        assert_eq!(f2.peek(), 7);
        assert_eq!(f2.claim(7), 7); // stale primary re-claims 7 -> prev 7, not a win
        assert_eq!(f2.claim(3), 7); // lower epoch fully fenced
        let _ = std::fs::remove_file(&path);
    }

    /// The drain gate: a promotion claim must wait until every in-flight
    /// write-intent op (holding the gate shared across its check+write) has
    /// finished before it folds a higher epoch. This is what guarantees a
    /// stale-epoch write can never land on the backend *during* a promotion —
    /// the torn-write window at its implementation root.
    #[test]
    fn claim_drains_inflight_writes_before_promoting() {
        use std::sync::mpsc;
        use std::time::Duration;
        let f = std::sync::Arc::new(Fence::new_in_memory());
        assert_eq!(f.claim(5), 0); // node holds epoch 5

        // Simulate an in-flight write: hold the gate shared, as handle_conn does
        // across its epoch check + backend dispatch.
        let held = f.write_gate();

        // A concurrent promotion runs on another thread and signals completion.
        let (tx, rx) = mpsc::channel::<u64>();
        let f2 = f.clone();
        let promoter = std::thread::spawn(move || {
            let prev = f2.claim(6);
            let _ = tx.send(prev);
        });

        // While the write gate is held, the claim MUST be blocked draining it —
        // it cannot have returned yet.
        assert!(
            rx.recv_timeout(Duration::from_millis(120)).is_err(),
            "promotion must wait for the in-flight write to drain"
        );

        // Release the "write"; now the promotion may proceed.
        drop(held);
        let prev = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("claim proceeds after drain");
        promoter.join().unwrap();
        assert_eq!(prev, 5, "won the fold: prev 5 < 6, token now 6");
        assert_eq!(f.peek(), 6);
    }

    /// Non-overlap is the whole point of the lease: while one holder's lease is
    /// live the arbiter refuses to hand the token to a strictly-higher claimant,
    /// so a primary can trust a locally cached lease instead of re-proving per
    /// write. A same-epoch claim renews; a lower one is fenced.
    #[test]
    fn lease_blocks_overlapping_promotion_until_expiry() {
        let f = Fence::new_in_memory();
        // Cold start: first grant at epoch 5 (cur was 0).
        let (prev, _until, granted) = f.lease(5, 60_000);
        assert_eq!((prev, granted), (0, true), "cold grant at epoch 5");
        assert_eq!(f.peek(), 5);
        // A competing higher epoch is REFUSED while the 5 lease is live — the
        // arbiter cannot have two live holders at once.
        let (prev, _, granted) = f.lease(6, 60_000);
        assert_eq!(
            (prev, granted),
            (5, false),
            "promotion blocked by live lease"
        );
        assert_eq!(f.peek(), 5, "token unchanged by the refused promotion");
        // The holder renews its own epoch: granted, token stays 5.
        let (prev, _, granted) = f.lease(5, 60_000);
        assert_eq!((prev, granted), (5, true), "renew own lease");
        // A stale lower epoch is fenced outright.
        let (prev, _, granted) = f.lease(4, 60_000);
        assert_eq!((prev, granted), (5, false), "stale epoch fenced");
    }

    /// With a zero-length lease the incumbent expires immediately, so the next
    /// higher claim is granted — demonstrating the expiry path deterministically
    /// without sleeping.
    #[test]
    fn lease_promotes_once_incumbent_expired() {
        let f = Fence::new_in_memory();
        assert!(f.lease(5, 0).2, "cold grant");
        let (prev, _, granted) = f.lease(6, 60_000);
        assert_eq!(
            (prev, granted),
            (5, true),
            "expired lease lets the promote through"
        );
        assert_eq!(f.peek(), 6);
    }
}

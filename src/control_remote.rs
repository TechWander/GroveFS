//! The REMOTE control plane: governance over the wire instead of a database
//! connection on every operator machine.
//!
//! Motivation: under PostgreSQL the only way to govern a deployment used to be
//! `--meta-store postgres://user:pw@…`, i.e. every management host held the
//! database password and reached the store directly. That credential cannot be
//! scoped per person, cannot be revoked without rotating the DB role, and
//! leaves no per-call audit. Under SQLite there was no remote path at all: the
//! trees are local files, so governing them meant being on the disk's host.
//!
//! This module closes both: a `meta-server --control-listen HOST:PORT` exposes
//! the SAME governance implementation it already uses internally
//! ([`crate::mcp::ControlStore`]) over the existing framed transport, gated by
//! an [`Scope::Admin`](crate::apikeys::Scope) API key. An operator on any
//! machine presents `<id>:<secret>` and the server does the work — it stays the
//! only process holding a PG connection or a `<meta-dir>`.
//!
//! Properties this buys, in order of importance:
//! - **The store credential never leaves the server.** A stolen operator laptop
//!   yields a revocable key, not a database superuser.
//! - **Every governance call is attributable.** The server appends one
//!   `mcp_calls` row per call (`method="control"`), with the account/label of
//!   the key that made it.
//! - **SQLite becomes remotely manageable** without putting `meta/` on a shared
//!   filesystem (WAL over NFS is unsafe, and the single-writer assumption would
//!   be broken by a second host).
//!
//! Deliberately NOT here: `gc` (it walks byte roots and requires an offline
//! quiesce the control plane cannot prove). It still runs on the host that owns
//! the bytes, and every entry point that cannot be served remotely says so by
//! name rather than doing something half-right. `fork` and `graft` ARE served
//! here: they only copy metadata inside the store the server already owns, and
//! every backend can now do both (SQLite rows or a PG schema).
//! `export`/`import` are served here too — the operator's `--out`/`--in` stays a
//! path on the operator's machine while the dump travels in line-aligned chunks
//! through the server's staging area. What crosses the wire is METADATA only
//! (`data_ref` keys), never content bytes: a tree restored on another machine
//! still needs the store's `blocks/` (or a data-server) under it, and the CLI
//! says so out loud rather than implying a migration copied the bytes.
//!
//! The client half is a plain [`crate::mcp::ControlStore`] implementation, so
//! the CLI, this file and the MCP server share ONE governance surface: nothing
//! above the trait knows whether it is talking to files, PostgreSQL, or a
//! remote meta-server.

use std::io::BufReader;
use std::net::TcpStream;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::mcp::ControlStore;
use crate::meta::{ForkStats, GraftItem, GraftStats, MetaProvider};
use crate::remote::proto;

/// A governance client bound to one `meta-server --control-listen` address and
/// one admin credential.
#[derive(Clone)]
pub struct ControlClient {
    addr: String,
    key: String,
    /// The web console's per-call attribution: `(account, ip)` of whoever is
    /// behind this client, `None` for an operator running a command themselves.
    /// It travels in the request body and is the ONLY thing the server's ACL
    /// can judge a delegated user by — see [`ControlClient::with_actor`].
    actor: Option<(String, String)>,
    /// Live connections, for a caller that outlives its first request.
    /// `None` is the classic one-connection-per-call exchange.
    pool: Option<Arc<GovPool>>,
}

impl ControlClient {
    pub fn new(addr: &str, key: &str) -> Self {
        Self {
            addr: addr.to_string(),
            key: key.to_string(),
            actor: None,
            pool: None,
        }
    }

    /// A client that REUSES its control connections, up to `max` of them, for as
    /// long as the process lives. Only a long-lived caller should ask for this
    /// (see [`GovPool`] for what a pooled request stops paying for); a CLI would
    /// just be naming a socket it uses once and exits with.
    pub fn pooled(addr: &str, key: &str, max: usize) -> Self {
        Self {
            pool: Some(Arc::new(GovPool {
                max: max.max(1),
                idle: Mutex::new(Vec::new()),
            })),
            ..Self::new(addr, key)
        }
    }

    /// Speak for a signed-in console account: every call this client makes
    /// carries `account` + `ip`, so the server (a) authorizes it against that
    /// account's delegations instead of the key's full rights, and (b) writes
    /// the audit row as that person rather than as "whoever holds the key".
    ///
    /// The credential stays the admin key, and that is the honest shape of it:
    /// the console is one trusted process holding the deployment's governance
    /// credential, and what this field adds is per-user LIMITATION and
    /// ATTRIBUTION, not a second authentication factor. Anyone who can read the
    /// key can name any account — and can already do everything the key allows,
    /// which is why the key must not leave the gateway's host.
    ///
    /// Cloned, not mutated in place: the gateway derives one client per request
    /// from a shared template, so a session's identity cannot leak into another.
    /// A pool, if the template has one, is shared by those clones (it lives in an
    /// `Arc`) — deriving a client per request must not derive a connection per
    /// request, which is the whole point of having a pool.
    pub fn with_actor(mut self, account: &str, ip: &str) -> Self {
        self.actor = Some((account.to_string(), ip.to_string()));
        self
    }

    pub fn addr(&self) -> &str {
        &self.addr
    }

    /// One governance call. `op` is a dotted verb (`def.list`, `view.create`,
    /// `apikey.create`, …); `args` is the op's JSON object. Errors are the
    /// server's own refusal text (unknown/revoked key, wrong scope, a governance
    /// refusal), never a bare status code — the point of the envelope.
    pub fn call(&self, op: &str, args: Value) -> Result<Value, String> {
        match &self.pool {
            Some(pool) => pool.call(&self.addr, &self.key, op, args, self.actor.as_ref()),
            None => one_shot(&self.addr, &self.key, op, args, self.actor.as_ref()),
        }
    }
}

/// The classic exchange: connect, one request, one reply, socket gone. This is
/// what a caller without a pool does, and what every client did before
/// [`GovPool`] existed — the server answers it identically.
fn one_shot(
    addr: &str,
    key: &str,
    op: &str,
    args: Value,
    actor: Option<&(String, String)>,
) -> Result<Value, String> {
    let body = request_body(Some(key), false, op, args, actor)?;
    let mut conn = GovConn::open(addr)?;
    let rep = conn.round_trip(addr, &body)?;
    // The socket is dropped whether or not the server would have kept it: a
    // one-shot caller means by "one call" exactly that.
    rep.out
}

/// The request envelope: the verb, the op's args, the credential when the
/// connection does not already carry one, and the console's per-user
/// attribution. `pin` is only ever sent as `true`, so what a one-shot caller
/// puts on the wire is what it always put there.
fn request_body(
    key: Option<&str>,
    pin: bool,
    op: &str,
    args: Value,
    actor: Option<&(String, String)>,
) -> Result<Vec<u8>, String> {
    let mut req = json!({ "op": op, "a": args });
    if let Some(k) = key {
        req["k"] = json!(k);
    }
    if pin {
        req["pin"] = json!(true);
    }
    if let Some((account, ip)) = actor {
        req["account"] = json!(account);
        req["ip"] = json!(ip);
    }
    serde_json::to_vec(&req).map_err(|e| format!("cannot encode control request: {e}"))
}

/// How long this side keeps an idle connection before reopening it rather than
/// write into a socket the server may already have given up on. The server's own
/// idle bound is an order of magnitude longer, so this reopen is a choice and
/// not a failed request.
const POOL_IDLE: Duration = Duration::from_secs(60);

/// Reusable control connections, for a caller that lives long enough to make
/// more than one call per process.
///
/// One connection per request is right for a CLI: the process is about to exit,
/// so the connection it just made is the only one it will ever need. It is wrong
/// for a web gateway whose worker threads turn browser clicks into governance
/// calls all day — every one of those paid a TCP connect, a thread spawn on the
/// server, and the two store queries of an authentication (the `api_keys` SELECT
/// plus its `last_used_at` UPDATE). This holds the socket instead, and the
/// server holds the credential with it: a request on an established connection
/// does not present the key at all.
///
/// What is deliberately NOT skipped per call: the audit row (one per request,
/// forever), the console account's delegation lookup (so a withdrawn grant bites
/// on the next click rather than the next reconnect), and — on the server's own
/// [`crate::apikeys::KEY_RECHECK`] tick — the credential's liveness, so a revoke
/// reaches a pooled connection within a second of arriving.
struct GovPool {
    /// Ceiling on pooled connections, which is also the ceiling on server
    /// threads this client can occupy. Never exceeded: a connection is taken out
    /// of the pool under the lock before it is used, and a return to a full pool
    /// closes the socket instead. `checkin` decides under the same lock it takes
    /// to push, so two workers can never both believe one slot is left.
    max: usize,
    idle: Mutex<Vec<GovConn>>,
}

/// One live connection the server has pinned our credential to.
struct GovConn {
    /// Read through a buffer: with a socket that survives past one exchange,
    /// over-reading is normal, and the bytes belong to the next reply on THIS
    /// connection. Writes go to a separate handle so nothing buffered can be
    /// reordered behind them.
    reader: BufReader<TcpStream>,
    writer: TcpStream,
    /// When it came back to the pool, against [`POOL_IDLE`].
    idle_since: Instant,
}

/// One governance reply unpacked: what the op returned (or the server's own
/// refusal sentence), and whether the server will answer another request on this
/// socket. A refusal with `keep == true` is a NORMAL outcome — the op said no,
/// the connection did not — so a pooled caller must not discard the socket over
/// it.
struct GovReply {
    out: Result<Value, String>,
    keep: bool,
}

impl GovConn {
    /// Connect and split for full-duplex framing.
    fn open(addr: &str) -> Result<Self, String> {
        let sock = TcpStream::connect(addr).map_err(|e| {
            format!(
                "cannot reach the control plane at {addr}: {e} — is `meta-server --control-listen {addr}` running?",
            )
        })?;
        let _ = sock.set_nodelay(true);
        let reader = BufReader::new(
            sock.try_clone()
                .map_err(|e| format!("control socket: {e}"))?,
        );
        Ok(Self {
            reader,
            writer: sock,
            idle_since: Instant::now(),
        })
    }

    /// Write one request, read its reply. The failure text distinguishes the two
    /// halves because "the server never answered" and "the server answered
    /// something I cannot parse" are different incidents.
    fn round_trip(&mut self, addr: &str, body: &[u8]) -> Result<GovReply, String> {
        proto::send_request(&mut self.writer, proto::OP_CONTROL, body)
            .map_err(|e| format!("control request failed: {e}"))?;
        let (status, _val, data) = proto::recv_response(&mut self.reader)
            .map_err(|e| format!("control reply failed: {e}"))?;
        Ok(reply_of(status, &data, addr))
    }
}

/// Unpack one governance reply. Transport status first (a non-zero STATUS is the
/// server saying it could not even frame an answer), then the `{ok,r}` /
/// `{ok:false,e}` envelope. The `pinned` field is the server's reuse answer and
/// is only ever sent as `true`: its absence means one request per socket, which
/// is what an older server does.
fn reply_of(status: i32, data: &[u8], addr: &str) -> GovReply {
    if status != 0 {
        return GovReply {
            out: Err(String::from_utf8_lossy(data).into_owned()),
            keep: false,
        };
    }
    let env: Value = match serde_json::from_slice(data) {
        Ok(v) => v,
        Err(e) => {
            return GovReply {
                out: Err(format!("malformed control reply from {addr}: {e}")),
                keep: false,
            }
        }
    };
    let keep = env.get("pinned").and_then(Value::as_bool) == Some(true);
    let out = if env.get("ok").and_then(Value::as_bool) == Some(true) {
        Ok(env.get("r").cloned().unwrap_or(Value::Null))
    } else {
        Err(env
            .get("e")
            .and_then(Value::as_str)
            .unwrap_or("the control plane refused the request")
            .to_string())
    };
    GovReply { out, keep }
}

impl GovPool {
    /// Borrow a connection, opening one when the pool is empty or every idle
    /// socket has aged out. The returned flag says which happened, and a new
    /// socket is the only one that has to present the credential.
    fn checkout(&self, addr: &str) -> Result<(GovConn, bool), String> {
        loop {
            let candidate = self.idle.lock().unwrap_or_else(|e| e.into_inner()).pop();
            match candidate {
                Some(c) if c.idle_since.elapsed() < POOL_IDLE => return Ok((c, false)),
                // Aged out: closed here rather than written into. Nothing was
                // ever in flight on an idle socket, so this can lose a request
                // no more than a reconnect can.
                Some(c) => drop(c),
                None => return Ok((GovConn::open(addr)?, true)),
            }
        }
    }

    /// Hand a connection back, or close it. Only a socket the server said `keep`
    /// about ever reaches this.
    fn checkin(&self, mut c: GovConn) {
        c.idle_since = Instant::now();
        let mut q = self.idle.lock().unwrap_or_else(|e| e.into_inner());
        if q.len() < self.max {
            q.push(c);
        }
    }

    fn call(
        &self,
        addr: &str,
        key: &str,
        op: &str,
        args: Value,
        actor: Option<&(String, String)>,
    ) -> Result<Value, String> {
        let (mut conn, fresh) = self.checkout(addr)?;
        // A socket the server already pinned does not get the credential again;
        // a new one presents it and asks to be kept.
        let body = request_body(if fresh { Some(key) } else { None }, true, op, args, actor)?;
        match conn.round_trip(addr, &body) {
            Ok(rep) => {
                // `keep` is the server's answer, not our guess: false for an
                // older server, for a session at its request bound, and for any
                // exchange after which the identity stopped being usable.
                if rep.keep {
                    self.checkin(conn);
                }
                rep.out
            }
            Err(e) => {
                // NEVER retried on a fresh socket. `view.create`, `grant.add`
                // and `apikey revoke` are not idempotent: resending a request
                // whose reply was lost can apply the same change twice. The
                // caller gets the transport failure and decides; the socket is
                // dropped here, so its next call starts clean.
                Err(e)
            }
        }
    }
}

/// One transfer message's payload. Overridable (`GROVEFS_XFER_CHUNK`) so a test
/// can force the multi-chunk path, and so a link with a tight budget can shrink
/// it; the server clamps to the same ceiling regardless.
fn xfer_chunk() -> usize {
    std::env::var("GROVEFS_XFER_CHUNK")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|n| n.clamp(1, 64 << 20) as usize)
        .unwrap_or(4 << 20)
}

impl ControlClient {
    /// Pull one view's metadata dump from the server into `out` on THIS machine.
    ///
    /// The dump is rendered on the server (only it can read the tree) and then
    /// pulled in line-aligned chunks, so neither side ever holds the whole thing
    /// in memory and a chunk never splits a row or a UTF-8 sequence. It is staged
    /// beside `--out` and renamed at the end: an aborted transfer leaves no half
    /// dump behind at the requested path — the same rule the local writers keep.
    pub fn export_view(
        &self,
        view: &str,
        out: &Path,
        force: bool,
    ) -> Result<crate::meta::ViewExportStats, String> {
        if out.exists() && !force {
            return Err(format!(
                "output {} already exists — pass --force to overwrite it",
                out.display()
            ));
        }
        let begin = self.call("view.export", json!({"view": view}))?;
        let handle = as_str(begin.get("handle").unwrap_or(&Value::Null), "export handle")?;
        let nodes = count_field(&begin, "nodes")?;
        let blobs = count_field(&begin, "blobs")?;
        let kind = begin
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("physical")
            .to_string();
        let mut tmp = out.to_path_buf();
        let name = out.file_name().and_then(|n| n.to_str()).unwrap_or("export");
        tmp.set_file_name(format!(".{name}.tmp-{}", std::process::id()));
        let chunk = xfer_chunk();
        let pulled = (|| -> Result<(), String> {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)
                .map_err(|e| format!("cannot create {}: {e}", tmp.display()))?;
            let mut off = 0u64;
            loop {
                let r = self.call(
                    "view.export.chunk",
                    json!({"handle": handle, "offset": off, "max": chunk}),
                )?;
                let text = as_str(r.get("text").unwrap_or(&Value::Null), "chunk text")?;
                f.write_all(text.as_bytes())
                    .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
                let next = r.get("next").and_then(Value::as_u64).unwrap_or(0);
                let eof = r.get("eof").and_then(Value::as_bool).unwrap_or(true);
                if eof {
                    break;
                }
                if next <= off {
                    return Err(format!(
                        "malformed chunk reply: offset {off} did not advance"
                    ));
                }
                off = next;
            }
            f.flush()
                .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
            f.sync_all()
                .map_err(|e| format!("cannot flush {}: {e}", tmp.display()))
        })();
        // The staged file lives on the server, so it is released either way —
        // success, a short read, or a dropped connection.
        let _ = self.call("view.export.done", json!({"handle": handle}));
        if let Err(e) = pulled {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        std::fs::rename(&tmp, out).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("cannot move export into {}: {e}", out.display())
        })?;
        Ok(crate::meta::ViewExportStats { nodes, blobs, kind })
    }

    /// Push a local dump to the server and install it as `view` there.
    ///
    /// Chunks are cut at line boundaries and must arrive in order (the server
    /// checks the running length against the offset), so a retry that landed
    /// twice or a chunk that went missing is refused instead of producing a
    /// half-parsed tree. `--as-fork` is recorded BY THE SERVER, in the same
    /// verdict: a definition it cannot write tears the imported tree down again.
    pub fn import_view(
        &self,
        view: &str,
        input: &Path,
        replace: bool,
        as_fork: Option<&str>,
    ) -> Result<crate::meta::ViewImportStats, String> {
        let mut f = std::fs::File::open(input)
            .map_err(|e| format!("cannot open {}: {e}", input.display()))?;
        let begin = self.call("view.import.begin", json!({}))?;
        let handle = as_str(begin.get("handle").unwrap_or(&Value::Null), "import handle")?;
        let chunk = xfer_chunk();
        let pushed = (|| -> Result<(), String> {
            use std::io::Read;
            let mut carry: Vec<u8> = Vec::with_capacity(chunk);
            let mut eof = false;
            let mut off = 0u64;
            loop {
                while carry.len() < chunk && !eof {
                    let mut piece = vec![0u8; chunk - carry.len()];
                    let n = f
                        .read(&mut piece)
                        .map_err(|e| format!("cannot read {}: {e}", input.display()))?;
                    if n == 0 {
                        eof = true;
                        break;
                    }
                    carry.extend_from_slice(&piece[..n]);
                }
                if carry.is_empty() {
                    break;
                }
                // Send whole lines. A single line longer than the window goes
                // in one piece: the server appends blindly and the parser is
                // the only thing that reads it.
                let mut cut = carry.len();
                if !eof {
                    if let Some(p) = carry.iter().rposition(|&b| b == b'\n') {
                        cut = p + 1;
                    }
                }
                let text = std::str::from_utf8(&carry[..cut]).map_err(|e| {
                    format!("{}: not valid UTF-8 at byte {off} ({e})", input.display())
                })?;
                let r = self.call(
                    "view.import.put",
                    json!({"handle": handle, "offset": off, "text": text}),
                )?;
                off = r
                    .get("size")
                    .and_then(Value::as_u64)
                    .unwrap_or(off + cut as u64);
                carry.drain(..cut);
                if eof {
                    break;
                }
            }
            Ok(())
        })();
        if let Err(e) = pushed {
            let _ = self.call("view.import.abort", json!({"handle": handle}));
            return Err(e);
        }
        let r = match self.call(
            "view.import.commit",
            json!({"view": view, "handle": handle, "replace": replace, "as_fork": as_fork}),
        ) {
            Ok(r) => r,
            Err(e) => {
                let _ = self.call("view.import.abort", json!({"handle": handle}));
                return Err(e);
            }
        };
        Ok(crate::meta::ViewImportStats {
            nodes: count_field(&r, "nodes")?,
            blobs: count_field(&r, "blobs")?,
            kind: r
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("physical")
                .to_string(),
            replaced: r.get("replaced").and_then(Value::as_bool).unwrap_or(false),
        })
    }
}

fn as_str(v: &Value, what: &str) -> Result<String, String> {
    v.as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("malformed control reply: {what} is not a string"))
}

fn as_str_list(v: &Value, what: &str) -> Result<Vec<String>, String> {
    v.as_array()
        .ok_or_else(|| format!("malformed control reply: {what} is not an array"))?
        .iter()
        .map(|x| as_str(x, what))
        .collect()
}

/// A count from a stats reply. Missing or negative is a protocol error, never
/// a zero: reporting "0 shared objects" for a reply the client did not
/// understand would read as a successful fork that copied nothing.
fn count_field(v: &Value, what: &str) -> Result<i64, String> {
    v.get(what)
        .and_then(Value::as_i64)
        .ok_or_else(|| format!("malformed control reply: '{what}' is not a count"))
}

/// The remote implementation of [`ControlStore`]: every call is one request to
/// the meta-server's control port. Cheap to clone (an `Arc` inside), because
/// `store_for_db_path` hands clones out.
#[derive(Clone)]
pub struct RemoteControlStore {
    ctl: Arc<ControlClient>,
    /// The control plane's backend name, as reported by `info` at construction
    /// (`"postgres"`/`"sqlite"`) — surfaced in `view list` and
    /// tool JSON so an operator can tell what the server actually governs.
    backend: &'static str,
}

impl RemoteControlStore {
    /// Connect-time probe: refuses early (with the server's own wording) when
    /// the address is unreachable or the key is not an admin key, instead of
    /// letting every later call fail one by one.
    pub fn connect(addr: &str, key: &str) -> Result<Self, String> {
        let ctl = ControlClient::new(addr, key);
        let info = ctl.call("info", json!({}))?;
        let name = info
            .get("backend")
            .and_then(Value::as_str)
            .unwrap_or("remote");
        // Leaked once per client construction: the set of possible names is a
        // closed enum in practice, so this is not a per-call allocation.
        let backend: &'static str = match name {
            "postgres" => "postgres",
            "sqlite" => "sqlite",
            _ => "remote",
        };
        Ok(Self {
            ctl: Arc::new(ctl),
            backend,
        })
    }

    pub fn client(&self) -> &ControlClient {
        &self.ctl
    }
}

impl ControlStore for RemoteControlStore {
    fn backend(&self) -> &'static str {
        self.backend
    }

    fn describe(&self) -> String {
        format!(
            "remote control plane at {} (backend {})",
            self.ctl.addr(),
            self.backend
        )
    }

    fn list_defs(&self) -> Result<Vec<(String, String)>, String> {
        let r = self.ctl.call("def.list", json!({}))?;
        let arr = r
            .as_array()
            .ok_or_else(|| "malformed control reply: def.list is not an array".to_string())?;
        let mut out = Vec::with_capacity(arr.len());
        for ent in arr {
            let pair = ent
                .as_array()
                .ok_or_else(|| "malformed control reply: def.list entry".to_string())?;
            if pair.len() != 2 {
                return Err("malformed control reply: def.list entry".to_string());
            }
            out.push((as_str(&pair[0], "def name")?, as_str(&pair[1], "def text")?));
        }
        Ok(out)
    }

    fn get_def_raw(&self, name: &str) -> Result<Option<String>, String> {
        let r = self.ctl.call("def.get", json!({"name": name}))?;
        if r.is_null() {
            Ok(None)
        } else {
            as_str(&r, "def text").map(Some)
        }
    }

    fn put_def(&self, name: &str, text: &str) -> Result<(), String> {
        self.ctl
            .call("def.put", json!({"name": name, "text": text}))?;
        Ok(())
    }

    fn remove_def(&self, name: &str) -> Result<bool, String> {
        Ok(self
            .ctl
            .call("def.remove", json!({"name": name}))?
            .as_bool()
            .unwrap_or(false))
    }

    fn stamp(&self, name: &str) -> Option<String> {
        self.ctl
            .call("stamp", json!({"name": name}))
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
    }

    fn list_physical(&self) -> Result<Vec<String>, String> {
        as_str_list(&self.ctl.call("physical.list", json!({}))?, "physical.list")
    }

    fn has_physical(&self, name: &str) -> Result<bool, String> {
        Ok(self
            .ctl
            .call("physical.has", json!({"name": name}))?
            .as_bool()
            .unwrap_or(false))
    }

    /// A remote store is bound to its server: `--meta-dir` selects a LOCAL
    /// tree and means nothing here, so the override is refused by name instead
    /// of being silently ignored (which would make `view list --meta-dir`
    /// answer about a store the deployment never sees).
    fn store_for_db_path(&self, db: &Path) -> Arc<dyn ControlStore> {
        let _ = db;
        Arc::new(self.clone())
    }

    fn has_fork_tree(&self, name: &str) -> Result<bool, String> {
        Ok(self
            .ctl
            .call("fork.has", json!({"name": name}))?
            .as_bool()
            .unwrap_or(false))
    }

    fn create_fork_tree(&self, base: &str, name: &str, seed: bool) -> Result<ForkStats, String> {
        // The server performs the copy; a backend that cannot fork answers with
        // its own refusal text, which is what the caller prints. Nothing is
        // re-derived here — the point of the control plane is ONE verdict.
        let r = self.ctl.call(
            "fork.create",
            json!({"base": base, "name": name, "seed": seed}),
        )?;
        Ok(ForkStats {
            nodes: count_field(&r, "nodes")?,
            blobs: count_field(&r, "blobs")?,
        })
    }

    fn graft_tree(
        &self,
        dst: &str,
        src: &str,
        items: &[GraftItem],
        dest: &str,
        skip_existing: bool,
    ) -> Result<GraftStats, String> {
        // The plan travels as data (it was already parsed from the operator's
        // file), so a malformed entry is refused by the server, not skipped.
        let wire: Vec<Value> = items
            .iter()
            .map(|i| json!({"src": i.src, "dst": i.dst}))
            .collect();
        let r = self.ctl.call(
            "fork.graft",
            json!({
                "dst": dst,
                "src": src,
                "items": wire,
                "dest": dest,
                "skip_existing": skip_existing,
            }),
        )?;
        Ok(GraftStats {
            nodes: count_field(&r, "nodes")?,
            dirs: count_field(&r, "dirs")?,
            blobs: count_field(&r, "blobs")?,
            skipped: count_field(&r, "skipped")?,
        })
    }

    /// Best-effort rollback of a half-made fork (the tree exists but its
    /// definition could not be written). The tree lives on the SERVER, so the
    /// rollback has to travel too; a failure here is deliberately swallowed —
    /// the caller is already reporting the definition error and a second one
    /// would bury it.
    fn destroy_fork_tree(&self, name: &str) {
        let _ = self.ctl.call("fork.drop", json!({"name": name}));
    }

    fn drop_fork_tree(&self, name: &str) -> Result<(), String> {
        self.ctl.call("fork.drop", json!({"name": name}))?;
        Ok(())
    }

    fn open_ro_view(&self, _view: &str) -> Result<Arc<dyn MetaProvider>, String> {
        Err(
            "opening a view tree on the operator machine is not a control-plane operation — \
             use `grovefs mount --manager …` or `grovefs du --meta-server …`"
                .into(),
        )
    }

    fn base_tree_missing_note(&self, _view: &str) -> Result<Option<String>, String> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// What the envelope looks like on the wire. The first case is the one that
    /// is worth a test: a caller that does not ask to be pinned must put
    /// EXACTLY the body it always put there, `pin` absent. The moment that
    /// changes, every CLI in the repo starts holding server threads open
    /// without anyone deciding it should.
    #[test]
    fn a_one_shot_request_is_the_classic_envelope() {
        let body = request_body(
            Some("abc:secret"),
            false,
            "view.list",
            json!({"x": 1}),
            None,
        )
        .unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["k"], json!("abc:secret"));
        assert_eq!(v["op"], json!("view.list"));
        assert_eq!(v["a"], json!({"x": 1}));
        assert!(
            v.get("pin").is_none(),
            "a one-shot caller must not ask to be pinned: {v}"
        );
        assert!(v.get("account").is_none(), "no actor, no attribution: {v}");
    }

    /// …and a pinned one asks to be kept while still naming the console user,
    /// because the ACL and the audit row are judged by THAT and not by the key.
    #[test]
    fn a_pinned_request_carries_the_attribution_the_acl_needs() {
        let actor = ("rootops".to_string(), "10.0.0.7".to_string());
        let body =
            request_body(Some("abc:secret"), true, "whoami", json!({}), Some(&actor)).unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["pin"], json!(true));
        assert_eq!(v["account"], json!("rootops"));
        assert_eq!(v["ip"], json!("10.0.0.7"));
        // The saving, stated as a body: on a socket that is already pinned, the
        // credential is not presented again — but the account ALWAYS is, because
        // a withdrawn delegation has to bite on the next click rather than on
        // the next reconnect.
        let body = request_body(None, true, "whoami", json!({}), Some(&actor)).unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert!(
            v.get("k").is_none(),
            "a reused request presents no secret: {v}"
        );
        assert_eq!(v["account"], json!("rootops"));
    }

    /// The reuse answer comes from the server and nowhere else, and a refusal
    /// is not the same verdict as a broken socket. Four shapes, all real:
    /// pinned success, pinned refusal (the op said no, the connection did not),
    /// the classic envelope with no answer at all (silence means one socket per
    /// request), and a transport STATUS.
    #[test]
    fn only_the_servers_pinned_field_opens_a_session() {
        let r = reply_of(0, br#"{"ok":true,"r":{"a":1},"pinned":true}"#, "h:1");
        assert_eq!(r.out.unwrap(), json!({"a": 1}));
        assert!(r.keep, "the server said it pinned us");

        let r = reply_of(
            0,
            br#"{"ok":false,"e":"not permitted","pinned":true}"#,
            "h:1",
        );
        assert_eq!(r.out.unwrap_err(), "not permitted");
        assert!(
            r.keep,
            "a governance refusal is an answer, not a broken socket"
        );

        let r = reply_of(0, br#"{"ok":true,"r":7}"#, "h:1");
        assert_eq!(r.out.unwrap(), json!(7));
        assert!(!r.keep, "absence means one request per socket");

        let r = reply_of(17, b"boom", "h:1");
        assert_eq!(r.out.unwrap_err(), "boom");
        assert!(!r.keep, "a transport status is not a session");

        // Malformed JSON names the peer: "malformed control reply" without an
        // address is unsearchable in a gateway log that talks to one server.
        let r = reply_of(0, b"not json", "127.0.0.1:50111");
        assert!(r.out.unwrap_err().contains("127.0.0.1:50111"));
        assert!(!r.keep);
    }

    /// A stand-in for the pinned half of the meta-server's `handle_gov_conn`:
    /// answer every request on the socket it arrives on and count what came in.
    /// The counts under test are the CONNECTION total and which requests
    /// presented the credential — the two things a pooled caller claims to have
    /// changed. Replies echo the op back, so a client that read the wrong
    /// socket's reply would fail the assertion instead of passing quietly.
    struct FakeCtl {
        addr: String,
        conns: Arc<AtomicUsize>,
        requests: Arc<AtomicUsize>,
        /// Requests that arrived on a socket already pinned, without a `k`:
        /// exactly what reuse is supposed to produce.
        keyless: Arc<AtomicUsize>,
    }

    impl FakeCtl {
        fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap().to_string();
            let conns = Arc::new(AtomicUsize::new(0));
            let requests = Arc::new(AtomicUsize::new(0));
            let keyless = Arc::new(AtomicUsize::new(0));
            let (c2, r2, k2) = (conns.clone(), requests.clone(), keyless.clone());
            std::thread::spawn(move || {
                for sock in listener.incoming() {
                    let Ok(mut sock) = sock else { continue };
                    c2.fetch_add(1, Ordering::SeqCst);
                    // One thread per accepted socket, requests served in order:
                    // the shape the real handler has for a pinned caller.
                    while let Ok((_op, body)) = proto::recv_request(&mut sock) {
                        let req: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                        if req.get("pin").and_then(Value::as_bool) == Some(true)
                            && req
                                .get("k")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .is_empty()
                        {
                            k2.fetch_add(1, Ordering::SeqCst);
                        }
                        r2.fetch_add(1, Ordering::SeqCst);
                        let env = serde_json::to_vec(&json!({
                            "ok": true, "r": req["op"], "pinned": true
                        }))
                        .unwrap();
                        let mut w = sock.try_clone().unwrap();
                        if proto::send_response(&mut w, 0, 0, &env).is_err() {
                            break;
                        }
                    }
                }
            });
            Self {
                addr,
                conns,
                requests,
                keyless,
            }
        }
    }

    /// The claim the pool is built on: three calls, one socket, one credential.
    /// If reuse silently stopped working this test would see three connections
    /// (a performance lie); if the credential started riding on every request
    /// again it would see zero keyless ones (an efficiency lie); if a reply were
    /// read off the wrong exchange, the echoed op would not match.
    #[test]
    fn a_pooled_caller_presents_the_credential_once_per_connection() {
        let fake = FakeCtl::start();
        let ctl = ControlClient::pooled(&fake.addr, "abc:secret", 2);
        for op in ["info", "whoami", "def.list"] {
            assert_eq!(
                ctl.call(op, json!({})).unwrap(),
                json!(op),
                "the reply must be the answer to THIS call"
            );
        }
        assert_eq!(
            fake.conns.load(Ordering::SeqCst),
            1,
            "three calls, one socket"
        );
        assert_eq!(fake.requests.load(Ordering::SeqCst), 3);
        assert_eq!(
            fake.keyless.load(Ordering::SeqCst),
            2,
            "only the request that establishes the session carries k"
        );
    }

    /// The other half: without a pool nothing changed, per call or per byte.
    /// This is what every CLI does, and what the server must keep serving
    /// unchanged for as long as old binaries are in people's shells.
    #[test]
    fn an_unpooled_caller_still_opens_one_connection_per_call() {
        let fake = FakeCtl::start();
        let ctl = ControlClient::new(&fake.addr, "abc:secret");
        for op in ["info", "whoami"] {
            assert_eq!(ctl.call(op, json!({})).unwrap(), json!(op));
        }
        assert_eq!(fake.conns.load(Ordering::SeqCst), 2, "no pool, no reuse");
        assert_eq!(
            fake.keyless.load(Ordering::SeqCst),
            0,
            "a one-shot request always presents the credential"
        );
    }

    /// A refusal the server answered while keeping the session must reach the
    /// caller as the server's own sentence AND leave the socket in the pool —
    /// otherwise a user pressing one forbidden button would cost the gateway a
    /// reconnect, and an ACL text would arrive as a transport error.
    #[test]
    fn a_refusal_on_a_pinned_socket_is_answered_and_kept() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let conns = Arc::new(AtomicUsize::new(0));
        let hits = Arc::new(AtomicUsize::new(0));
        let (c2, h2) = (conns.clone(), hits.clone());
        std::thread::spawn(move || {
            for sock in listener.incoming() {
                let Ok(mut sock) = sock else { continue };
                let (conns, hits) = (c2.clone(), h2.clone());
                // A thread per accepted socket, serving it until it closes: so a
                // reconnect is counted rather than hanging this test.
                std::thread::spawn(move || {
                    conns.fetch_add(1, Ordering::SeqCst);
                    loop {
                        let Ok((_op, body)) = proto::recv_request(&mut sock) else {
                            return;
                        };
                        let req: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                        let n = hits.fetch_add(1, Ordering::SeqCst) + 1;
                        // Refuse exactly ONE request — the second — and stay
                        // open, which is what the real handler does with a
                        // governance refusal on a session.
                        let env = if n == 2 {
                            json!({"ok": false, "e": "'view.rm' is not per-view governance", "pinned": true})
                        } else {
                            json!({"ok": true, "r": req["op"], "pinned": true})
                        };
                        let data = serde_json::to_vec(&env).unwrap();
                        let mut w = sock.try_clone().unwrap();
                        if proto::send_response(&mut w, 0, 0, &data).is_err() {
                            return;
                        }
                    }
                });
            }
        });
        let ctl = ControlClient::pooled(&addr, "abc:secret", 1);
        assert_eq!(ctl.call("info", json!({})).unwrap(), json!("info"));
        let e = ctl.call("view.rm", json!({"view": "x"})).unwrap_err();
        assert!(
            e.contains("not per-view governance"),
            "the server's words: {e}"
        );
        // Third call lands on the SAME socket: it got no reply if the refusal
        // threw the connection away, so the count below is the claim.
        assert_eq!(ctl.call("whoami", json!({})).unwrap(), json!("whoami"));
        assert_eq!(
            conns.load(Ordering::SeqCst),
            1,
            "a refusal must not cost the gateway a reconnect"
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            3,
            "three requests, one connection"
        );
    }

    /// Both paths must describe an unreachable plane in the one sentence the
    /// docs and the gateway's startup probe print. A pool that invented its own
    /// wording would leave half the deployments with advice nobody recognizes.
    #[test]
    fn an_unreachable_plane_is_named_the_same_way_either_way() {
        // Nothing listens on 1; the connect fails before a byte is written.
        for (what, r) in [
            (
                "pooled",
                GovPool {
                    max: 1,
                    idle: Mutex::new(Vec::new()),
                }
                .call("127.0.0.1:1", "k:s", "info", json!({}), None),
            ),
            (
                "one-shot",
                one_shot("127.0.0.1:1", "k:s", "info", json!({}), None),
            ),
        ] {
            let e = r.unwrap_err();
            assert!(
                e.contains("cannot reach the control plane at 127.0.0.1:1")
                    && e.contains("--control-listen"),
                "{what}: {e}"
            );
        }
    }
}

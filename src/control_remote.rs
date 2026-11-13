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
//! every backend can now do both (SQLite rows, a PG schema, an FDB subspace).
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

use std::net::TcpStream;
use std::path::Path;
use std::sync::Arc;

use serde_json::{json, Value};

use crate::meta::{GraftItem, GraftStats, ForkStats, MetaProvider};
use crate::mcp::ControlStore;
use crate::remote::proto;

/// A governance client bound to one `meta-server --control-listen` address and
/// one admin credential.
#[derive(Clone)]
pub struct ControlClient {
    addr: String,
    key: String,
}

impl ControlClient {
    pub fn new(addr: &str, key: &str) -> Self {
        Self {
            addr: addr.to_string(),
            key: key.to_string(),
        }
    }

    pub fn addr(&self) -> &str {
        &self.addr
    }

    /// One governance call. `op` is a dotted verb (`def.list`, `view.create`,
    /// `apikey.create`, …); `args` is the op's JSON object. Errors are the
    /// server's own refusal text (unknown/revoked key, wrong scope, a governance
    /// refusal), never a bare status code — the point of the envelope.
    pub fn call(&self, op: &str, args: Value) -> Result<Value, String> {
        let body = serde_json::to_vec(&json!({
            "k": self.key,
            "op": op,
            "a": args,
        }))
        .map_err(|e| format!("cannot encode control request: {e}"))?;
        let mut sock = TcpStream::connect(&self.addr).map_err(|e| {
            format!(
                "cannot reach the control plane at {}: {e} — is `meta-server --control-listen {}` running?",
                self.addr, self.addr
            )
        })?;
        let _ = sock.set_nodelay(true);
        let mut w = sock
            .try_clone()
            .map_err(|e| format!("control socket: {e}"))?;
        proto::send_request(&mut w, proto::OP_CONTROL, &body)
            .map_err(|e| format!("control request failed: {e}"))?;
        let (status, _val, data) =
            proto::recv_response(&mut sock).map_err(|e| format!("control reply failed: {e}"))?;
        if status != 0 {
            return Err(String::from_utf8_lossy(&data).into_owned());
        }
        let env: Value = serde_json::from_slice(&data)
            .map_err(|e| format!("malformed control reply from {}: {e}", self.addr))?;
        if env.get("ok").and_then(Value::as_bool) == Some(true) {
            Ok(env.get("r").cloned().unwrap_or(Value::Null))
        } else {
            Err(env
                .get("e")
                .and_then(Value::as_str)
                .unwrap_or("the control plane refused the request")
                .to_string())
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
        let handle = as_str(
            begin.get("handle").unwrap_or(&Value::Null),
            "export handle",
        )?;
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
        Ok(crate::meta::ViewExportStats {
            nodes,
            blobs,
            kind,
        })
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
        let handle = as_str(
            begin.get("handle").unwrap_or(&Value::Null),
            "import handle",
        )?;
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
                off = r.get("size").and_then(Value::as_u64).unwrap_or(off + cut as u64);
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
/// a zero: reporting "0 shared object(s)" for a reply the client did not
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
    /// (`"postgres"`/`"sqlite"`/`"foundationdb"`) — surfaced in `view list` and
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
            "foundationdb" => "foundationdb",
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

    fn create_fork_tree(
        &self,
        base: &str,
        name: &str,
        seed: bool,
    ) -> Result<ForkStats, String> {
        // The server performs the copy; a backend that cannot fork (PG has no
        // per-view snapshot path, an `fdb://` view is a subspace) answers with
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
             use `grovefs mount --meta-server …` or `grovefs du --meta-server …`"
                .into(),
        )
    }

    fn base_tree_missing_note(&self, _view: &str) -> Result<Option<String>, String> {
        Ok(None)
    }
}

//! The MANAGER daemon: holds the declared topology, answers `OP_LOOKUP` with a
//! signed roster, and keeps track of which declared nodes are actually up.
//!
//! Two inputs, and the difference between them is the whole design:
//! - the **declaration** (`grovefs manager define`) is what the OPERATOR says
//!   the topology is: a deployment's meta failover list and its complete data
//!   member set. This is authoritative because membership is a provisioning
//!   fact, not an emergent one — GroveFS pins striped objects to the exact
//!   member set (see [`crate::backend::sharded`]), so a roster assembled from
//!   "whoever is currently heartbeating" would hand a client a set that refuses
//!   its own objects.
//! - a **registration** (`OP_REGISTER`) is what a NODE says about itself. It can
//!   never widen a roster: a registration is accepted only for an address
//!   already declared for that role and deployment. So a leaked join key can
//!   re-assert a legitimate address (a liveness lie, which buys nothing on the
//!   data path) but cannot substitute its own — the property that makes
//!   "register, then mount by name" safe to offer at all.
//!
//! State lives in `<state-dir>/manager/deployments.json`, and `serve` re-reads
//! it when the file changes. That is what lets `manager define` be a separate
//! short-lived command instead of a fourth authenticated op, and keeps the
//! declaration set reviewable in version control like any other piece of
//! operator config.

use super::{Announcement, Role, Roster};
use crate::crypto::now_secs;
use crate::manager::{check_dialable, registration_interval, split_signed};
use crate::remote::proto;
use libc::{EACCES, EAGAIN, ENOENT, ENOSYS};
use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

/// Default lifetime of an ISSUED roster, in seconds. Long enough that a mount in
/// progress never races its own bootstrap, short enough that a captured roster
/// stops being a routing instruction at the next topology change. A mount
/// consults the manager once and never again, so this bounds REPLAY.
pub const DEFAULT_ROSTER_TTL: u64 = 300;

/// Default lifetime of a REGISTRATION before it lapses without a heartbeat. The
/// node re-sends at [`registration_interval`] (a third of this), so one lost
/// heartbeat is not an outage report.
pub const DEFAULT_REGISTER_TTL: u64 = 90;

/// A registration may not claim a longer life than this, however the node asks:
/// a buggy `--register-ttl` should not park a stale entry for a year.
const REGISTER_TTL_MAX: u64 = 3_600;
/// …nor a shorter one: a node asking to lapse in 1s is a config error that would
/// present as a flaky registry.
const REGISTER_TTL_MIN: u64 = 5;

/// What the operator declared for one deployment. Order is significant for
/// `meta` (it IS the failover preference) and irrelevant for `data` (placement
/// sorts by member id).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Declaration {
    pub meta: Vec<String>,
    pub data: Vec<String>,
}

impl Declaration {
    fn contains(&self, role: Role, addr: &str) -> bool {
        let list = match role {
            Role::Meta => &self.meta,
            Role::Data => &self.data,
        };
        list.iter().any(|a| a == addr)
    }

    fn declared_for(&self, role: Role) -> &Vec<String> {
        match role {
            Role::Meta => &self.meta,
            Role::Data => &self.data,
        }
    }

    fn is_empty(&self) -> bool {
        self.meta.is_empty() && self.data.is_empty()
    }
}

/// The last time a node of this role/address claimed to be up, and what it said
/// its member id was. Two timestamps because `manager status` answers two
/// different operator questions with them: `last_seen` is "is this node still
/// talking to me" (a heartbeat refreshes it, so a healthy member reads ~0-30s),
/// while `first_seen` is "how long has this address been part of the fleet" —
/// resetting that on every heartbeat would make the second question unanswerable.
#[derive(Debug, Clone)]
struct Live {
    expires_at: u64,
    first_seen: u64,
    last_seen: u64,
    member_id: Option<u64>,
}

/// Why a registration or a lookup was refused, and which errno the peer should
/// see. One type for both paths because the refusal text is the product here:
/// an operator reading `Connection refused` off a mount has exactly one
/// question, and this answers it.
#[derive(Debug)]
pub enum Denial {
    /// Nobody has declared this deployment on this manager.
    Undeclared {
        deployment: String,
        known: Vec<String>,
    },
    /// The deployment exists; this address is not part of it for this role.
    NotDeclared {
        role: Role,
        addr: String,
        deployment: String,
        declared: Vec<String>,
    },
    /// The deployment exists but declares no metadata service, so there is
    /// nothing a mount could dial.
    NoMeta {
        deployment: String,
    },
    /// `--require-registered` and some declared address has no live claim.
    NotRegistered {
        deployment: String,
        missing: Vec<String>,
    },
}

impl fmt::Display for Denial {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Denial::Undeclared { deployment, known } => write!(
                f,
                "deployment '{deployment}' is not declared on this manager (known: [{}]) — \
                 define it with `grovefs manager define --name {deployment} --meta ADDR[,ADDR] \
                 --data ADDR[,ADDR]`",
                known.join(", ")
            ),
            Denial::NotDeclared {
                role,
                addr,
                deployment,
                declared,
            } => write!(
                f,
                "{} address {addr} is not declared for deployment '{deployment}'; a \
                 registration can never ADD an address to a roster — correct --advertise-addr \
                 or declare it (declared for this role: [{}])",
                role.name(),
                declared.join(", ")
            ),
            Denial::NoMeta { deployment } => write!(
                f,
                "deployment '{deployment}' declares no meta-server, so a roster cannot answer \
                 what to mount against; declare one (`grovefs manager define --name \
                 {deployment} --meta …`)"
            ),
            Denial::NotRegistered {
                deployment,
                missing,
            } => write!(
                f,
                "deployment '{deployment}' has declared addresses that have not registered: \
                 [{}] — start them, or drop --require-registered and let clients find out by \
                 connecting",
                missing.join(", ")
            ),
        }
    }
}

impl Denial {
    /// `EAGAIN` for a wait-for-it condition (a node that will register in a
    /// moment), `ENOENT` for a name that does not exist, `EACCES` for a node
    /// that is not authorized to place itself in a topology. The distinction
    /// lets a script retry the first and fix its config on the second.
    fn code(&self) -> i32 {
        match self {
            Denial::NotRegistered { .. } => EAGAIN,
            Denial::Undeclared { .. } | Denial::NoMeta { .. } => ENOENT,
            Denial::NotDeclared { .. } => EACCES,
        }
    }
}

/// The daemon. `serve` moves it into an `Arc`; the mutex is the only shared
/// mutable state (a registry written a few times a minute and read a few times
/// an hour, so no finer-grained locking is worth the reading).
pub struct Manager {
    listen: String,
    state_dir: PathBuf,
    join_key: Vec<u8>,
    roster_key: Vec<u8>,
    roster_ttl: u64,
    register_ttl: u64,
    require_registered: bool,
    inner: parking_lot::Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    declared: BTreeMap<String, Declaration>,
    /// `(deployment, role, addr)` → last claim. Keyed by the DECLARED address
    /// string, so two spellings of one host are two entries and neither can
    /// shadow the other.
    live: BTreeMap<(String, Role, String), Live>,
    epoch: u64,
    /// `mtime` of the file `declared` came from; a change triggers a re-read,
    /// which is how a `manager define` beside a running daemon lands.
    file_mtime: Option<std::time::SystemTime>,
}

/// The declaration file's shape, kept separate from the daemon's in-memory
/// state (which also holds liveness and must not get serialized).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DeclFile {
    pub epoch: u64,
    pub deployments: BTreeMap<String, Declaration>,
}

/// `<state-dir>/manager` — the one directory holding a manager's operator state:
/// the declaration file, and by default the two keys it was started with.
pub fn manager_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("manager")
}

/// `<state-dir>/manager/deployments.json`.
pub fn decl_path(state_dir: &Path) -> PathBuf {
    manager_dir(state_dir).join("deployments.json")
}

/// Read the declaration file. A missing file is an empty set, not an error: a
/// freshly installed manager legitimately has nothing declared, and `serve`
/// says so in its banner rather than refusing to start.
pub fn load_declared(state_dir: &Path) -> Result<DeclFile, String> {
    let path = decl_path(state_dir);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(DeclFile::default()),
        Err(e) => return Err(format!("{}: cannot read: {e}", path.display())),
    };
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| format!("{}: not valid JSON: {e}", path.display()))?;
    let mut out = DeclFile::default();
    let obj = value
        .as_object()
        .ok_or_else(|| format!("{}: expected a JSON object", path.display()))?;
    if let Some(v) = obj.get("version").and_then(|v| v.as_u64()) {
        if v != 1 {
            return Err(format!(
                "{}: declaration format version {v} is not understood by this build",
                path.display()
            ));
        }
    }
    out.epoch = obj.get("epoch").and_then(|v| v.as_u64()).unwrap_or(0);
    let Some(decls) = obj.get("deployments").and_then(|v| v.as_object()) else {
        return Ok(out);
    };
    for (name, d) in decls {
        let list = |k: &str| -> Vec<String> {
            d.get(k)
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };
        out.deployments.insert(
            name.clone(),
            Declaration {
                meta: list("meta"),
                data: list("data"),
            },
        );
    }
    Ok(out)
}

/// Write the declaration file atomically (temp + rename), so a `serve` that
/// reloads mid-write never sees a truncated document and quietly answers with a
/// partial topology.
pub fn save_declared(state_dir: &Path, file: &DeclFile) -> Result<PathBuf, String> {
    let path = decl_path(state_dir);
    let dir = path
        .parent()
        .ok_or_else(|| format!("{}: no parent directory", path.display()))?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut decls = serde_json::Map::new();
    for (name, d) in &file.deployments {
        decls.insert(
            name.clone(),
            serde_json::json!({ "meta": d.meta, "data": d.data }),
        );
    }
    let obj = serde_json::json!({
        "version": 1,
        "epoch": file.epoch,
        "deployments": serde_json::Value::Object(decls),
    });
    let text = serde_json::to_string_pretty(&obj).map_err(|e| format!("serialize: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text.as_bytes()).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .map_err(|e| format!("{} -> {}: {e}", tmp.display(), path.display()))?;
    Ok(path)
}

/// Stable, order-preserving dedupe, with the dialable check applied on the way
/// through: duplicates in a failover list are almost always a typo, and the
/// second copy makes a mount's behavior depend on nothing but noise.
fn normalize(list: &[String]) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::with_capacity(list.len());
    for item in list {
        let a = check_dialable(item)?;
        if !out.contains(&a) {
            out.push(a);
        }
    }
    Ok(out)
}

/// Add or replace ONE deployment's declaration and persist it. Bumps the epoch
/// only when the content actually changed, so `epoch` keeps meaning "this
/// topology has been re-declared this many times".
///
/// This is the `manager define` path: it touches the FILE, not a running
/// daemon, which is why changing topology needs no key beyond shell access to
/// the state directory (a roster signature is minted by the daemon alone).
pub fn define_deployment(
    state_dir: &Path,
    name: &str,
    meta: &[String],
    data: &[String],
) -> Result<(PathBuf, DeclFile), String> {
    if name.is_empty() || name.contains('/') || name.contains(',') || name.contains(':') {
        return Err(format!(
            "deployment name {name:?}: use a plain token (no '/', ',' or ':') — it appears in \
             every mount command line and inside the signed roster"
        ));
    }
    let mut file = load_declared(state_dir)?;
    let next = Declaration {
        meta: normalize(meta)?,
        data: normalize(data)?,
    };
    if next.is_empty() {
        return Err(format!(
            "deployment '{name}' would declare no addresses at all: pass --meta ADDR[,ADDR] \
             and/or --data ADDR[,ADDR]"
        ));
    }
    if file.deployments.get(name) == Some(&next) {
        // Re-declaring the same thing is not an error (an idempotent re-run from
        // config management) and must not inflate the epoch.
        return Ok((decl_path(state_dir), file));
    }
    if next.meta.is_empty() {
        eprintln!(
            "note: deployment '{name}' declares no meta-server, so `mount --manager` against it \
             will be refused — a mount always resolves BOTH a meta- and a data-server from the \
             roster, so declare one with --meta (a data-only deployment is not mountable)"
        );
    }
    file.deployments.insert(name.to_string(), next);
    file.epoch += 1;
    let path = save_declared(state_dir, &file)?;
    Ok((path, file))
}

impl Manager {
    pub fn new(
        listen: String,
        state_dir: PathBuf,
        join_key: Vec<u8>,
        roster_key: Vec<u8>,
        roster_ttl: u64,
        register_ttl: u64,
        require_registered: bool,
    ) -> Result<Manager, String> {
        let file = load_declared(&state_dir)?;
        Ok(Manager {
            listen,
            state_dir,
            join_key,
            roster_key,
            roster_ttl,
            register_ttl,
            require_registered,
            inner: parking_lot::Mutex::new(Inner {
                declared: file.deployments,
                epoch: file.epoch,
                ..Default::default()
            }),
        })
    }

    /// Blocks forever serving registrations, lookups and status.
    pub fn serve(self) -> io::Result<()> {
        let listener = crate::platform::bind_listener(&self.listen)?;
        let this = Arc::new(self);
        println!(
            "manager listening on {} (host-readable state {})",
            this.listen,
            decl_path(&this.state_dir).display()
        );
        {
            let inner = this.inner.lock();
            if inner.declared.is_empty() {
                println!(
                    "manager: nothing declared yet — define a deployment with `grovefs manager \
                     define --name NAME --meta ADDR[,ADDR] --data ADDR[,ADDR]`; lookups fail \
                     closed until then"
                );
            }
            for (name, d) in &inner.declared {
                println!(
                    "manager: deployment '{name}' (epoch {}) — meta [{}] data [{}]",
                    inner.epoch,
                    d.meta.join(", "),
                    d.data.join(", ")
                );
            }
        }
        println!(
            "manager: rosters valid {}s after issue; registrations lapse after {}s of silence; \
             lookups {}",
            this.roster_ttl,
            this.register_ttl,
            if this.require_registered {
                "REQUIRE every declared address to have registered"
            } else {
                "answer from the declaration, live or not"
            }
        );
        println!(
            "manager: neither key leaves this process — a node proves itself with the JOIN key, \
             a client checks the ROSTER key; heartbeat is ~{}s, and a node that cannot reach \
             this service keeps serving its own mounts",
            registration_interval(this.register_ttl).as_secs()
        );
        for stream in listener.incoming() {
            match stream {
                Ok(s) => {
                    crate::remote::enable_tcp_keepalive(&s);
                    let this = this.clone();
                    thread::spawn(move || {
                        if let Err(e) = this.handle_conn(s) {
                            // A peer hanging up after a refusal is normal.
                            if e.kind() != io::ErrorKind::UnexpectedEof {
                                eprintln!("manager conn error: {e}");
                            }
                        }
                    });
                }
                Err(e) => eprintln!("manager accept failed: {e}"),
            }
        }
        Ok(())
    }

    /// Re-read the declaration file when its mtime moved. Cheap by construction
    /// (one stat per request, and a mount lookup is a rare event), and it is
    /// what lets `manager define` stay a separate command.
    fn reload_if_changed(&self) {
        let path = decl_path(&self.state_dir);
        let mtime = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok();
        let Some(mtime) = mtime else { return };
        let mut inner = self.inner.lock();
        if Some(mtime) == inner.file_mtime {
            return;
        }
        match load_declared(&self.state_dir) {
            Ok(file) => {
                // A failed read leaves the previous declarations in force: a
                // half-written or hand-corrupted file must not be able to empty
                // the registry and refuse every mount in the cluster.
                inner.file_mtime = Some(mtime);
                let changed = file.deployments != inner.declared || file.epoch != inner.epoch;
                inner.declared = file.deployments;
                inner.epoch = file.epoch;
                if changed {
                    println!("manager: declarations reloaded from {}", path.display());
                }
            }
            Err(e) => eprintln!("manager: {e}; keeping the declarations already loaded"),
        }
    }

    /// Drop lapsed registrations, so `live` cannot grow without bound on a
    /// manager whose operator retires nodes without re-declaring them.
    fn purge_expired(&self, now: u64) {
        let mut inner = self.inner.lock();
        inner.live.retain(|_, l| l.expires_at > now);
    }

    fn handle_conn(&self, stream: TcpStream) -> io::Result<()> {
        stream.set_nodelay(true)?;
        let mut reader = &stream;
        let mut writer = &stream;
        let (op, body) = proto::recv_request(&mut reader)?;
        match op {
            proto::OP_REGISTER => self.on_register(&mut writer, &body),
            proto::OP_LOOKUP => self.on_lookup(&mut writer, &body),
            proto::OP_STATUS => self.on_status(&mut writer, &body),
            other => proto::send_response(
                &mut writer,
                ENOSYS,
                0,
                format!("manager: op {other} is not served on this port").as_bytes(),
            ),
        }
    }

    /// A node says where it is. Authorized twice over: the join-key MAC proves
    /// it is a cluster node, the declaration proves the address belongs to the
    /// deployment it claims.
    fn on_register(&self, w: &mut &TcpStream, body: &[u8]) -> io::Result<()> {
        let a = match Announcement::decode_signed(body, &self.join_key) {
            Ok(a) => a,
            // Checked before anything is read, so an unauthenticated peer cannot
            // even probe which deployments exist.
            Err(r) => return refuse(w, EACCES, &format!("registration refused: {r}")),
        };
        self.reload_if_changed();
        match self.record(&a, now_secs()) {
            Ok(echo) => proto::send_response(w, 0, 0, echo.as_bytes()),
            Err(d) => refuse(w, d.code(), &d.to_string()),
        }
    }

    /// Record one node's liveness claim. Split out from [`Manager::on_register`]
    /// so the policy is testable without a socket, and so the wire layer holds
    /// nothing but framing.
    fn record(&self, a: &Announcement, now: u64) -> Result<String, Denial> {
        let ttl = a.ttl.clamp(REGISTER_TTL_MIN, REGISTER_TTL_MAX);
        let mut inner = self.inner.lock();
        let d = match inner.declared.get(&a.deployment) {
            Some(d) => d,
            None => {
                return Err(Denial::Undeclared {
                    deployment: a.deployment.clone(),
                    known: inner.declared.keys().cloned().collect(),
                })
            }
        };
        if !d.contains(a.role, &a.addr) {
            // THE anti-forgery rule. A valid join key buys a liveness record for
            // a DECLARED address and nothing else.
            return Err(Denial::NotDeclared {
                role: a.role,
                addr: a.addr.clone(),
                deployment: a.deployment.clone(),
                declared: d.declared_for(a.role).clone(),
            });
        }
        let key = (a.deployment.clone(), a.role, a.addr.clone());
        let first_seen = inner
            .live
            .get(&key)
            .map(|l| l.first_seen)
            .unwrap_or(now);
        inner.live.insert(
            key,
            Live {
                expires_at: now.saturating_add(ttl),
                first_seen,
                last_seen: now,
                member_id: a.member_id,
            },
        );
        Ok(format!(
            "registered {} {} of deployment '{}'{} — live {ttl}s",
            a.role.name(),
            a.addr,
            a.deployment,
            a.member_id
                .map(|m| format!(", member-id {m}"))
                .unwrap_or_default(),
        ))
    }

    /// The client's question. Unsigned by design (an address list is not a
    /// secret); what makes the ANSWER trustworthy is the roster key the client
    /// holds and no one on the client side needs.
    fn on_lookup(&self, w: &mut &TcpStream, body: &[u8]) -> io::Result<()> {
        let mut i = 0;
        let deployment = match proto::get_key(body, &mut i) {
            Ok(d) if i == body.len() => d,
            _ => return refuse(w, libc::EINVAL, "manager: malformed lookup"),
        };
        self.reload_if_changed();
        match self.build_roster(&deployment, now_secs()) {
            Ok(roster) => {
                let bytes = roster.encode_signed(&self.roster_key);
                proto::send_response(w, 0, 0, &bytes)
            }
            Err(d) => refuse(w, d.code(), &d.to_string()),
        }
    }

    /// Assemble (and, with `require_registered`, gate) a roster.
    ///
    /// The lists are the DECLARATION, not the live set — see the module docs for
    /// why "whoever is up" would be wrong for both sides (`check_layout` refuses
    /// a changed member set; a dropped standby silently weakens HA). Liveness
    /// decides only whether to refuse the lookup outright, and an empty `data`
    /// declaration is answered as "no data service", never as a partial one.
    fn build_roster(&self, deployment: &str, now: u64) -> Result<Roster, Denial> {
        let inner = self.inner.lock();
        let Some(d) = inner.declared.get(deployment) else {
            return Err(Denial::Undeclared {
                deployment: deployment.to_string(),
                known: inner.declared.keys().cloned().collect(),
            });
        };
        if d.meta.is_empty() {
            return Err(Denial::NoMeta {
                deployment: deployment.to_string(),
            });
        }
        if self.require_registered {
            let missing: Vec<String> = d
                .declared_for(Role::Meta)
                .iter()
                .chain(d.declared_for(Role::Data).iter())
                .filter(|addr| {
                    let role = if d.meta.iter().any(|a| a == *addr) {
                        Role::Meta
                    } else {
                        Role::Data
                    };
                    !inner
                        .live
                        .get(&(deployment.to_string(), role, addr.to_string()))
                        .is_some_and(|l| l.expires_at > now)
                })
                .cloned()
                .collect();
            if !missing.is_empty() {
                return Err(Denial::NotRegistered {
                    deployment: deployment.to_string(),
                    missing,
                });
            }
        }
        Ok(Roster {
            deployment: deployment.to_string(),
            meta: d.meta.clone(),
            data: d.data.clone(),
            epoch: inner.epoch,
            issued_at: now,
            expires_at: now.saturating_add(self.roster_ttl),
        })
    }

    /// The operator's view: declarations, liveness, and the epoch a client is
    /// about to be handed. Authenticated with the join key, since enumerating a
    /// cluster's addresses is not something to hand a passer-by.
    fn on_status(&self, w: &mut &TcpStream, body: &[u8]) -> io::Result<()> {
        let payload = match split_signed(body, &self.join_key) {
            Ok(p) => p,
            Err(r) => return refuse(w, EACCES, &format!("status refused: {r}")),
        };
        let mut i = 0;
        let want = proto::get_key(payload, &mut i).unwrap_or_default();
        let now = now_secs();
        self.purge_expired(now);
        self.reload_if_changed();
        let inner = self.inner.lock();
        let mut deps = Vec::new();
        for (name, d) in &inner.declared {
            if !want.is_empty() && name != &want {
                continue;
            }
            // `purge_expired` has just run, so an entry in `live` means "live
            // right now" — which is what `registered` reports.
            let row = |role: Role, list: &[String]| -> Vec<serde_json::Value> {
                list.iter()
                    .map(|addr| {
                        let l = inner.live.get(&(name.clone(), role, addr.clone()));
                        serde_json::json!({
                            "addr": addr,
                            "member_id": l.and_then(|x| x.member_id),
                            "registered": l.is_some(),
                            "seen_secs_ago": l.map(|x| now.saturating_sub(x.last_seen)),
                            "member_for_secs": l.map(|x| now.saturating_sub(x.first_seen)),
                            "live_secs_left": l.map(|x| x.expires_at.saturating_sub(now)),
                        })
                    })
                    .collect()
            };
            deps.push(serde_json::json!({
                "name": name,
                "meta": row(Role::Meta, &d.meta),
                "data": row(Role::Data, &d.data),
            }));
        }
        let out = serde_json::json!({
            "listen": &self.listen,
            "declaration_file": decl_path(&self.state_dir).display().to_string(),
            "epoch": inner.epoch,
            "roster_ttl": self.roster_ttl,
            "register_ttl": self.register_ttl,
            "require_registered": self.require_registered,
            "now": now,
            "deployments": deps,
        });
        drop(inner);
        proto::send_response(w, 0, 0, out.to_string().as_bytes())
    }
}

/// One refusal: an errno status plus the reason as the payload, so the caller
/// prints WHICH of its configs is wrong instead of `Connection refused`.
fn refuse(w: &mut &TcpStream, code: i32, why: &str) -> io::Result<()> {
    proto::send_response(w, code, 0, why.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::Refusal;

    fn tmp_state(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gfs-mgr-test-{}-{}",
            name,
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn manager(dir: &Path, require: bool) -> Manager {
        Manager::new(
            "127.0.0.1:0".to_string(),
            dir.to_path_buf(),
            vec![1u8; 32],
            vec![2u8; 32],
            300,
            90,
            require,
        )
        .unwrap()
    }

    fn ann(role: Role, deployment: &str, addr: &str, member_id: Option<u64>) -> Announcement {
        Announcement {
            role,
            deployment: deployment.to_string(),
            addr: addr.to_string(),
            member_id,
            ttl: 90,
        }
    }

    /// Declarations survive the JSON round trip, including `meta` ORDER (it is
    /// the failover preference, so losing it would be a silent behavior change)
    /// and the epoch.
    #[test]
    fn declaration_file_round_trips_order_and_epoch() {
        let dir = tmp_state("roundtrip");
        let f = DeclFile {
            epoch: 4,
            deployments: BTreeMap::from([
                (
                    "prod".to_string(),
                    Declaration {
                        meta: vec!["m2:1".into(), "m1:1".into()],
                        data: vec!["d1:1".into(), "d2:1".into()],
                    },
                ),
                (
                    "lab".to_string(),
                    Declaration {
                        meta: vec!["m9:1".into()],
                        data: vec![],
                    },
                ),
            ]),
        };
        save_declared(&dir, &f).unwrap();
        let back = load_declared(&dir).unwrap();
        assert_eq!(back, f);
        assert_eq!(back.deployments["prod"].meta, vec!["m2:1", "m1:1"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A missing file is "nothing declared yet", not a failure: `serve` must
    /// come up and say so.
    #[test]
    fn missing_declaration_file_is_empty() {
        let dir = tmp_state("missing");
        assert_eq!(load_declared(&dir).unwrap(), DeclFile::default());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A JSON file the operator hand-edited into nonsense must not be able to
    /// empty a running registry: the daemon keeps the declarations it has.
    #[test]
    fn corrupt_declaration_file_is_reported_not_applied() {
        let dir = tmp_state("corrupt");
        define_deployment(&dir, "prod", &["m1:1".into()], &[]).unwrap();
        let m = manager(&dir, false);
        m.reload_if_changed();
        assert_eq!(m.inner.lock().declared.len(), 1);
        std::fs::write(decl_path(&dir), "{ not json").unwrap();
        // Force the mtime to move: a same-timestamp write is a no-op by design
        // (the reload is keyed on mtime, not on content hash).
        std::thread::sleep(std::time::Duration::from_millis(1_200));
        std::fs::write(decl_path(&dir), "{ still not json").unwrap();
        m.reload_if_changed();
        assert_eq!(
            m.inner.lock().declared.len(),
            1,
            "a broken reload must never drop a live deployment"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `define` is idempotent and bumps the epoch only on a real change —
    /// config-management re-runs must not make every client see a new epoch.
    #[test]
    fn define_bumps_epoch_only_on_change() {
        let dir = tmp_state("define");
        let (_, f1) = define_deployment(&dir, "prod", &["m1:1".into()], &["d1:1".into()]).unwrap();
        assert_eq!(f1.epoch, 1);
        let (_, f2) = define_deployment(&dir, "prod", &["m1:1".into()], &["d1:1".into()]).unwrap();
        assert_eq!(f2.epoch, 1, "re-declaring the same topology is not a change");
        let (_, f3) = define_deployment(
            &dir,
            "prod",
            &["m1:1".into(), "m2:1".into()],
            &["d1:1".into()],
        )
        .unwrap();
        assert_eq!(f3.epoch, 2);
        assert_eq!(f3.deployments["prod"].meta, vec!["m1:1", "m2:1"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Duplicates are a typo; whitespace is not part of an address.
    #[test]
    fn define_dedupes_and_trims() {
        let dir = tmp_state("dedupe");
        let (_, f) = define_deployment(
            &dir,
            "prod",
            &["m1:1".into(), " m1:1 ".into()],
            &["d1:1".into(), "d1:1".into(), "d2:1".into()],
        )
        .unwrap();
        assert_eq!(f.deployments["prod"].meta, vec!["m1:1"]);
        assert_eq!(f.deployments["prod"].data, vec!["d1:1", "d2:1"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A bind wildcard, a missing port or a nameless host is refused at DEFINE
    /// time, because the roster it would produce is dialed by a client that
    /// cannot use it.
    #[test]
    fn define_refuses_addresses_a_client_cannot_dial() {
        let dir = tmp_state("wildcard");
        for bad in [
            "0.0.0.0:50053",
            "[::]:50053",
            ":::50053",
            "host-without-port",
            "host:",
            ":50053",
            "host:notaport",
        ] {
            let e = define_deployment(&dir, "prod", &[bad.to_string()], &[]).unwrap_err();
            assert!(
                e.contains(&format!("{bad:?}")),
                "{bad:?} rejected with a message that does not name it: {e}"
            );
        }
        // And the forms that ARE dialable, including a bracketed IPv6 host.
        for good in ["10.0.0.1:50053", "[fd00::5]:50053", "h.example:1"] {
            define_deployment(&dir, "prod", &[good.to_string()], &[]).unwrap_or_else(|e| {
                panic!("{good:?} refused: {e}")
            });
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A deployment that declares nothing is not a deployment, and a name that
    /// would corrupt a command line or the roster is refused.
    #[test]
    fn define_validates_shape() {
        let dir = tmp_state("shape");
        assert!(define_deployment(&dir, "prod", &[], &[]).is_err());
        for bad in ["", "a/b", "a,b", "a:1"] {
            assert!(
                define_deployment(&dir, bad, &["m:1".into()], &[]).is_err(),
                "{bad:?} accepted"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The roster a lookup builds is the DECLARATION verbatim — including a
    /// declared member that has never registered. That is the difference between
    /// a directory service and a load balancer, and only a test pins it:
    /// silently "helpfully" dropping the down member would break
    /// `check_layout` for the client instead.
    #[test]
    fn roster_ships_the_declared_set_not_the_live_set() {
        let dir = tmp_state("declared");
        define_deployment(
            &dir,
            "prod",
            &["m1:1".into(), "m2:1".into()],
            &["d1:1".into(), "d2:1".into()],
        )
        .unwrap();
        let m = manager(&dir, false);
        let r = m.build_roster("prod", 1_000).expect("roster");
        assert_eq!(r.meta, vec!["m1:1", "m2:1"]);
        assert_eq!(r.data, vec!["d1:1", "d2:1"]);
        assert_eq!(r.issued_at, 1_000);
        assert_eq!(r.expires_at, 1_300);
        assert_eq!(r.meta_list(), "m1:1,m2:1");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Unknown deployment: the refusal names the ones this manager does know,
    /// because "am I talking to the right manager" is the question a typo raises.
    #[test]
    fn lookup_of_unknown_deployment_names_the_known_ones() {
        let dir = tmp_state("unknown");
        define_deployment(&dir, "prod", &["m1:1".into()], &[]).unwrap();
        let m = manager(&dir, false);
        let e = m.build_roster("prod-typo", 1).unwrap_err().to_string();
        assert!(e.contains("prod-typo") && e.contains("prod"), "{e}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A deployment with no meta address cannot answer "what should I mount
    /// against", so it refuses rather than issuing an empty failover list.
    #[test]
    fn roster_needs_a_meta_address() {
        let dir = tmp_state("nometa");
        define_deployment(&dir, "prod", &[], &["d1:1".into()]).unwrap();
        let m = manager(&dir, false);
        assert!(m.build_roster("prod", 1).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `--require-registered` turns "the mount fails on connect" into "the
    /// lookup says which member is missing" — and only that: the roster content
    /// never shrinks.
    #[test]
    fn require_registered_gates_on_liveness_but_never_shrinks() {
        let dir = tmp_state("require");
        define_deployment(&dir, "prod", &["m1:1".into()], &["d1:1".into()]).unwrap();
        let m = manager(&dir, true);
        let e = m.build_roster("prod", 1_000).unwrap_err().to_string();
        assert!(e.contains("m1:1") && e.contains("d1:1"), "{e}");

        m.record(&ann(Role::Meta, "prod", "m1:1", None), 1_000).unwrap();
        // Data still never registered → refused, naming only d1:1.
        let e = m.build_roster("prod", 1_001).unwrap_err().to_string();
        assert!(e.contains("d1:1") && !e.contains("m1:1"), "{e}");

        m.record(&ann(Role::Data, "prod", "d1:1", Some(0)), 1_001)
            .unwrap();
        let r = m.build_roster("prod", 1_002).expect("both live now");
        assert_eq!(r.data, vec!["d1:1"]);

        // Once both claims lapse the lookup is refused again — by liveness only.
        let e = m.build_roster("prod", 9_000).unwrap_err().to_string();
        assert!(e.contains("m1:1") && e.contains("d1:1"), "{e}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The join MAC is checked before the declaration is read, so an
    /// unauthenticated peer cannot enumerate deployments by probing.
    #[test]
    fn registration_needs_the_join_key() {
        let dir = tmp_state("joinkey");
        define_deployment(&dir, "prod", &["m1:1".into()], &[]).unwrap();
        let m = manager(&dir, false);
        let good = ann(Role::Meta, "prod", "m1:1", None);
        // The manager holds vec![1;32]; this body is signed with something else.
        let body = good.encode_signed(&[9u8; 32]);
        assert_eq!(
            Announcement::decode_signed(&body, &m.join_key),
            Err(Refusal::Signature)
        );
        assert_eq!(
            Announcement::decode_signed(&good.encode_signed(&m.join_key), &m.join_key),
            Ok(good)
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A registration for an address that was never declared is refused even
    /// with a perfect join-key signature: the rule that stops a stolen key from
    /// pointing clients at the attacker. The refusal lists what IS declared, so
    /// a mistyped `--advertise-addr` is self-diagnosing.
    #[test]
    fn undeclared_address_can_never_enter_a_roster() {
        let dir = tmp_state("undeclared");
        define_deployment(&dir, "prod", &["m1:1".into()], &["d1:1".into()]).unwrap();
        let m = manager(&dir, false);
        for (role, addr) in [
            (Role::Data, "attacker.example:50052"),
            // A declared host, announced under the wrong role, is still not
            // declared for that role.
            (Role::Meta, "d1:1"),
            // A near-miss on port is not a match either.
            (Role::Data, "d1:2"),
        ] {
            let text = m.record(&ann(role, "prod", addr, None), 1_000).unwrap_err().to_string();
            assert!(text.contains(addr), "{addr}: {text}");
            assert!(
                text.contains("registration can never ADD an address"),
                "{addr}: {text}"
            );
        }
        // And the declared ones are accepted.
        assert!(m.record(&ann(Role::Data, "prod", "d1:1", Some(1)), 1_000).is_ok());
        assert!(m.record(&ann(Role::Meta, "prod", "m1:1", None), 1_000).is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A bogus TTL cannot park a live entry forever, nor lapse it instantly: the
    /// clamp is what makes `--register-ttl` uninteresting to attack.
    #[test]
    fn registration_ttl_is_clamped() {
        let dir = tmp_state("ttl");
        define_deployment(&dir, "prod", &["m1:1".into()], &[]).unwrap();
        let m = manager(&dir, true);
        let mut a = ann(Role::Meta, "prod", "m1:1", None);
        a.ttl = u64::MAX;
        m.record(&a, 1_000).unwrap();
        // A year later it has lapsed (the clamp capped it at an hour).
        assert!(m.build_roster("prod", 1_000 + 4_000).is_err());
        a.ttl = 0;
        m.record(&a, 2_000).unwrap();
        assert!(m.build_roster("prod", 2_004).is_ok(), "clamped up to 5s");
        assert!(m.build_roster("prod", 2_010).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Registrations that lapsed are dropped, so the map cannot grow without
    /// bound as an operator rolls nodes.
    #[test]
    fn expired_registrations_are_purged() {
        let dir = tmp_state("purge");
        define_deployment(&dir, "prod", &["m1:1".into()], &["d1:1".into()]).unwrap();
        let m = manager(&dir, false);
        m.record(&ann(Role::Meta, "prod", "m1:1", None), 1_000)
            .unwrap();
        m.record(&ann(Role::Data, "prod", "d1:1", None), 5_000)
            .unwrap();
        assert_eq!(m.inner.lock().live.len(), 2);
        // t=3000: the meta claim (recorded at 1000, live 90s) has lapsed, the data
        // claim (recorded at 5000) has not.
        m.purge_expired(3_000);
        let live = m.inner.lock();
        assert_eq!(live.live.len(), 1, "only the newer claim survives");
        assert!(live
            .live
            .contains_key(&("prod".to_string(), Role::Data, "d1:1".to_string())));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `first_seen` is the age of the ADDRESS record, not of the last heartbeat:
    /// `manager status` reads it as "how long has this member been part of the
    /// fleet" (`member_for_secs`), and resetting it every 30s would make that
    /// unanswerable. `last_seen` is the opposite — it MUST move on every claim,
    /// or `seen_secs_ago` grows for a perfectly healthy node and reads as an
    /// outage that never happened.
    #[test]
    fn first_seen_survives_heartbeats() {
        let dir = tmp_state("firstseen");
        define_deployment(&dir, "prod", &["m1:1".into()], &[]).unwrap();
        let m = manager(&dir, false);
        let a = ann(Role::Meta, "prod", "m1:1", None);
        m.record(&a, 1_000).unwrap();
        m.record(&a, 1_030).unwrap();
        let inner = m.inner.lock();
        let l = inner
            .live
            .get(&("prod".to_string(), Role::Meta, "m1:1".to_string()))
            .unwrap();
        assert_eq!(l.first_seen, 1_000);
        assert_eq!(l.last_seen, 1_030, "the heartbeat stamp moves with each claim");
        assert_eq!(l.expires_at, 1_120, "the TTL rolls forward on each claim");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A reload picks up a `manager define` that ran beside the daemon — the
    /// whole reason `define` needs no authenticated op.
    #[test]
    fn reload_picks_up_an_external_define() {
        let dir = tmp_state("reload");
        define_deployment(&dir, "prod", &["m1:1".into()], &[]).unwrap();
        let m = manager(&dir, false);
        assert!(m.build_roster("lab", 1).is_err());
        // An out-of-process `manager define` writes the same file…
        std::thread::sleep(std::time::Duration::from_millis(1200));
        define_deployment(&dir, "lab", &["m9:1".into()], &[]).unwrap();
        m.reload_if_changed();
        assert_eq!(m.build_roster("lab", 1).expect("reloaded").meta, vec!["m9:1"]);
        std::fs::remove_dir_all(&dir).ok();
    }
}

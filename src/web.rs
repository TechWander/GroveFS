//! The WEB CONSOLE: `grovefs web`, a standalone gateway in front of
//! `meta-server --control-listen`.
//!
//! Why a separate process and not a listener inside the meta-server: the
//! meta-server is a data-path daemon (FUSE-backed RPC, one worker pool per
//! mount, an HA sender thread). Putting an HTTP stack in it would mean a slow
//! browser, a hung `/api/call` or an HTTP-level exploit is inside the process
//! every live mount depends on, and it would force every client build to carry
//! those dependencies. Here the blast radius is one process holding exactly one
//! credential, which can be restarted, firewalled to localhost, or not started
//! at all — `meta-server` runs identically with or without it.
//!
//! What this module is, precisely: a TRANSLATION layer and a POLICY layer, and
//! nothing else.
//!
//! - translation: cookie session → the `account`/`ip` fields the control plane
//!   already understands ([`ControlClient::with_actor`]), plus the two routes
//!   that are not a single call (a metadata dump as a file download, and an
//!   uploaded dump fed into the chunked importer).
//! - policy: who may sign in from where (per-IP throttle), which ops the console
//!   exposes at all ([`ALLOWED_OPS`]), CSRF, and the response headers a browser
//!   needs. The GOVERNANCE ACL — which account may touch which view — is NOT
//!   duplicated here; it lives with the data, in `server_ext::gates`, because a
//!   second copy of an authorization decision in a second process is one refactor
//!   away from being wrong. This file only ever REMOVES capability: an op the
//!   console refuses to route cannot be reached from a browser even where the
//!   server would allow it.
//!
//! The trust model, stated plainly: the gateway holds an `admin`-scope key, so
//! anyone who can read this process's configuration already governs the
//! deployment. Signing in does not create capability, it LIMITS and ATTRIBUTES
//! it — a browser gets a session, a session acts as one account, and every call
//! is audited as `account@ip`. That is why the default bind is loopback, why a
//! routable bind needs `--allow-insecure-http` (this server speaks plain HTTP:
//! TLS belongs to the reverse proxy in front of it, or to the network), and why
//! passwords are verified by the META-SERVER: the digests are in its store, so a
//! gateway that is a pure translation layer cannot be handed the password
//! database by implication.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tiny_http::{Header, Method, Request, Response, Server};

use crate::control_remote::ControlClient;

/// The single-page app, embedded in the binary at build time. Embedded because
/// "open the management URL" must work on a host with no network access and no
/// `/usr/share/grovefs`; a `--assets DIR` flag would only add a way for the
/// served UI to disagree with the binary that answers it.
const INDEX_HTML: &str = include_str!("webui/index.html");
const APP_CSS: &str = include_str!("webui/app.css");
const APP_JS: &str = include_str!("webui/app.js");
const FAVICON_SVG: &str = include_str!("webui/favicon.svg");

/// What the console listens on unless told otherwise. Loopback on a
/// non-privileged port: the operator points a browser at the host the gateway
/// runs on, and nothing is exposed to anyone else until asked.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:8787";

/// A session unused for this long is dropped. 8h is a working day, not a week:
/// the console mints api keys and changes roles, so an unattended browser is not
/// the same as an unattended `ssh` session and gets the shorter leash. The
/// account store, not this number, is the authority on whether a person may act
/// at all — a revoked delegation or a disabled account takes effect on that
/// session's NEXT call, because the server re-reads the principal per request.
pub const DEFAULT_TTL: Duration = Duration::from_secs(8 * 3600);

/// The cookie name. Deliberately not `sessionid`: on a host running several
/// management consoles, which one set a cookie has to be readable in the
/// browser's own storage inspector.
const COOKIE: &str = "gfs_sid";

/// Cap on a JSON request body. A governance call's arguments are names, labels
/// and short texts; the one op that carries bulk text (`def.put`, a view
/// definition) is a few KB. Anything larger is a mistake or an attack.
const MAX_JSON_BODY: usize = 4 << 20;

/// Cap on an uploaded metadata dump. The importer is chunked on the WIRE (see
/// `control_remote`), but a browser hands us the whole file in one body, so the
/// gateway has to spool it once. A larger dump should go through
/// `grovefs view import --in`, which streams from a local path.
const MAX_DUMP_BODY: usize = 512 << 20;

/// Failed sign-ins tolerated per client IP inside [`FAIL_WINDOW`] before the
/// throttle engages. Five is "someone mistyped twice", not a brute force.
const MAX_FAILS: u32 = 5;
const FAIL_WINDOW: Duration = Duration::from_secs(15 * 60);
const BASE_BLOCK: Duration = Duration::from_secs(30);
const MAX_BLOCK: Duration = Duration::from_secs(30 * 60);

/// How long `fails` failures should block an IP. Pure, so the growth curve is
/// testable without a clock and a socket.
fn block_for(fails: u32) -> Duration {
    let over = fails.saturating_sub(MAX_FAILS).min(10);
    let secs = BASE_BLOCK
        .as_secs()
        .saturating_mul(1u64 << over)
        .min(MAX_BLOCK.as_secs());
    Duration::from_secs(secs)
}

/// The ops a browser may reach through `POST /api/call`. The server authorizes
/// most of these anyway; this list is the console's own promise about what its
/// users can click, and the reason four destructive verbs are absent:
///
/// - `gc.scan`/`gc.apply` and `refs.rebuild` walk every byte root and the whole
///   ledger, take minutes, and `gc --apply` deletes CONTENT objects. The CLI
///   refuses to run them while a view is mounted, prints a dry-run report first,
///   and a page refresh in the middle of either would leave the caller unsure
///   whether it happened at all.
/// - `view.rm_base` destroys a physical tree (and under SQLite, its objects).
///   Nothing about a web form makes that safer than the CLI's typed-name
///   confirmation.
/// - `fork.graft` takes a plan FILE of per-path moves — a file upload with no
///   interesting UI until someone needs one.
///
/// Retention is NOT absent any more, deliberately: an unbounded `mcp_calls`
/// table is an operational failure the admin has to notice by accident, so the
/// console exposes the window (`audit.retention.get/set`) and an
/// apply-against-the-current-window cut (`apikey.purge`, gated `Super` and
/// audited like every call). Setting a shorter window trims immediately — the
/// same cut `--audit-retention` performs at startup, from the page instead of
/// the CLI.
///
/// Present and worth naming: `view.create` (standing up a base view is exactly
/// what a management page is for, and the server requires an administrator),
/// `def.put`/`fork.create` (a delegated account's own lens and fork work), and
/// the `account.*`/`grant.*` plane. The `storage.*` plane is here for the same
/// reason `view.create` is: instance registration is the deployment decision an
/// operator makes WHILE the server runs — the server gates every one of them
/// `Super`, the guards in `pg_store::registry_*` refuse an unsafe removal (a
/// control plane, an instance with placed views), and the reply never carries
/// more authority than the gate already checked. `login` is NOT here — it is
/// its own route, because it runs before there is a session to authorize with.
const ALLOWED_OPS: &[&str] = &[
    "info",
    "whoami",
    "store.info",
    "def.list",
    "def.get",
    "def.put",
    "def.remove",
    "physical.list",
    "stamp",
    "fork.has",
    "fork.create",
    "fork.drop",
    "view.create",
    // Pure read of the placement rule, so the create form can say WHICH instance
    // the default pick means instead of leaving the operator to guess it.
    "view.plan",
    "view.stats",
    "storage.add",
    "storage.update",
    "storage.remove",
    "storage.preview",
    "storage.set_control",
    "storage.relocate",
    "apikey.list",
    "apikey.create",
    "apikey.revoke",
    "apikey.delete",
    "apikey.rotate",
    // Read a stored credential back. In the whitelist because the console offers
    // the verb; gated by `Gate::Key` server-side (its owner or a super admin),
    // and every call is audited with the target id.
    "apikey.secret",
    "apikey.set_ro",
    "apikey.update",
    "apikey.calls",
    "apikey.purge",
    "audit.retention.get",
    "audit.retention.set",
    "account.list",
    "account.create",
    "account.passwd",
    "account.role",
    "account.disable",
    "account.delete",
    "grant.list",
    "grant.add",
    "grant.remove",
];

/// Is `op` routable from a browser? Exact match, so a prefix trick
/// (`apikey.list/../gc.apply`) can never satisfy it.
fn allowed_op(op: &str) -> bool {
    ALLOWED_OPS.contains(&op)
}

/// Everything `serve` needs, resolved by `main` from flags + environment.
pub struct Config {
    /// `HOST:PORT` to bind.
    pub listen: String,
    /// A client template holding the control address and the `admin`-scope key.
    /// Never used as-is for a user call: every one goes through
    /// [`Gateway::actor`].
    pub control: ControlClient,
    pub session_ttl: Duration,
    /// Take the client address from `X-Forwarded-For` instead of the socket.
    /// Only correct behind a proxy that SETS and SANITIZES it — which is why it
    /// is a flag and not a default: trusting a header the client can forge
    /// would turn the sign-in throttle and the audit line into decorations.
    pub xff: bool,
    /// Worker threads, i.e. how many console requests may be in flight. A
    /// control call blocks its thread, so this is a concurrency cap and a
    /// memory cap at once; further requests wait in tiny_http's own queue.
    pub workers: usize,
    /// Bind a routable address over plain HTTP anyway. See [`serve`]'s refusal.
    pub allow_insecure_http: bool,
}

impl Config {
    /// The bind address's IP, for the loopback test. Done by resolving rather
    /// than by string comparison so that `localhost`, `127.0.0.1` and
    /// `[::1]:8787` are all recognized as local and `0.0.0.0` is not.
    fn is_loopback(&self) -> bool {
        listen_ip(&self.listen).is_some_and(|ip| ip.is_loopback())
    }
}

fn listen_ip(listen: &str) -> Option<std::net::IpAddr> {
    if let Ok(a) = listen.parse::<SocketAddr>() {
        return Some(a.ip());
    }
    let host = listen.rsplit_once(':').map(|(h, _)| h).unwrap_or(listen);
    if let Ok(a) = host.parse::<std::net::IpAddr>() {
        return Some(a);
    }
    // A name: resolve it the way the bind will, so `localhost:8787` does not
    // need `--allow-insecure-http` to say it is local.
    std::net::ToSocketAddrs::to_socket_addrs(&format!("{host}:0"))
        .ok()
        .and_then(|mut it| it.next())
        .map(|a| a.ip())
}

/// One signed-in browser. `Clone` so a request can read it, release the lock,
/// and only then block on a network call: the session map must never be held
/// across a control-plane round trip, or one slow governance call on a PG
/// backend would stall every other tab in the process.
#[derive(Clone)]
struct Session {
    account: String,
    /// Handed to the page as JSON and required back in a header. The cookie
    /// alone proves "this browser has a session"; the token proves "this request
    /// came from the page we served", which a cross-origin form does not have.
    /// SameSite=Strict already refuses to SEND the cookie cross-site, so these
    /// are two independent locks on the same door.
    csrf: String,
    last: Instant,
}

/// Per-IP sign-in failures. Counted in the gateway because the gateway is what
/// sees the browser; the meta-server sees only one client (this process), and
/// attributing a spray to it would be both useless and wrong.
struct Attempt {
    fails: u32,
    first: Instant,
    blocked_until: Option<Instant>,
}

/// A response, assembled in one place so that no route can forget the security
/// headers.
struct Reply {
    status: u16,
    /// `None` when `file` is set: then the body is streamed from disk.
    body: Option<Vec<u8>>,
    mime: &'static str,
    file: Option<PathBuf>,
    headers: Vec<(String, String)>,
}

impl Reply {
    fn json(status: u16, v: Value) -> Self {
        Self {
            status,
            body: Some(serde_json::to_vec(&v).unwrap_or_default()),
            mime: "application/json; charset=utf-8",
            file: None,
            headers: Vec::new(),
        }
    }
    fn body(status: u16, mime: &'static str, body: Vec<u8>) -> Self {
        Self {
            status,
            body: Some(body),
            mime,
            file: None,
            headers: Vec::new(),
        }
    }
    /// A governance refusal. `ok:false` with the server's own sentence — see
    /// [`Gateway::call`] for why that is a 200.
    fn fail(msg: impl Into<String>) -> Self {
        Reply::json(200, json!({"ok": false, "error": msg.into()}))
    }
    fn error(status: u16, msg: impl Into<String>) -> Self {
        Reply::json(status, json!({"ok": false, "error": msg.into()}))
    }
    fn file(path: PathBuf, filename: &str) -> Self {
        Self {
            status: 200,
            body: None,
            mime: "application/octet-stream",
            file: Some(path),
            // Quoted, and built from a name that already passed
            // `valid_view_name`, so no quote or separator can reach it.
            headers: vec![(
                "Content-Disposition".into(),
                format!("attachment; filename=\"{filename}\""),
            )],
        }
    }
    fn with_header(mut self, name: &str, value: String) -> Self {
        self.headers.push((name.to_string(), value));
        self
    }
}

/// The gateway's whole state: its config plus the two tables it owns.
struct Gateway {
    cfg: Config,
    sessions: Mutex<HashMap<String, Session>>,
    attempts: Mutex<HashMap<String, Attempt>>,
}

fn token() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Serve the console. Blocks for the lifetime of the process on success: the
/// accept loop IS the foreground work of `grovefs web`.
pub fn serve(cfg: Config) -> Result<(), Box<dyn std::error::Error>> {
    // Checked BEFORE the bind, so that a unit file which sets both a routable
    // address and the flag starts, and one that forgets the flag fails without
    // ever having opened a port.
    if !cfg.is_loopback() && !cfg.allow_insecure_http {
        return Err(format!(
            "refusing to bind {} — this console speaks plain HTTP, so a routable address puts \
             every sign-in and every governance call on the wire unencrypted. Either bind \
             127.0.0.1 and terminate TLS in a reverse proxy (and pass --use-x-forwarded-for so \
             sign-ins are throttled per CLIENT rather than per proxy), or pass \
             --allow-insecure-http if this network is trusted end to end.",
            cfg.listen
        )
        .into());
    }
    // Probe the plane before binding: a wrong key or an unreachable
    // `--control-listen` must fail the START, with the server's own sentence,
    // rather than fail the first person who tries to sign in.
    // As an OPERATOR (no actor): this is the gateway checking its own
    // credential, not a user acting, and `info` is the cheapest call that
    // authenticates AND names the backend.
    let info = cfg.control.call("info", json!({})).map_err(|e| {
        format!(
            "cannot reach the control plane at {} with this key: {e} The console needs \
             `meta-server --control-listen HOST:PORT` and an `admin`-scope account key \
             (`grovefs apikey create --for admin --owner <account>`), supplied to this process \
             by --api-key / --api-key-file / $GROVEFS_CONTROL_KEY.",
            cfg.control.addr()
        )
    })?;
    let server = Arc::new(Server::http(cfg.listen.as_str()).map_err(|e| {
        format!(
            "cannot listen on {}: {e} — is another console (or anything else) on that port?",
            cfg.listen
        )
    })?);
    let gw = Arc::new(Gateway {
        cfg,
        sessions: Mutex::new(HashMap::new()),
        attempts: Mutex::new(HashMap::new()),
    });

    println!(
        "grovefs web: management console on http://{}/",
        gw.cfg.listen
    );
    println!(
        "  control plane : {} (backend {})",
        gw.cfg.control.addr(),
        info.get("backend").and_then(Value::as_str).unwrap_or("?")
    );
    println!(
        "  sign-in       : accounts of that server's web store; the first owner comes from its \
         --admin-user flag"
    );
    println!(
        "  sessions      : {}h idle window, {}",
        gw.cfg.session_ttl.as_secs() / 3600,
        crate::cli_style::plural(
            gw.cfg.workers.max(1) as usize,
            "worker thread",
            "worker threads"
        )
    );
    if !gw.cfg.is_loopback() {
        println!(
            "  WARNING       : bound to a NON-LOOPBACK address over plain HTTP — credentials and \
             governance calls cross the wire in the clear. Terminate TLS in front of this."
        );
    }

    // Expire idle sessions without waiting for their owner to come back: a
    // abandoned tab should not leave a usable cookie in the table forever.
    {
        let g = Arc::clone(&gw);
        thread::spawn(move || loop {
            thread::sleep(Duration::from_secs(60));
            g.sweep();
        });
    }

    let mut threads = Vec::with_capacity(gw.cfg.workers.max(1));
    for _ in 0..gw.cfg.workers.max(1) {
        let g = Arc::clone(&gw);
        let s = Arc::clone(&server);
        threads.push(thread::spawn(move || {
            // `incoming_requests` pops from the server's shared queue, so N
            // iterators over one server are N independent workers — no hand-off
            // of our own to get wrong.
            for rq in s.incoming_requests() {
                g.handle(rq);
            }
        }));
    }
    for t in threads {
        let _ = t.join();
    }
    Ok(())
}

impl Gateway {
    fn handle(&self, mut rq: Request) {
        let url = rq.url().to_string();
        let (path, query) = match url.split_once('?') {
            Some((p, q)) => (p, q.to_string()),
            None => (url.as_str(), String::new()),
        };
        let method = rq.method().clone();
        let ip = self.client_ip(&rq);
        let reply = match (method, path) {
            (Method::Get | Method::Head, "/") | (Method::Get | Method::Head, "/index.html") => {
                Reply::body(
                    200,
                    "text/html; charset=utf-8",
                    INDEX_HTML.as_bytes().to_vec(),
                )
            }
            (Method::Get, "/app.css") => {
                Reply::body(200, "text/css; charset=utf-8", APP_CSS.as_bytes().to_vec())
            }
            (Method::Get, "/app.js") => Reply::body(
                200,
                "application/javascript; charset=utf-8",
                APP_JS.as_bytes().to_vec(),
            ),
            (Method::Get, "/favicon.svg") => {
                Reply::body(200, "image/svg+xml", FAVICON_SVG.as_bytes().to_vec())
            }
            // Browsers request /favicon.ico by default even when a `link rel=icon`
            // points at the SVG; a 204 keeps that miss out of the console.
            (Method::Get, "/favicon.ico") => {
                Reply::body(204, "text/plain; charset=utf-8", Vec::new())
            }
            (Method::Post, "/api/login") => self.login(&mut rq, &ip),
            (Method::Post, "/api/logout") => self.logout(&mut rq),
            (Method::Get, "/api/session") => self.session(&rq, &ip),
            (Method::Post, "/api/call") => self.call(&mut rq, &ip),
            (Method::Get, "/api/export") => self.export(&rq, &query, &ip),
            (Method::Post, "/api/import") => self.import(&mut rq, &query, &ip),
            (m, other) if other.starts_with("/api/") => Reply::error(
                405,
                format!(
                    "{other} is not a console route for {m:?} (routes: POST /api/login, \
                        POST /api/logout, GET /api/session, POST /api/call, GET /api/export, \
                        POST /api/import)"
                ),
            ),
            (m, other) => Reply::error(404, format!("no console page at {m:?} {other}")),
        };
        send(rq, reply);
    }

    // ---- sessions --------------------------------------------------------

    /// The session behind this request, if it is present, known and unexpired;
    /// refreshes the idle window when it is.
    fn auth(&self, rq: &Request) -> Option<Session> {
        let sid = cookie(rq, COOKIE)?;
        let mut map = self.sessions.lock().ok()?;
        let now = Instant::now();
        let s = map.get_mut(&sid)?;
        if now.checked_duration_since(s.last)? >= self.cfg.session_ttl {
            // Expired HERE and now, rather than at the next sweep: an expired
            // cookie must never be accepted by coincidence of timing.
            map.remove(&sid);
            return None;
        }
        s.last = now;
        Some(s.clone())
    }

    fn sweep(&self) {
        let ttl = self.cfg.session_ttl;
        if let Ok(mut m) = self.sessions.lock() {
            m.retain(|_, s| s.last.elapsed() < ttl);
        }
        // Failure counters are only interesting while they can still block
        // someone; a month of distinct IPs is not worth keeping.
        if let Ok(mut m) = self.attempts.lock() {
            m.retain(|_, a| a.first.elapsed() < FAIL_WINDOW * 4);
        }
    }

    // ---- sign-in throttle --------------------------------------------------

    /// How long this IP still has to wait, if it is blocked.
    fn blocked(&self, ip: &str) -> Option<Duration> {
        let until = self
            .attempts
            .lock()
            .ok()?
            .get(ip)
            .and_then(|a| a.blocked_until)?;
        until.checked_duration_since(Instant::now())
    }

    fn record_fail(&self, ip: &str) {
        let Ok(mut m) = self.attempts.lock() else {
            return;
        };
        let now = Instant::now();
        let e = m.entry(ip.to_string()).or_insert(Attempt {
            fails: 0,
            first: now,
            blocked_until: None,
        });
        if e.first.elapsed() > FAIL_WINDOW {
            e.fails = 0;
            e.first = now;
            e.blocked_until = None;
        }
        e.fails += 1;
        if e.fails >= MAX_FAILS {
            // Exponential and capped: enough pressure that a password spray
            // costs days, never enough to lock out the real person for a whole
            // shift after a colleague's burst of typos.
            e.blocked_until = Some(now + block_for(e.fails));
        }
    }

    // ---- routes ------------------------------------------------------------

    fn login(&self, rq: &mut Request, ip: &str) -> Reply {
        if let Some(wait) = self.blocked(ip) {
            let secs = wait.as_secs().max(1);
            return Reply::json(
                429,
                json!({
                    "ok": false,
                    "error": format!("too many failed sign-ins from this address; try again in {secs}s"),
                    "retryAfter": secs,
                }),
            );
        }
        let body = match read_body(rq, MAX_JSON_BODY) {
            Ok(b) => b,
            Err(e) => return Reply::error(400, e),
        };
        let v: Value = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(_) => return Reply::error(400, "expected a JSON body with account and password"),
        };
        let account = v.get("account").and_then(Value::as_str).unwrap_or("");
        let password = v.get("password").and_then(Value::as_str).unwrap_or("");
        if account.is_empty() || password.is_empty() {
            return Reply::error(400, "account and password are both required");
        }
        // NO actor on this call: sign-in is answered before there is a principal
        // to authorize, and the server's `login` gate is exactly that case. The
        // password is forwarded and never stored here — the digests live in the
        // meta-server's store, which is also why verification cannot happen in
        // this process.
        match self
            .cfg
            .control
            .call("login", json!({"account": account, "password": password}))
        {
            Ok(r) => {
                let sid = token();
                let csrf = token();
                let acct = r
                    .get("account")
                    .and_then(Value::as_str)
                    .unwrap_or(account)
                    .to_string();
                if let Ok(mut m) = self.sessions.lock() {
                    m.insert(
                        sid.clone(),
                        Session {
                            account: acct.clone(),
                            csrf: csrf.clone(),
                            last: Instant::now(),
                        },
                    );
                }
                // A successful sign-in clears the counter: the throttle is
                // against attackers, and one more reason for a legitimate user
                // to be slowly locked out is one too many.
                if let Ok(mut m) = self.attempts.lock() {
                    m.remove(ip);
                }
                let max_age = self.cfg.session_ttl.as_secs();
                Reply::json(
                    200,
                    json!({
                        "ok": true,
                        "account": acct,
                        "role": r.get("role").cloned().unwrap_or(Value::Null),
                        "bases": r.get("bases").cloned().unwrap_or_else(|| json!([])),
                        "csrf": csrf,
                    }),
                )
                .with_header(
                    "Set-Cookie",
                    format!("{COOKIE}={sid}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age}"),
                )
            }
            Err(e) => {
                // One wording for "no such account" and "wrong password" is the
                // server's job and it does it (`WebAuthStore::login`); nothing
                // here may improve on that by leaking which one happened.
                self.record_fail(ip);
                Reply::error(401, e)
            }
        }
    }

    fn logout(&self, rq: &mut Request) -> Reply {
        if let Some(sid) = cookie(rq, COOKIE) {
            if let Ok(mut m) = self.sessions.lock() {
                m.remove(&sid);
            }
        }
        Reply::json(200, json!({"ok": true}))
            // Max-Age=0 is how a cookie is actually deleted from a browser.
            .with_header(
                "Set-Cookie",
                format!("{COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0"),
            )
    }

    /// `GET /api/session`: the shell's "am I still signed in, and as whom". It
    /// asks the SERVER (`whoami`) instead of answering from the session table, so
    /// a disabled account or a withdrawn delegation shows up on the next page
    /// load rather than only when the user presses a button.
    fn session(&self, rq: &Request, ip: &str) -> Reply {
        let Some(s) = self.auth(rq) else {
            return Reply::json(401, json!({"ok": false, "signedIn": false}));
        };
        match self.actor(&s, ip).call("whoami", json!({})) {
            Ok(r) => Reply::json(
                200,
                json!({
                    "ok": true,
                    "signedIn": true,
                    "csrf": s.csrf,
                    "who": r,
                    // The page renders its own buttons from this list, so the
                    // console's promise and its UI cannot drift apart.
                    "ops": ALLOWED_OPS,
                }),
            ),
            Err(e) => {
                // The account is deleted, disabled, or the plane is unreachable;
                // in every case the page must stop behaving as signed in.
                if let (Some(sid), Ok(mut m)) = (cookie(rq, COOKIE), self.sessions.lock()) {
                    m.remove(&sid);
                }
                Reply::json(401, json!({"ok": false, "signedIn": false, "error": e}))
            }
        }
    }

    /// `POST /api/call` `{"op":…,"args":{…}}` → the control plane, as this
    /// session's account.
    fn call(&self, rq: &mut Request, ip: &str) -> Reply {
        let Some(s) = self.auth(rq) else {
            return Reply::error(401, "not signed in");
        };
        if !csrf_ok(rq, &s) {
            return Reply::error(403, "missing or wrong X-GroveFS-CSRF header");
        }
        if !origin_ok(rq) {
            return Reply::error(403, "cross-origin request refused");
        }
        let body = match read_body(rq, MAX_JSON_BODY) {
            Ok(b) => b,
            Err(e) => return Reply::error(400, e),
        };
        let v: Value = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(_) => return Reply::error(400, "expected a JSON body {\"op\":…, \"args\":{…}}"),
        };
        let Some(op) = v.get("op").and_then(Value::as_str) else {
            return Reply::error(400, "the call needs an \"op\"");
        };
        if op == "login" {
            return Reply::error(403, "sign-in is POST /api/login, not a routed op");
        }
        if !allowed_op(op) {
            return Reply::error(
                403,
                format!("'{op}' is not available from the console: it stays an operator CLI command, where its dry-run report and confirmations are part of calling it"),
            );
        }
        let args = v.get("args").cloned().unwrap_or_else(|| json!({}));
        if !args.is_object() {
            return Reply::error(400, "\"args\" must be a JSON object");
        }
        match self.actor(&s, ip).call(op, args) {
            Ok(r) => Reply::json(200, json!({"ok": true, "result": r})),
            // HTTP 200 + `ok:false` ON PURPOSE. A refusal here is usually the
            // server's ACL sentence ("account 'x' is not an administrator, and
            // 'view.create' is not per-view governance: …") — the most useful
            // text in the product for the person who just pressed the button.
            // Making it a 4xx would let the browser's console swallow it and the
            // page show "HTTP 403" instead of a reason.
            Err(e) => Reply::fail(e),
        }
    }

    /// `GET /api/export?view=V&csrf=T`: a view's metadata dump as a download.
    /// Read-only, yet still token-checked, because a cross-site `<img>` that made
    /// the server render and ship a whole tree would be a cheap way to leak its
    /// structure — and because the check costs one `if`.
    fn export(&self, rq: &Request, query: &str, ip: &str) -> Reply {
        let Some(s) = self.auth(rq) else {
            return Reply::error(401, "not signed in");
        };
        let q = parse_query(query);
        if !csrf_matches(&q, &s) || !origin_ok(rq) {
            return Reply::error(403, "missing or wrong csrf parameter");
        }
        let view = match view_arg(q.get("view").cloned()) {
            Ok(v) => v,
            Err(e) => return Reply::error(400, e),
        };
        let path = temp_path("export");
        // `force = true`: the target is a fresh spool this request owns, so the
        // "refuse to overwrite" guard the CLI needs has nothing to protect here.
        match self.actor(&s, ip).export_view(&view, &path, true) {
            Ok(_) => Reply::file(path.clone(), &format!("{view}.grovefs-meta.txt")),
            Err(e) => {
                let _ = std::fs::remove_file(&path);
                Reply::fail(e)
            }
        }
    }

    /// `POST /api/import?view=V&csrf=T[&as_fork=B][&replace=1]`: the body IS the
    /// dump. The gateway spools it and hands it to the same chunked client the
    /// CLI uses, so the server sees exactly what `grovefs view import --in`
    /// would send — same handles, same ordering checks, same commit verdicts, and
    /// the same ACL (`view.import.commit` refuses a new base view from anyone but
    /// an administrator, whatever the transport in front of it).
    fn import(&self, rq: &mut Request, query: &str, ip: &str) -> Reply {
        let Some(s) = self.auth(rq) else {
            return Reply::error(401, "not signed in");
        };
        let q = parse_query(query);
        if !csrf_matches(&q, &s) || !origin_ok(rq) || !csrf_ok(rq, &s) {
            return Reply::error(403, "missing or wrong csrf proof");
        }
        let view = match view_arg(q.get("view").cloned()) {
            Ok(v) => v,
            Err(e) => return Reply::error(400, e),
        };
        let as_fork = match q.get("as_fork") {
            Some(b) if !b.is_empty() => match view_arg(Some(b.clone())) {
                Ok(v) => Some(v),
                Err(e) => return Reply::error(400, e),
            },
            _ => None,
        };
        let replace = q.get("replace").map(|v| v == "1").unwrap_or(false);
        let raw = match read_body(rq, MAX_DUMP_BODY) {
            Ok(b) => b,
            Err(e) => return Reply::error(400, e),
        };
        if raw.is_empty() {
            return Reply::error(400, "the upload is empty");
        }
        // Checked HERE as well as by the importer, so a wrong-file mistake gets
        // one clear refusal instead of a mid-transfer protocol error after the
        // whole body has been pushed.
        if std::str::from_utf8(&raw).is_err() {
            return Reply::error(400, "a metadata dump must be UTF-8 text");
        }
        let path = temp_path("import");
        if let Err(e) = std::fs::write(&path, &raw) {
            return Reply::error(500, format!("cannot spool the upload: {e}"));
        }
        let out = self
            .actor(&s, ip)
            .import_view(&view, &path, replace, as_fork.as_deref());
        let _ = std::fs::remove_file(&path);
        match out {
            Ok(st) => Reply::json(
                200,
                json!({"ok": true, "result": {
                    "view": view,
                    "nodes": st.nodes,
                    "blobs": st.blobs,
                    "kind": st.kind,
                    "replaced": st.replaced,
                }}),
            ),
            Err(e) => Reply::fail(e),
        }
    }

    /// A client that speaks FOR this session: the server judges the account's
    /// delegations and audits `account@ip`. Cloned from the template per request,
    /// so one session's identity cannot leak into another's call.
    fn actor(&self, s: &Session, ip: &str) -> ControlClient {
        self.cfg
            .control
            .clone()
            .with_actor(&s.account, if ip.is_empty() { "-" } else { ip })
    }

    /// Who the audit line and the throttle name. The socket by default; with
    /// `--use-x-forwarded-for`, the first `X-Forwarded-For` entry — i.e. whatever
    /// the proxy claims, which is the operator's explicit decision to trust it.
    fn client_ip(&self, rq: &Request) -> String {
        if self.cfg.xff {
            if let Some(x) = header(rq, "x-forwarded-for") {
                if let Some(first) = x.split(',').next() {
                    let t = first.trim();
                    // Bounded and copied verbatim otherwise: this string reaches
                    // the server's audit column, and a 4 KB header must not
                    // become a 4 KB audit row.
                    if !t.is_empty() && t.len() <= 64 && t.is_ascii() {
                        return t.to_string();
                    }
                }
            }
        }
        match rq.remote_addr() {
            // TCP only, in this build: a UNIX listener has no peer address to
            // audit, which is why tiny_http reports `None` rather than a name.
            Some(SocketAddr::V4(a)) => a.ip().to_string(),
            Some(SocketAddr::V6(a)) => a.ip().to_string(),
            None => String::new(),
        }
    }
}

/// The `view`/`base` name of an export or import, validated by the SAME
/// predicate the server uses to create a view — so a browser cannot reach
/// `view.export` with a path-ish name the CLI would refuse.
fn view_arg(got: Option<String>) -> Result<String, String> {
    let v = got.unwrap_or_default();
    if v.is_empty() {
        return Err("a view name is required".to_string());
    }
    if !crate::layout::valid_view_name(&v) {
        return Err(format!("invalid view name '{v}'"));
    }
    Ok(v)
}

/// A secret compared without leaking how far the match got. The CSRF token is
/// random and its length public, so this is not the password path — but stating
/// the rule once is cheaper than arguing per call site about which comparisons
/// may short-circuit.
fn eq_const(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Case-insensitive header lookup, done by hand because tiny_http's own
/// `equiv` needs a `&'static str` and these names come from one call site each.
fn header(rq: &Request, name: &str) -> Option<String> {
    rq.headers().iter().find_map(|h| {
        h.field
            .as_str()
            .as_str()
            .eq_ignore_ascii_case(name)
            .then(|| h.value.as_str().to_string())
    })
}

/// One cookie out of the request.
fn cookie(rq: &Request, name: &str) -> Option<String> {
    let raw = header(rq, "cookie")?;
    raw.split(';').find_map(|c| {
        let (k, v) = c.trim().split_once('=')?;
        (!v.is_empty() && k == name).then(|| v.to_string())
    })
}

/// A same-origin check for the mutating routes. Browsers send `Origin` on every
/// cross-origin request and on same-origin POSTs; a same-origin `curl` sends
/// none and is refused by the CSRF requirement instead. So: if the header is
/// present it MUST match the `Host` the browser used.
fn origin_ok(rq: &Request) -> bool {
    let Some(origin) = header(rq, "origin") else {
        return true;
    };
    let host = header(rq, "host").unwrap_or_default();
    let origin_host = origin
        .split("://")
        .nth(1)
        .unwrap_or(origin.as_str())
        .trim_end_matches('/');
    !host.is_empty() && origin_host == host
}

fn csrf_ok(rq: &Request, s: &Session) -> bool {
    let got = header(rq, "x-grovefs-csrf").unwrap_or_default();
    eq_const(got.as_bytes(), s.csrf.as_bytes())
}

fn parse_query(q: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for pair in q.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        out.insert(decode(k), decode(v));
    }
    out
}

fn csrf_matches(q: &HashMap<String, String>, s: &Session) -> bool {
    q.get("csrf")
        .is_some_and(|v| eq_const(v.as_bytes(), s.csrf.as_bytes()))
}

/// Percent-decode a query component: `%XX` and `+` only, which is all a view
/// name or a hex token can need.
fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(b) => {
                        out.push(b);
                        i += 3;
                    }
                    None => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Read a request body with a hard ceiling. `Content-Length` is honoured when
/// present (so an oversized upload is refused before it is read) and the counter
/// is enforced either way, which also covers a chunked body.
fn read_body(rq: &mut Request, max: usize) -> Result<Vec<u8>, String> {
    if let Some(len) = rq.body_length() {
        if len > max {
            return Err(format!(
                "the body is {len} bytes, over this console's {max}-byte limit"
            ));
        }
    }
    let mut buf = Vec::with_capacity(rq.body_length().unwrap_or(0).min(max));
    let mut tmp = [0u8; 64 << 10];
    loop {
        let n = rq
            .as_reader()
            .read(&mut tmp)
            .map_err(|e| format!("cannot read the request body: {e}"))?;
        if n == 0 {
            return Ok(buf);
        }
        if buf.len() + n > max {
            return Err(format!(
                "the request body is over this console's {max}-byte limit"
            ));
        }
        buf.extend_from_slice(&tmp[..n]);
    }
}

/// A spool file for one request, removed by the responder or the error path.
fn temp_path(what: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "grovefs-web-{}-{}-{}",
        std::process::id(),
        what,
        token()
    ))
}

/// Write the reply, adding the headers no route may omit. Takes the request by
/// value because that is what tiny_http's `respond` consumes, and boxes the body
/// reader so that a JSON reply (a cursor over bytes) and a metadata export (a
/// spool `File`, streamed, so a large dump does not have to fit in this
/// process's RAM twice) get the SAME header treatment from one place.
fn send(rq: Request, reply: Reply) {
    let Reply {
        status,
        body,
        mime,
        file,
        headers,
    } = reply;
    fn h(name: &str, value: &str) -> Option<Header> {
        Header::from_bytes(name.as_bytes(), value.as_bytes()).ok()
    }
    let mut resp = match (body, &file) {
        (Some(b), _) => Response::from_data(b).boxed(),
        (None, Some(path)) => match std::fs::File::open(path) {
            Ok(f) => Response::from_file(f).boxed(),
            Err(_) => {
                // The spool vanished mid-request (a tmpfs reaper, an operator):
                // answer 500 rather than a header-only 200 with no body.
                let msg = serde_json::to_vec(&json!({
                    "ok": false,
                    "error": "the export spool disappeared; try the download again"
                }))
                .unwrap_or_default();
                let mut e = Response::from_data(msg).boxed().with_status_code(500);
                if let Some(x) = h("Content-Type", "application/json; charset=utf-8") {
                    e = e.with_header(x);
                }
                let _ = rq.respond(e);
                return;
            }
        },
        (None, None) => Response::from_data(Vec::new()).boxed(),
    };
    resp = resp.with_status_code(status);
    let mut all: Vec<(String, String)> = vec![
        ("Content-Type".into(), mime.to_string()),
        // A management console has nothing to cache: a stale app.js after an
        // upgrade would show buttons for ops the binary no longer has.
        ("Cache-Control".into(), "no-store".into()),
        ("X-Content-Type-Options".into(), "nosniff".into()),
        ("X-Frame-Options".into(), "DENY".into()),
        ("Referrer-Policy".into(), "no-referrer".into()),
        // The SPA is two embedded files and same-origin fetches, so everything
        // locks to `'self'` — and `'none'` for the shapes that must not exist
        // here at all. No `unsafe-eval`: there is no bundler and no template
        // compiler in this UI, so a policy that allows them would be paying for
        // an attack surface the page cannot need.
        (
            "Content-Security-Policy".into(),
            "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; \
             connect-src 'self'; form-action 'none'; frame-ancestors 'none'; base-uri 'none'"
                .into(),
        ),
    ];
    all.extend(headers);
    for (name, value) in all {
        if let Some(hdr) = h(&name, &value) {
            resp = resp.with_header(hdr);
        }
    }
    let _ = rq.respond(resp);
    // `respond` writes the body synchronously, so the spool may go now.
    if let Some(path) = file {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The console's whitelist and the server's ACL table are lists of the SAME
    /// ops, maintained in two files. A typo here would be invisible until a user
    /// clicked the button, so: every routed op must have a rule.
    #[test]
    fn every_routed_op_has_an_access_rule() {
        for op in ALLOWED_OPS {
            assert!(
                crate::remote::meta::control_op_known(op),
                "'{op}' is routed by the console but has no gate on the server",
            );
        }
    }

    /// The verbs deliberately left out of the browser's reach. If someone
    /// adds one back, this fails and the argument above has to be re-read.
    /// `apikey.purge` USED to be on this list — the retention UI overrode that
    /// call — which is why it is asserted PRESENT below instead.
    #[test]
    fn destructive_ops_are_not_routable() {
        for op in [
            "gc.scan",
            "gc.apply",
            "refs.rebuild",
            "view.rm_base",
            "fork.graft",
            "login",
        ] {
            assert!(!allowed_op(op), "{op} must stay out of the console");
        }
        assert!(allowed_op("apikey.purge"));
        assert!(allowed_op("audit.retention.get"));
        assert!(allowed_op("audit.retention.set"));
    }

    #[test]
    fn unknown_ops_are_not_routable() {
        assert!(!allowed_op("apikey.list/../gc.apply"));
        assert!(!allowed_op(""));
        assert!(!allowed_op("INFO"));
        assert!(allowed_op("apikey.list"));
    }

    /// The throttle curve: starts gentle, ends bounded. An unbounded backoff is
    /// a self-inflicted outage; a bounded-at-30s one is not a throttle.
    #[test]
    fn sign_in_backoff_grows_and_stays_bounded() {
        assert_eq!(block_for(MAX_FAILS), BASE_BLOCK);
        assert_eq!(block_for(MAX_FAILS + 1), BASE_BLOCK * 2);
        assert_eq!(block_for(MAX_FAILS + 4), BASE_BLOCK * 16);
        assert_eq!(block_for(1000), MAX_BLOCK);
    }

    #[test]
    fn loopback_is_loopback() {
        for a in ["127.0.0.1:8787", "[::1]:8787", "localhost:8787"] {
            assert!(
                Config {
                    listen: a.to_string(),
                    control: ControlClient::new("x", "y"),
                    session_ttl: DEFAULT_TTL,
                    xff: false,
                    workers: 4,
                    allow_insecure_http: false,
                }
                .is_loopback(),
                "{a} must count as local",
            );
        }
        for a in ["0.0.0.0:8787", "10.0.0.5:8787"] {
            assert!(
                !Config {
                    listen: a.to_string(),
                    control: ControlClient::new("x", "y"),
                    session_ttl: DEFAULT_TTL,
                    xff: false,
                    workers: 4,
                    allow_insecure_http: false,
                }
                .is_loopback(),
                "{a} is not local",
            );
        }
    }

    #[test]
    fn query_and_cookies_parse() {
        let q = parse_query("view=main&csrf=abc%20d&x+y=1&empty");
        assert_eq!(q.get("view").map(String::as_str), Some("main"));
        assert_eq!(q.get("csrf").map(String::as_str), Some("abc d"));
        assert_eq!(q.get("x y").map(String::as_str), Some("1"));
        assert_eq!(q.get("empty").map(String::as_str), Some(""));
    }

    #[test]
    fn csrf_and_names_are_strict() {
        let s = Session {
            account: "a".into(),
            csrf: "tok".into(),
            last: Instant::now(),
        };
        // Exact length AND exact bytes: a prefix, an extension and an empty
        // header are all refusals, so a token can never be "nearly" right.
        assert!(eq_const("tok".as_bytes(), s.csrf.as_bytes()));
        assert!(!eq_const("".as_bytes(), s.csrf.as_bytes()));
        assert!(!eq_const("to".as_bytes(), s.csrf.as_bytes()));
        assert!(!eq_const("toke".as_bytes(), s.csrf.as_bytes()));
        assert!(!eq_const("tok".as_bytes(), b"top"));
        assert!(view_arg(Some("good-name_1".into())).is_ok());
        assert!(view_arg(Some("../etc/passwd".into())).is_err());
        assert!(view_arg(Some(String::new())).is_err());
        assert!(view_arg(None).is_err());
    }

    #[test]
    fn spool_files_are_unique_per_call() {
        let a = temp_path("export");
        let b = temp_path("export");
        assert_ne!(a, b);
        assert!(a
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("grovefs-web-"));
    }
}

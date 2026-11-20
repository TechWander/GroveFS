//! The `MetaServer` half of [`crate::remote::meta`]: role/HA state, the
//! central runtime that owns per-view [`MetaEngine`]s, the accept/serve loops,
//! snapshot replication and the control-plane handlers. Gated behind `native`
//! — clients (including the Windows `grovefs-win` mount from the sibling
//! `grovefs-client` repo) compile only
//! [`RemoteMeta`] from the parent module.

use super::*;
use crate::meta::{MetaEngine, MetaStore};
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
///
/// The third kind, behind `--features treedb`, is the EXPERIMENTAL engine
/// (`--meta-store treedb://host:port`): trees live inside a treedb server, and
/// — unlike the PG backend — the control TABLES (credentials, accounts,
/// grants) stay in local SQLite under `--meta-dir` until
/// `GROVEFS_TREEDB_CONTROL=engine` moves them into the engine's generic tables
/// (treedb DESIGN.md §10 C 4). The view DEFINITIONS and placement always stay
/// local for now, because treedb has no table for them. That split is what
/// [`StoreKind::control_tables_uri`] encodes; nothing else may assume "not
/// SQLite" means "control lives remote".
#[derive(Clone, Debug)]
pub enum StoreKind {
    Sqlite,
    Pg(String),
    /// `treedb://host:port` — the address only, parsed once at `with_store()`.
    #[cfg(feature = "treedb")]
    Treedb(String),
}

impl StoreKind {
    /// Where the CONTROL plane (api keys, web accounts, view definitions,
    /// placement) lives: `Some(uri)` means shared tables inside that PostgreSQL
    /// store, `None` means the local SQLite files and `meta/views/` under
    /// `--meta-dir`. A treedb backend returns `None` deliberately — the engine
    /// serves trees, not credentials — so every control-plane site keeps working
    /// over a treedb deployment without knowing treedb exists.
    pub(crate) fn control_uri(&self) -> Option<&str> {
        match self {
            StoreKind::Pg(u) => Some(u.as_str()),
            StoreKind::Sqlite => None,
            #[cfg(feature = "treedb")]
            StoreKind::Treedb(_) => None,
        }
    }

    /// Where the CREDENTIAL and ACCOUNT stores (`api_keys`, `api_key_usage`,
    /// `mcp_calls`, `web_accounts`, `web_grants`) live — the same answer as
    /// [`Self::control_uri`] under PG, but under treedb it turns into the engine
    /// URI **once `GROVEFS_TREEDB_CONTROL=engine` says those tables have been
    /// moved there**.
    ///
    /// Deliberately a second accessor rather than a wider `control_uri`: the
    /// view DEFINITIONS and placement still live in local files (nothing in the
    /// engine models them yet), so a treedb deployment can have every row-table
    /// in the engine while the tree-side bookkeeping stays on disk. With the
    /// switch off it answers exactly what `control_uri` does, i.e. today's
    /// local-SQLite shape.
    pub(crate) fn control_tables_uri(&self) -> Option<String> {
        match self {
            StoreKind::Pg(u) => Some(u.clone()),
            StoreKind::Sqlite => None,
            #[cfg(feature = "treedb")]
            StoreKind::Treedb(addr) => crate::apikeys::ApiKeyStore::treedb_control_enabled()
                .then(|| format!("{}{addr}", crate::treedb_store::SCHEME)),
        }
    }

    /// Backend name as the startup banner states it. Lower-case, stable, and
    /// argument-free on purpose: the URI is NEVER part of this, because a
    /// `postgres://user:pw@host/db` URI embeds a credential and the banner goes
    /// to stdout, which on a systemd deployment lands in the journal — a
    /// world-readable log. Operators who need the URI have it in their own
    /// command line / unit file already.
    pub(crate) fn backend_name(&self) -> &'static str {
        match self {
            StoreKind::Sqlite => "sqlite",
            StoreKind::Pg(_) => "postgres",
            #[cfg(feature = "treedb")]
            StoreKind::Treedb(_) => "treedb",
        }
    }

    /// Where a view's tree physically lives under this backend, in the one line
    /// an operator needs when the banner's `base` path is misleading — under
    /// SQLite `base` is the tree's home, under the others it is a config-only
    /// directory holding nothing but view definitions and logs. A `file:`
    /// registry names itself (a PATH, never a credential).
    pub(crate) fn backend_layout(&self) -> String {
        match self {
            StoreKind::Sqlite => "one db file per view under <base>/meta/".to_string(),
            StoreKind::Pg(u) if u.starts_with("file:") => format!(
                "one schema per view in PostgreSQL; instances from registry {u}; <base> holds no trees"
            ),
            StoreKind::Pg(_) => {
                "one schema per view in PostgreSQL; <base> holds no trees".to_string()
            }
            #[cfg(feature = "treedb")]
            StoreKind::Treedb(_) => {
                "one tree per view in the treedb server; <base> holds definitions and keys only"
                    .to_string()
            }
        }
    }
}

/// Everything a connection thread needs to serve a node. Wrapped in `Arc` so the
/// accept loop hands one to each spawned thread and the HA background threads
/// share the same cache / role / peer view.
struct MetaRuntime {
    base: PathBuf,
    token: String,
    /// Shared HMAC key for minting the Data-Access Capability
    /// (`--data-access-key`). `Some` => every accepted `M_OPEN` ships a
    /// capability the client must replay on its data-plane `OP_OPEN`; `None` =>
    /// the ack carries none and the data plane is gated by token / api-key
    /// alone (byte-for-byte unchanged). See [`crate::remote::dac`].
    data_access_key: Option<Vec<u8>>,
    /// Capability lifetime in seconds; `0` = never expires. See
    /// [`Self::data_access_key`] and [`crate::remote::dac`].
    data_access_ttl: u64,
    /// Per-caller credential gate for `M_OPEN` and live sessions; `None` keeps
    /// the static-token/LAN-trusted behavior byte-for-byte.
    keys: Option<Arc<crate::apikeys::ApiKeyStore>>,
    /// base view → (physical engine, last-resolve ms stamp). Engines are
    /// opened lazily by [`store_for`] and retired by the idle sweeper
    /// ([`evict_idle`]) once neither a mounted session nor recent traffic
    /// pins them; under the PG backend eviction is what releases the
    /// per-view pooled connections back to `max_connections`.
    cache: Arc<Mutex<HashMap<String, (Arc<MetaStore>, Arc<AtomicU64>)>>>,
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
    /// arbiter node, not merely bounded (see [`crate::remote::server`]'s data-plane gate
    /// and [`crate::remote::client::RemoteBackend`]).
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
    /// cache the wire loop dispatches against. Each entry carries the same
    /// last-resolve stamp as [`MetaRuntime::cache`]: a busy lens over a base
    /// keeps the WHOLE base group resident, so eviction only fires when the
    /// physical tree and every derived name on it have gone idle together.
    views: Arc<Mutex<HashMap<String, (Arc<dyn MetaProvider>, Arc<AtomicU64>)>>>,
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
    /// Idle window (ms) for the engine sweeper; 0 disables eviction entirely
    /// (entries stay resident for the process lifetime — the pre-sweeper
    /// behavior every existing test relies on).
    engine_idle_ms: u64,
    /// `--audit-retention DUR`, as spelled by the operator; `None` = the audit
    /// table is append-only (the behavior before the flag existed).
    audit_retention: Option<String>,
    /// The account-key store that gates the CONTROL port, when
    /// `--control-listen` is set. Deliberately separate from [`Self::keys`]: the
    /// mount gate (`--require-api-key`) and the governance gate are two
    /// entrances with different credential scopes, and enabling one must never
    /// silently enable the other.
    ctl_keys: Option<Arc<crate::apikeys::ApiKeyStore>>,
    /// The web console's sign-in accounts and their delegation to base views,
    /// built with the control port (same routing rule as [`Self::ctl_keys`]:
    /// control-plane state follows the metadata backend). `None` without a
    /// control port — there is then nothing that can authenticate an actor.
    /// Every governance call re-reads it, so a revoked grant bites on the NEXT
    /// call rather than at the next login.
    web_auth: Option<Arc<crate::webauth::WebAuthStore>>,
    /// Staged-transfer handle → the principal that minted it. A handle is a
    /// file name under `<base>/meta/.xfer/`, and staged dumps are per-tenant
    /// content (one view's whole metadata tree), so a delegated account must
    /// not be able to pull a chunk out of a dump it did not stage — not even by
    /// guessing a handle, which is only `pid` + a timestamp. Entries live as long
    /// as the transfer; [`Self::prune_xfer_owners`] drops the ones whose file is
    /// gone. A restart loses them, which is already true of the staged files
    /// themselves (the reply says so), and an unknown handle is then only
    /// reachable by a super principal.
    xfer_owner: Mutex<HashMap<String, String>>,
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

    /// Record that `who` minted the staged-transfer `handle`, and forget the
    /// handles whose staging file is already gone (a client that staged a dump
    /// and never came back for chunk 2 would otherwise leave a row forever).
    /// Pruning here rather than on a timer costs one `stat` per abandoned
    /// transfer, at the moment the map is already being touched.
    fn claim_xfer(&self, handle: &str, who: &str) {
        let mut m = self.xfer_owner.lock();
        if m.len() > 64 {
            let dir = self.base.join("meta").join(".xfer");
            m.retain(|h, _| dir.join(h).exists());
        }
        m.insert(handle.to_string(), who.to_string());
    }

    /// Refuse a staged transfer that belongs to a different principal. An admin
    /// (a control-key operator, or a console admin) may reach any handle: the
    /// recovery path "an admin finishes a colleague's stuck import" is worth
    /// more than the isolation between handles, and an admin can read the
    /// staging file off the disk anyway. A handle with NO recorded owner (a
    /// transfer started before a server restart) is treated the same way — the
    /// file exists and only a trusted caller can name it.
    fn xfer_guard(&self, handle: &str, who: &crate::webauth::Principal) -> Result<(), String> {
        match self.xfer_owner.lock().get(handle) {
            Some(owner) if owner.as_str() != who.account.as_str() && !who.is_super() => Err(format!(
                "transfer handle '{handle}' was staged by another account — a delegated user may \n                     only continue transfers it started; re-run the command under your own sign-in"
            )),
            _ => Ok(()),
        }
    }

    /// A finished or aborted transfer releases its ownership row.
    fn release_xfer(&self, handle: &str) {
        self.xfer_owner.lock().remove(handle);
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
    /// Shared HMAC key for the Data-Access Capability (`--data-access-key`),
    /// moved into [`MetaRuntime`] at `serve()`. `None` (default) issues no
    /// capability, leaving the data plane gated by token / api-key only.
    data_access_key: Option<Vec<u8>>,
    /// Capability lifetime in seconds (`--data-access-ttl`); `0` = never expires.
    data_access_ttl: u64,
    /// Optional per-caller credential gate (`--require-api-key`), backed by the
    /// same `<base>/meta/api_keys.db` `grovefs apikey` / `grovefs mcp` use.
    keys: Option<Arc<crate::apikeys::ApiKeyStore>>,
    role: Role,
    peer: Option<String>,
    sync_repl: bool,
    witness: Option<String>,
    store: StoreKind,
    /// Set when `--meta-store` named a scheme this binary cannot provide (any
    /// `scheme://` URI other than the PostgreSQL ones). `serve()` refuses on a
    /// non-empty value, which is what keeps an unsupported backend from being
    /// silently served by SQLite.
    store_unavailable: Option<String>,
    /// Idle engine eviction window (ms); see [`MetaRuntime::engine_idle_ms`].
    engine_idle_ms: u64,
    /// `--audit-retention DUR`: how long `mcp_calls` rows live, as the
    /// operator spelled it (`90d`). `None` (the default) keeps today's
    /// append-forever behavior — nothing is deleted unless it is asked for.
    /// See [`reap_audit`].
    audit_retention: Option<String>,
    /// Optional REMOTE CONTROL PLANE address (`--control-listen HOST:PORT`).
    /// Empty/unset keeps the process exactly as before: no governance port, no
    /// admin credentials, nothing new to firewall.
    control_listen: Option<String>,
    /// The install-time web-console owner (`--admin-user` +
    /// `--admin-password-file`), held as account + password ONLY for the length
    /// of `serve()` — it is hashed into the account store and then dropped, and
    /// never reaches `MetaRuntime`. `None` seeds nothing: an existing store keeps
    /// exactly the accounts it already has.
    admin_account: Option<(String, String)>,
}

/// Default idle-engine eviction window for a builder-constructed `MetaServer`
/// (5 minutes). The CLI passes the parsed `--engine-idle-secs` explicitly;
/// embedders get this default, `0` via the builder turns the sweeper off.
const DEFAULT_ENGINE_IDLE_MS: u64 = 300_000;

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
            data_access_key: None,
            data_access_ttl: 0,
            keys: None,
            role: Role::Primary,
            peer: None,
            sync_repl: false,
            witness: None,
            store: StoreKind::Sqlite,
            store_unavailable: None,
            engine_idle_ms: DEFAULT_ENGINE_IDLE_MS,
            audit_retention: None,
            control_listen: None,
            admin_account: None,
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
            data_access_key: None,
            data_access_ttl: 0,
            keys: None,
            role: Role::Primary,
            peer: None,
            sync_repl: false,
            witness: None,
            store: StoreKind::Sqlite,
            store_unavailable: None,
            engine_idle_ms: DEFAULT_ENGINE_IDLE_MS,
            audit_retention: None,
            control_listen: None,
            admin_account: None,
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

    /// Enable Data-Access Capability issuance: every accepted `M_OPEN` mints an
    /// HMAC-signed token under `key` and ships it to the client, which must
    /// replay it to a `data-server` configured with the SAME key. This is the
    /// meta-server half of closing the "rogue meta reusing a legit data-server"
    /// hole (see [`crate::remote::dac`]); `ttl_secs = 0` mints non-expiring
    /// capabilities (the robust default for long-lived mounts), a nonzero TTL
    /// bounds the replay window at the cost of a remount once it lapses.
    /// Unset (the default) issues nothing and leaves behavior byte-for-byte
    /// unchanged.
    pub fn with_data_access_key(mut self, key: Vec<u8>, ttl_secs: u64) -> Self {
        self.data_access_key = Some(key);
        self.data_access_ttl = ttl_secs;
        self
    }

    /// Expose the REMOTE CONTROL PLANE on a second listener
    /// (`--control-listen HOST:PORT`): governance calls (`view`/`apikey`/`refs`)
    /// arrive as framed requests and are executed HERE, by the process that
    /// already owns the metadata store — so an operator on any machine needs an
    /// admin-scope account key instead of a database URI.
    ///
    /// The port always authenticates (the server opens the key store itself and
    /// demands `Scope::Admin`); there is no unauthenticated mode. Unset keeps
    /// this process byte-for-byte identical to a server without the feature.
    pub fn with_control_listen(mut self, addr: Option<String>) -> Self {
        self.control_listen = addr.filter(|a| !a.trim().is_empty());
        self
    }

    /// Set the install-time web-console owner: the account `grovefs web` logs in
    /// as the first time. Only meaningful together with
    /// [`MetaServer::with_control_listen`] (that is where the account store is
    /// opened); `serve()` calls [`crate::webauth::bootstrap_owner`], which
    /// creates the row when the store is EMPTY, treats a same-named existing
    /// admin as an idempotent no-op (a systemd restart re-reads the unit file) —
    /// and REFUSES to mint a second owner into a store that already has one.
    /// Accounts after that are created by an admin in the console/CLI, so this
    /// flag is an install-time credential, not a per-boot one.
    pub fn with_admin_account(mut self, account: Option<String>, password: Option<String>) -> Self {
        self.admin_account = match (account, password) {
            (Some(a), Some(p)) => Some((a, p)),
            _ => None,
        };
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
    /// * `None` or `"sqlite"` — the default embedded-SQLite backend, one db per
    ///   view under `<base>/meta/`.
    /// * `postgres://user:pw@host:port/db` (or `postgresql://...`) — the
    ///   PostgreSQL backend, one PG schema per view. A comma list
    ///   (`postgres://…,postgres://…`) or a `file:/abs/path` INSTANCE REGISTRY
    ///   (one `name = postgres://…` per line, the first entry = control plane;
    ///   keeps credentials out of argv and names instances stably enough for
    ///   the placement table) spreads view trees across several databases —
    ///   see [`crate::pg_store::resolve_view_uri`]. Combined with
    ///   `--role/--peer/--witness` at `serve()` time this returns an
    ///   [`io::ErrorKind::InvalidInput`] because GroveFS-native HA snapshot
    ///   shipping (`VACUUM INTO`) is a SQLite-specific mechanism; run PG with
    ///   PostgreSQL's own streaming/logical replication instead.
    ///
    /// Any OTHER `scheme://` URI is not silently demoted to SQLite: it is
    /// remembered and refused by `serve()`. Falling back would start a server
    /// that stores metadata somewhere other than where the operator said, which
    /// is the one outcome a `--meta-store` flag must never produce.
    ///
    /// * `treedb://host:port` — EXPERIMENTAL (`--features treedb`) metadata
    ///   engine, one treedb view per GroveFS base view. Only the TREE side moves
    ///   there; definitions, keys and accounts stay in local SQLite, so
    ///   `--meta-dir` still means something under this scheme.
    pub fn with_store(mut self, uri: Option<String>) -> Self {
        match uri {
            Some(u) if crate::pg_store::is_pg_store(&u) => {
                self.store = StoreKind::Pg(u);
            }
            #[cfg(feature = "treedb")]
            Some(u) if crate::treedb_store::is_treedb_store(&u) => {
                // A malformed address is refused exactly like an unknown scheme:
                // `serve()` reports it, the server never starts on a default.
                match crate::treedb_store::parse_addr(&u) {
                    Ok(addr) => self.store = StoreKind::Treedb(addr),
                    Err(e) => {
                        self.store_unavailable = Some(format!("`{u}` is not a valid --meta-store treedb address: {e}"));
                        self.store = StoreKind::Sqlite;
                    }
                }
            }
            Some(u) if u.contains("://") => {
                self.store_unavailable = Some(format!(
                    "`{u}` is not a --meta-store scheme this build serves: GroveFS metadata lives in SQLite (the local trees under --meta-dir) or PostgreSQL (`--meta-store postgres://…`)"
                ));
                self.store = StoreKind::Sqlite;
            }
            _ => self.store = StoreKind::Sqlite,
        }
        self
    }

    /// Evict idle per-view engines after `secs` seconds without a mounted
    /// session or traffic (0 = never evict). Under a PG backend this bounds
    /// resident connections to RECENTLY USED views instead of every view
    /// opened since start — the difference between "views a server can host"
    /// and PG's `max_connections`.
    pub fn with_engine_idle_secs(mut self, secs: u64) -> Self {
        self.engine_idle_ms = secs.saturating_mul(1000);
        self
    }

    /// Bound the audit trail's growth (`--audit-retention`). Takes the duration
    /// TEXT rather than seconds so the cutoff is always resolved against the
    /// server's clock at sweep time (a fixed cutoff computed at startup would
    /// drift into "keep everything" on a long-lived process), and so
    /// `store.info` can show the operator what was actually configured.
    pub fn with_audit_retention(mut self, window: Option<String>) -> Self {
        self.audit_retention = window;
        self
    }

    pub fn serve(&self) -> io::Result<()> {
        // A --meta-store scheme this binary cannot serve is refused rather than
        // downgraded: see `with_store`.
        if let Some(msg) = &self.store_unavailable {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, msg.clone()));
        }
        // PG does not exercise the GroveFS-native HA pipeline: the
        // `VACUUM INTO` snapshot mechanism and the fencing-witness lease are
        // SQLite-specific. Refuse the combination at startup instead of letting
        // a caller assume `--peer` still replicates (it silently wouldn't).
        if !matches!(self.store, StoreKind::Sqlite)
            && (self.peer.is_some() || self.witness.is_some() || self.role == Role::Standby)
        {
            let (scheme, remedy) = match &self.store {
                StoreKind::Pg(_) => (
                    "postgres://…",
                    "Run PostgreSQL's own streaming / logical replication for HA,\n                     or keep the meta-server single-instance under PG.",
                ),
                // The guard above already excluded SQLite, and the arms cover
                // every non-Sqlite variant this build has; naming that here
                // rather than with a catch-all is what stops a future variant
                // from silently borrowing another backend's remedy text.
                StoreKind::Sqlite => unreachable!("HA refusal is built per non-Sqlite store"),
                #[cfg(feature = "treedb")]
                StoreKind::Treedb(_) => (
                    "treedb://…",
                    "treedb recovers from its own WAL and snapshots, but GroveFS-native\n                     snapshot shipping is not implemented for it — run the meta-server single-instance.",
                ),
            };
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "--meta-store {scheme} cannot be combined with --role standby / --peer / --witness: \n                     GroveFS-native HA snapshot shipping is SQLite-only.\n                     {remedy}"
                ),
            ));
        }
        // SQLite trees live under `<base>/meta`; a PG store writes nothing
        // locally, so creating that directory would be a misleading footprint
        // (an empty `meta/` on a host that has no local metadata at all). Under
        // treedb the directory IS still needed — the view definitions and the
        // placement table never leave `--meta-dir`, whatever the switch says.
        if self.store.control_uri().is_none() {
            std::fs::create_dir_all(self.base.join("meta"))?;
        }
        // The CONTROL port's own credential store. Built here (and only here)
        // so `--control-listen` is self-contained: it does NOT reuse
        // `--require-api-key`'s store object but the same physical rows, and it
        // does NOT turn on the mount gate. A control port without a key store
        // would be an unauthenticated administrator entrance, which is why
        // there is no "open" mode for it at all.
        let (ctl_keys, web_auth, admin_note) = match &self.control_listen {
            None => (None, None, None),
            Some(_) => {
                // 凭据表与账户表可能已经比视图定义先搬进引擎
                // （`GROVEFS_TREEDB_CONTROL=engine`），所以这里问的是
                // `control_tables_uri`：视图定义与 placement 还在本地，用 `control_uri`
                // 会让 meta-server 读一份刚被 `apikey create` 写空的本地库。
                let keys_uri = self.store.control_tables_uri();
                let keys = Arc::new(
                    crate::apikeys::ApiKeyStore::open_for(&self.base, keys_uri.as_deref())
                        .map_err(io::Error::other)?,
                );
                // 账户表跟凭据同一个出口：M1b 之后它们同进同退，两个出口迟早
                // 会说两句话（一个进引擎、一个留在磁盘）。
                let web = Arc::new(
                    crate::webauth::WebAuthStore::open_for(&self.base, keys_uri.as_deref())
                        .map_err(io::Error::other)?,
                );
                // Install-time owner (`--admin-user` + `--admin-password-file`).
                // Seeded HERE, by the process that owns the store, because a
                // governance entrance that could mint its own administrator has
                // no gate; the rule `bootstrap_owner` applies (and why a restart
                // must not fail on a flag a unit file keeps re-reading) is stated
                // there.
                let note = match &self.admin_account {
                    Some((account, password)) => Some(
                        crate::webauth::bootstrap_owner(&web, account, password)
                            .map_err(io::Error::other)?,
                    ),
                    None => None,
                };
                (Some(keys), Some(web), note)
            }
        };
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
        // Second banner line: the backend. `base` alone used to imply the engine
        // (a SQLite base IS the trees); with two backends it no longer does,
        // and an operator debugging "where did my tree go" must be able to tell
        // from the log. Printed as its own line rather than folded into the
        // listening line so the historical `…(base …, role …)` transcript shape
        // that operators already grep for is unchanged.
        println!(
            "meta-server: metadata backend {} — {}",
            self.store.backend_name(),
            self.store.backend_layout()
        );
        // Instance registry, NAMES only (URIs embed passwords and the banner
        // lands in the journal): an operator running a multi-instance
        // deployment must see at startup which instance carries the control
        // plane and whether every registry line parsed. An unreadable
        // registry fails the start — a server that cannot answer "where does
        // view X live" must not come up looking healthy.
        if let StoreKind::Pg(spec) = &self.store {
            let book = crate::pg_store::expand_store_spec(spec).map_err(|e| {
                io::Error::new(io::ErrorKind::InvalidInput, format!("--meta-store: {e}"))
            })?;
            if book.instances.len() > 1 {
                println!(
                    "meta-server: {} PG instance(s) [{}] — control plane on '{}', view homes recorded in grovefs_control.view_placement (`view create` balances; `--pg-instance` pins)",
                    book.instances.len(),
                    book.names(),
                    book.control().name
                );
            }
        }
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
        // backend, never a local file the PG deployment would ignore. Under treedb
        // the definitions stay local (nothing in the engine models them) but the
        // TREE inventory is the engine's: a base view there has no `meta/*.db`,
        // and a delegation that cannot resolve its base would authorize nothing.
        let control = match &self.store {
            #[cfg(feature = "treedb")]
            StoreKind::Treedb(addr) => crate::mcp::control_store_for_treedb(&self.base, addr),
            _ => crate::mcp::control_store_for(&self.base, self.store.control_uri())
                .map_err(io::Error::other)?,
        };
        let rt = Arc::new(MetaRuntime {
            base: self.base.clone(),
            token: self.token.clone(),
            data_access_key: self.data_access_key.clone(),
            data_access_ttl: self.data_access_ttl,
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
            engine_idle_ms: self.engine_idle_ms,
            audit_retention: self.audit_retention.clone(),
            ctl_keys,
            web_auth,
            xfer_owner: Mutex::new(HashMap::new()),
        });
        if rt.sync_repl {
            println!("meta-server: synchronous replication ON (RPO=0 for mutating ops, ACK timeout {SYNC_REPL_ACK_TIMEOUT:?})");
        }
        // ---- remote control plane (governance over the wire) -----------------
        //
        // Bound BEFORE the filesystem port accepts anything, so a typo in
        // `--control-listen` fails the start instead of leaving an operator
        // with a server that looks up but refuses every governance call. The
        // listener is a SECOND port on purpose: an admin entrance must be
        // separable by firewall/netns from the mount plane, and a mount
        // session must never be able to reach a governance op.
        if let Some(addr) = &self.control_listen {
            let cl = TcpListener::bind(addr).map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!(
                        "cannot bind --control-listen {addr}: {e} (the control port is a second \
                         listener beside --listen; pick a free address, e.g. 127.0.0.1:50154)"
                    ),
                )
            })?;
            println!(
                "meta-server: CONTROL plane listening on {addr} (admin-scope api keys only; \
                 every call is audited as method=control — mint with `grovefs apikey create \
                 --for admin --owner <account>` on this host)"
            );
            // The web console's account store came up with this port, so say
            // what it holds: an operator must be able to tell "the install flag
            // seeded the first owner" from "the flag was ignored because one
            // already exists" without opening a browser.
            if let Some(note) = &admin_note {
                println!("meta-server: web accounts — {note}");
            }
            println!(
                "meta-server: web console sign-ins are verified HERE (`grovefs web --control-server {}`); \
                 accounts and their base-view delegation live in {}",
                addr,
                match &self.store {
                    StoreKind::Pg(_) => "grovefs_control.web_accounts/web_grants",
                    StoreKind::Sqlite => "meta/web_accounts.db",
                    #[cfg(feature = "treedb")]
                    StoreKind::Treedb(_) => {
                        // Same switch as the credentials (M1a/M1b): when it is on,
                        // the engine's generic tables are the store and the local
                        // file is read by nobody.
                        if crate::apikeys::ApiKeyStore::treedb_control_enabled() {
                            "the treedb engine's generic tables (GROVEFS_TREEDB_CONTROL=engine)"
                        } else {
                            "meta/web_accounts.db (local: the engine's control tables are off)"
                        }
                    }
                }
            );
            let crt = rt.clone();
            thread::spawn(move || gov_accept(cl, crt));
            // Audit retention, when an operator asked for one. Without this the
            // trail is append-only forever: every governance call, every MCP
            // tool call and every mount handshake adds a row, so a deployment
            // that lives on the console accumulates a table nobody can read.
            // Sweeping at 1/24 of the window (bounded to 10min..12h) turns
            // `--audit-retention 90d` into a daily trim, and sleeping FIRST
            // keeps a boot from starting with a DELETE on the store.
            if let Some(window) = rt.audit_retention.clone() {
                let ar = rt.clone();
                let nap = crate::apikeys::duration_secs(&window)
                    .ok()
                    .flatten()
                    .map(|s| (s / 24).clamp(600, 43_200))
                    .unwrap_or(86_400);
                let every = if nap >= 3600 {
                    format!("~{}h", nap / 3600)
                } else {
                    format!("{}min", nap / 60)
                };
                println!(
                    "meta-server: audit rows older than --audit-retention {window} are swept every {every} (`grovefs apikey purge --before {window}` runs the same cut on demand)",
                );
                thread::spawn(move || loop {
                    thread::sleep(std::time::Duration::from_secs(nap as u64));
                    reap_audit(&ar, &window);
                });
            }
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
    serve_on_meta_full(listener, base, token, None, 0);
}

/// Accept loop that mints a Data-Access Capability under `dac_key` on every
/// accepted `M_OPEN` (the `meta-server --data-access-key` path), so a test can
/// drive a real meta→client→data admission without a FUSE mount. See
/// [`crate::remote::dac`].
pub fn serve_on_meta_dac(
    listener: TcpListener,
    base: PathBuf,
    token: &str,
    dac_key: Vec<u8>,
    dac_ttl: u64,
) {
    serve_on_meta_full(listener, base, token, Some(dac_key), dac_ttl);
}

fn serve_on_meta_full(
    listener: TcpListener,
    base: PathBuf,
    token: &str,
    dac_key: Option<Vec<u8>>,
    dac_ttl: u64,
) {
    // SQLite-only helper (single-node path): the control plane is the local
    // `meta/views` dir, matching `store: StoreKind::Sqlite`.
    let control =
        crate::mcp::control_store_for(&base, None).expect("file control store is infallible");
    let rt = Arc::new(MetaRuntime {
        base,
        token: token.to_string(),
        data_access_key: dac_key,
        data_access_ttl: dac_ttl,
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
        // Plain LAN-trusted test server: keep engines resident (no sweeper),
        // exactly the pre-eviction behavior.
        engine_idle_ms: 0,
        audit_retention: None,
        ctl_keys: None,
        web_auth: None,
        xfer_owner: Mutex::new(HashMap::new()),
    });
    serve_runtime(listener, rt);
}

fn serve_runtime(listener: TcpListener, rt: Arc<MetaRuntime>) {
    // One ticker carries two jobs: engine eviction (an idle view releases its
    // WHOLE pool) and pool reaping (a burst above the warm floor is shed back).
    // The second is what makes a mounted view's idle writers reclaimable at
    // all — eviction can never reach a mounted view, sessions pin the engine.
    // Reaping is only worth a thread on a PG backend; SQLite holds no
    // per-view PG backends, so the pre-eviction "no sweeper" behavior of the
    // LAN-trusted test server (`engine_idle_ms == 0`) stays exactly as it was.
    let pg_reap = matches!(rt.store, StoreKind::Pg(_)) && crate::pg_store::pool_idle_ms() > 0;
    if rt.engine_idle_ms > 0 || pg_reap {
        let sw = rt.clone();
        thread::spawn(move || loop {
            // Sweep at half the window each job is configured with, bounded to
            // 1s–60s, and take the SHORTER of the two: eviction wants to land
            // within ≤1.5× its idle window, and a reaper that sleeps longer
            // than the reap TTL would stretch an operator's `--pg-pool-idle-secs
            // 5` into a minute of held burst connections.
            let evict = if sw.engine_idle_ms > 0 {
                (sw.engine_idle_ms / 2).clamp(1_000, 60_000)
            } else {
                u64::MAX
            };
            let reap = (crate::pg_store::pool_idle_ms() / 2).clamp(1_000, 60_000);
            let nap = evict.min(reap);
            thread::sleep(std::time::Duration::from_millis(nap));
            let gone = evict_idle(&sw);
            if gone > 0 {
                println!(
                    "meta-server: evicted {gone} idle engine(s) (>{}s idle, no sessions)",
                    sw.engine_idle_ms / 1000
                );
            }
            let shed = reap_pools(&sw);
            if shed > 0 {
                // Per-view windows may differ (a `view pool --idle-secs` override
                // beats the process value), so the line names the process default
                // as what it is: the floor of the sweep interval, not the TTL that
                // necessarily fired.
                println!(
                    "meta-server: reaped {shed} idle PG pool connection(s) back to their warm floor (process window {}s, per-view overrides honored)",
                    crate::pg_store::pool_idle_ms() / 1000
                );
            }
        });
    }
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                // One thread per connection (mount work socket, event
                // subscriber, HA peer): a half-open peer must not pin the
                // thread, the mount's `subs` entry and thus the view's engine
                // (PG: 8 writers + 4 readers) forever — keepalive breaks the
                // blocked read in ~2 min so the existing cleanup chain
                // (subscriber removal → idle eviction) can run.
                crate::remote::enable_tcp_keepalive(&s);
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

// ---------------------------------------------------------------------------
// REMOTE CONTROL PLANE (governance over the wire)
// ---------------------------------------------------------------------------
//
// Why a second listener instead of a new op on the filesystem port: an admin
// entrance must be separable from the mount plane by firewall/netns, and a
// mount session must never be able to reach a governance op. Why in THIS
// process: it is the only one that holds the backend connection (a PG URI) or
// the local `<meta-dir>`, so the operator's credential replaces a database
// password. Every call is authenticated as `Scope::Admin` and audited.
//
// The framing is one request / one response per governance verb (see
// `proto::OP_CONTROL`). A caller that sends `"pin": true` gets its credential
// kept on the socket, so its later requests skip the connect, the thread spawn
// and the two store queries an authentication costs; a caller that sends
// nothing gets the classic one-request connection, which is what every CLI
// invocation still asks for. The session is bounded on purpose
// ([`GOV_PINNED_REQUESTS`], [`GOV_PINNED_IDLE`]) so an identity cannot outlive
// the process that opened it, and it is re-checked against the store on the
// same [`crate::apikeys::KEY_RECHECK`] tick a live mount is.
fn gov_accept(listener: TcpListener, rt: Arc<MetaRuntime>) {
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                // Keepalive on top of the read bound in `handle_gov_conn`: a
                // pinned connection is held open by design, so the timeout is
                // what stops a gateway that died mid-session from pinning a
                // thread forever, and keepalive is what notices the host itself
                // went away (~2 min).
                crate::remote::enable_tcp_keepalive(&s);
                let rt = rt.clone();
                thread::spawn(move || {
                    if let Err(e) = handle_gov_conn(s, &rt) {
                        if e.kind() != io::ErrorKind::UnexpectedEof {
                            eprintln!("control conn error: {e}");
                        }
                    }
                });
            }
            Err(e) => eprintln!("control accept failed: {e}"),
        }
    }
}

/// One governance response: `STATUS 0` with an `{ok,r}` / `{ok:false,e}`
/// envelope in the body. Failures are NEVER a transport-level status, so a
/// caller can tell "the op said no" from "the server went away".
fn gov_reply(s: &mut TcpStream, result: serde_json::Value, err: Option<&str>) -> io::Result<()> {
    gov_reply_pinned(s, result, err, false)
}

/// [`gov_reply`] plus the answer to a pinning caller's question "may I send
/// another request on this socket?". `pinned` is only ever ADDED when true, so
/// the envelope a one-shot caller parses — and every caller built before
/// sessions existed — is byte-identical to the old one, and silence means no.
fn gov_reply_pinned(
    s: &mut TcpStream,
    result: serde_json::Value,
    err: Option<&str>,
    pinned: bool,
) -> io::Result<()> {
    let mut env = match err {
        Some(e) => serde_json::json!({"ok": false, "e": e}),
        None => serde_json::json!({"ok": true, "r": result}),
    };
    if pinned {
        env["pinned"] = serde_json::json!(true);
    }
    let data = serde_json::to_vec(&env).unwrap_or_default();
    proto::send_response(s, 0, 0, &data)
}

/// The caller behind one governance connection, decided from the request body.
///
/// Two shapes arrive on this port and they must not be confused:
///
/// - **an operator** — an admin-scope account key and NO `account` field. That
///   key already means full governance everywhere in GroveFS (`grovefs view`,
///   `grovefs apikey`, MCP), so it keeps exactly those rights and is attributed
///   by key owner;
/// - **a console user** — the SAME admin key (the gateway holds it; it is the
///   only credential that can reach this port) plus `account`, the signed-in
///   web account. That account is re-read from the store on every call and the
///   ACL below applies to it.
///
/// The trust boundary is therefore the admin key, not the `account` field: a
/// process that can read the key could name anyone, but it can already do
/// everything the key permits, so nothing is gained. What the field buys is
/// ATTRIBUTION and per-user LIMITATION — which is the whole point of putting a
/// console in front of this port: many people, few shared credentials, and each
/// person's reach bounded by their delegations. `grovefs web` is the only thing
/// that ever sets it, and it refuses to start without the key.
fn gov_actor(
    rt: &Arc<MetaRuntime>,
    account: &str,
    authed: &crate::apikeys::AuthedKey,
) -> Result<crate::webauth::Principal, String> {
    if account.is_empty() {
        return Ok(crate::webauth::Principal::operator(
            authed.owner.as_deref().unwrap_or(&authed.label),
        ));
    }
    let store = rt.web_auth.as_deref().ok_or_else(|| {
        "this server has no web account store, so it cannot accept an `account` on a control call \
         (start the meta-server with --control-listen; sign-ins are checked by that process)"
            .to_string()
    })?;
    crate::webauth::Principal::lookup(store, account)
}

/// Verify a console sign-in HERE, where the password digests live. The gateway
/// deliberately cannot check a password: it holds no digests, so the answer to
/// "is this the right password" has to come from the store, and the store is
/// only open in the process that owns it. Returns the same shape a successful
/// `login` reply needs, and one identical refusal for "no such account" and
/// "wrong password".
fn gov_login(rt: &Arc<MetaRuntime>, a: &serde_json::Value) -> Result<serde_json::Value, String> {
    use serde_json::json;
    let store = rt.web_auth.as_deref().ok_or_else(|| {
        "this server has no web account store — start it with --control-listen to get sign-ins"
            .to_string()
    })?;
    let account = crate::webauth::normalize_account(&req_str(a, "account")?)?;
    let password = req_str(a, "password")?;
    let ok = store.login(&account, &password)?;
    Ok(json!({
        "account": ok.account,
        "role": ok.role.as_str(),
        "bases": ok.bases,
    }))
}

/// One retention pass: drop `mcp_calls` rows older than the window, and say so
/// only when rows actually went (a sweep that prints every ~hour would bury the
/// startup lines that matter). A refusal is reported because the next pass will
/// hit the same wall — an operator wants to know the table is not being trimmed.
fn reap_audit(rt: &Arc<MetaRuntime>, window: &str) {
    let Some(keys) = rt.ctl_keys.as_ref() else {
        return;
    };
    let Ok(cutoff) = crate::apikeys::cutoff_from_age(window) else {
        eprintln!(
            "meta-server: --audit-retention '{window}' is not a duration (use 45m, 24h, 90d…); no audit sweep is running"
        );
        return;
    };
    match keys.purge_calls_older_than(&cutoff) {
        Ok(0) => {}
        Ok(n) => println!(
            "meta-server: audit retention '{window}' dropped {n} mcp_calls row(s) older than {cutoff}"
        ),
        Err(e) => eprintln!("meta-server: audit retention sweep failed: {e}"),
    }
}

/// Requests one pinned governance session answers before the server closes the
/// socket for its caller's next request to reopen, and how long it may stay
/// silent first. Both are bounds on an identity held on someone else's behalf;
/// the client is always answered before a connection ends, so nothing is ever
/// lost to a bound — the next call simply opens a socket and pays one
/// authentication again.
const GOV_PINNED_REQUESTS: usize = 512;
const GOV_PINNED_IDLE: Duration = Duration::from_secs(300);

/// The credential one governance connection answers for once its caller asked
/// to be pinned. Holding it is the entire saving: an authenticated request costs
/// two store queries (an `api_keys` SELECT plus the `last_used_at` UPDATE), and
/// a session does not repeat them per request.
struct GovSession {
    authed: crate::apikeys::AuthedKey,
    /// The next liveness re-check, on the cadence the mount plane keeps.
    check: Instant,
    /// Requests answered so far, against [`GOV_PINNED_REQUESTS`].
    served: usize,
}

/// Serve one governance connection: the classic one request / one response for a
/// caller that does not ask to be pinned, a bounded loop for one that does.
///
/// Both directions of a version skew are handled on purpose, not by luck:
/// - an OLD client against this server sends one request, reads the reply and
///   closes; the next read here hits EOF and the thread exits, which is the same
///   ending it always had with one extra `recv`;
/// - a reply to a caller that did not ask to be pinned carries no `pinned`
///   field, so a newer client never pools it. No second request is ever written
///   into a socket the server has already forgotten, so no op can be lost to the
///   skew.
fn handle_gov_conn(mut sock: TcpStream, rt: &Arc<MetaRuntime>) -> io::Result<()> {
    let _ = sock.set_nodelay(true);
    // A pinned session is held open BETWEEN requests by design, so it must not
    // be held open forever by an accident: a caller that vanishes mid-session
    // costs one thread until this fires, and nothing after it.
    let _ = sock.set_read_timeout(Some(GOV_PINNED_IDLE));
    let mut sess: Option<GovSession> = None;
    loop {
        let (op_code, body) = match proto::recv_request(&mut sock) {
            Ok(v) => v,
            // Silence past the idle bound, or a client that went away between
            // requests: both are a normal end of life, not an error to print.
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::UnexpectedEof
                ) =>
            {
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        if gov_one(&mut sock, rt, &mut sess, op_code, &body)? {
            continue;
        }
        return Ok(());
    }
}

/// Answer ONE governance request on `sock`; `Ok(true)` means the connection may
/// carry another. Every other outcome has been replied to and closes the socket
/// — a wrong op code, an unreadable body, a caller the store will not
/// authenticate, a session that just served its last request.
///
/// The order of the gates is the one this handler has always had and it matters:
/// credential first (nothing is audited as anybody until somebody is proven),
/// then the sign-in case, then the console account's delegations, then the op.
fn gov_one(
    sock: &mut TcpStream,
    rt: &Arc<MetaRuntime>,
    sess: &mut Option<GovSession>,
    op_code: u8,
    body: &[u8],
) -> io::Result<bool> {
    if op_code != proto::OP_CONTROL {
        gov_reply(
            sock,
            serde_json::Value::Null,
            Some(&format!(
                "this port speaks governance only (expected op {}, got {op_code}) — a mount must use --listen",
                proto::OP_CONTROL
            )),
        )?;
        return Ok(false);
    }
    let req: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => {
            gov_reply(
                sock,
                serde_json::Value::Null,
                Some(&format!("malformed control request: {e}")),
            )?;
            return Ok(false);
        }
    };
    // "Keep my credential on this socket." Only the gateway's pool asks; a CLI
    // leaves the field off and gets the one-request connection it expects.
    let want_pin = req.get("pin").and_then(|v| v.as_bool()).unwrap_or(false);
    let op = req.get("op").and_then(|v| v.as_str()).unwrap_or("");
    let args = req.get("a").cloned().unwrap_or(serde_json::Value::Null);
    // The web console's per-user attribution: WHO signed in, and from where. Only
    // the `account` half is checked against the store; `ip` is a label, taken
    // from the gateway's own view of the socket (it is the gateway that sees the
    // browser, not this server), and it is recorded verbatim so an audit line
    // cannot be made to look like someone else's by a crafted value that happens
    // to parse — it is a string in a detail column, nothing more.
    let account = req.get("account").and_then(|v| v.as_str()).unwrap_or("");
    let ip = req.get("ip").and_then(|v| v.as_str()).unwrap_or("");
    let actor = |owner: &str| {
        if account.is_empty() {
            owner.to_string()
        } else {
            format!("{account}@{ip}")
        }
    };
    let Some(keys) = rt.ctl_keys.as_ref() else {
        gov_reply(
            sock,
            serde_json::Value::Null,
            Some(
                "this meta-server has no control plane (start it with --control-listen HOST:PORT)",
            ),
        )?;
        return Ok(false);
    };
    // A session that already exists is re-verified on the KEY_RECHECK tick and
    // not per request: `apikey revoke` and an elapsed `--ttl` therefore reach a
    // pooled connection on the first request after the window, which is the
    // promise the mount plane already makes to a live mount. The refusal is the
    // handshake's own sentence, and the session ends with it.
    if let Some(p) = sess.as_mut() {
        if Instant::now() >= p.check {
            match keys.revalidate_live(&p.authed.id, crate::apikeys::Scope::Admin) {
                Ok(()) => p.check = Instant::now() + crate::apikeys::KEY_RECHECK,
                Err(e) => {
                    let _ = keys.log_call(
                        Some(&p.authed),
                        "control",
                        op,
                        false,
                        &format!("account={} {e}", actor("-")),
                    );
                    gov_reply(sock, serde_json::Value::Null, Some(&e))?;
                    return Ok(false);
                }
            }
        }
    }
    // Who is asking: the identity this socket pinned on an earlier request, or
    // the credential presented now. Cloned out first, so the borrow of `sess`
    // has ended before the pin below writes it.
    let pinned = sess.as_ref().map(|p| p.authed.clone());
    let authed = match pinned {
        Some(a) => a,
        None => match keys
            .authenticate_admin(req.get("k").and_then(|v| v.as_str()).unwrap_or(""))
        {
            Ok(a) => {
                if want_pin {
                    *sess = Some(GovSession {
                        authed: a.clone(),
                        check: Instant::now() + crate::apikeys::KEY_RECHECK,
                        served: 0,
                    });
                }
                a
            }
            Err(e) => {
                // An unauthenticated attempt is audited too (keyless), so a wrong
                // or revoked credential is visible in `apikey calls` instead of
                // only in the operator's terminal.
                let _ = keys.log_call(None, "control", op, false, &e);
                gov_reply(sock, serde_json::Value::Null, Some(&e))?;
                return Ok(false);
            }
        },
    };
    // Whether this socket may carry another request, decided BEFORE the op runs
    // because every reply below owes the caller that answer. `served` counts the
    // request being answered now.
    let keep = match sess.as_mut() {
        Some(p) => {
            p.served += 1;
            p.served < GOV_PINNED_REQUESTS
        }
        None => false,
    };
    // Sign-in is the one call made BEFORE there is a principal to authorize, so
    // it is answered here and never reaches `control_dispatch` — where an
    // unaccountable caller would have no rule to be judged by.
    if op == "login" {
        let out = gov_login(rt, &args);
        // A failed sign-in is audited against the KEY (there is no verified
        // account yet) and names the account tried, so `apikey calls` shows the
        // attempts and not just the successes. It must NOT echo the password.
        let detail = match &out {
            Ok(v) => format!(
                "account={} role={}",
                actor("-"),
                v.get("role").and_then(|r| r.as_str()).unwrap_or("?")
            ),
            Err(e) => format!(
                "account={} {e}",
                if account.is_empty() { "-" } else { account }
            ),
        };
        let _ = keys.log_call(
            Some(&authed),
            "control",
            "login",
            out.is_ok(),
            &detail,
        );
        return match out {
            Ok(v) => {
                gov_reply_pinned(sock, v, None, keep)?;
                Ok(keep)
            }
            Err(e) => {
                gov_reply_pinned(sock, serde_json::Value::Null, Some(&e), keep)?;
                Ok(keep)
            }
        };
    }
    let who = match gov_actor(rt, account, &authed) {
        Ok(w) => w,
        Err(e) => {
            let _ = keys.log_call(
                Some(&authed),
                "control",
                op,
                false,
                &format!("account={} {e}", actor("-")),
            );
            // An account that cannot be resolved is that ACCOUNT's problem, not
            // the socket's: the credential is fine, so another account's next
            // click on the same pooled connection must not pay for a reconnect.
            gov_reply_pinned(sock, serde_json::Value::Null, Some(&e), keep)?;
            return Ok(keep);
        }
    };
    let owner = authed.owner.as_deref().unwrap_or(&authed.label);
    match control_dispatch(rt, op, &args, &who) {
        Ok(v) => {
            let _ = keys.log_call(
                Some(&authed),
                "control",
                op,
                true,
                &format!("account={}", actor(owner)),
            );
            gov_reply_pinned(sock, v, None, keep)?;
        }
        Err(e) => {
            let _ = keys.log_call(
                Some(&authed),
                "control",
                op,
                false,
                &format!("account={} {e}", actor(owner)),
            );
            // A governance refusal is an ANSWER, not a broken connection: the
            // op said no, the socket did not, so it stays pooled.
            gov_reply_pinned(sock, serde_json::Value::Null, Some(&e), keep)?;
        }
    }
    Ok(keep)
}

fn req_str(a: &serde_json::Value, name: &str) -> Result<String, String> {
    a.get(name)
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| format!("control call is missing the '{name}' argument"))
}

/// The graft plan as it arrives over the wire: an array of `{"src":…,"dst":…}`
/// objects. Parsed strictly — a malformed entry is refused rather than skipped,
/// because a silently dropped plan line would graft a subset the operator never
/// asked for and still report success.
fn graft_items(a: &serde_json::Value) -> Result<Vec<crate::meta::GraftItem>, String> {
    let arr = a
        .get("items")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "control call is missing the 'items' array".to_string())?;
    let mut out = Vec::with_capacity(arr.len());
    for (i, ent) in arr.iter().enumerate() {
        let src = ent
            .get("src")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("graft item #{i}: missing 'src'"))?;
        let dst = ent
            .get("dst")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("graft item #{i}: missing 'dst'"))?;
        out.push(crate::meta::GraftItem {
            src: src.to_string(),
            dst: dst.to_string(),
        });
    }
    Ok(out)
}

/// Seed a just-created tree from a manifest that arrived as text. Returns the
/// suffix appended to the creation report, or an empty string when there was no
/// manifest. The uid/gid recorded are the SERVER's, because the files are
/// materialized here — the operator's ids describe a different machine.
fn seed_if_manifest(
    meta: &dyn crate::meta::MetaProvider,
    be: Option<&dyn crate::backend::StorageBackend>,
    manifest: Option<&str>,
) -> Result<String, String> {
    let Some(text) = manifest else {
        return Ok(String::new());
    };
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let (dirs, files) = crate::mcp::seed_tree_from_text(meta, be, text, uid, gid)?;
    Ok(format!(
        "\napplied seed manifest: {dirs} dir(s) and {files} file(s) created; names that already existed were left alone"
    ))
}

fn is_striped(dr: &str) -> bool {
    dr.starts_with('s') && dr.matches(':').count() >= 2
}

/// Free every object the ledger just released, from `droot`. Shared by the two
/// teardown arms so a striped key (which a plain root cannot reach) is reported
/// rather than half-deleted in one branch and not the other.
fn free_unbooked(rep_refs: &[String], droot: &Path) -> String {
    let (mut freed, mut missing, mut failed) = (0usize, 0usize, 0usize);
    match crate::backend::open_local(droot) {
        Ok(be) => {
            for dr in rep_refs {
                if is_striped(dr) {
                    failed += 1;
                    continue;
                }
                if be.len(dr).is_err() {
                    missing += 1;
                    continue;
                }
                match be.remove(dr) {
                    Ok(()) => freed += 1,
                    Err(_) => failed += 1,
                }
            }
        }
        Err(e) => {
            return format!(
                "cannot open data root {} to free {} released object(s): {e} — the tree is gone and its bookings are released, so those bytes are unreferenced now; free them through the data-server or a mount",
                droot.display(),
                rep_refs.len()
            )
        }
    }
    format!(
        "{freed} object(s) freed from {}, {missing} not found under that data root (they live on another store?), {failed} could not be freed (striped keys move through a data-server)",
        droot.join("blocks").display()
    )
}

/// What one governance op demands of its caller. Stated as data, in one match,
/// and evaluated BEFORE the op runs: the console's buttons are a convenience,
/// this is the gate. An op that is not listed here is REFUSED rather than
/// defaulted, so adding a verb to `control_dispatch` without saying who may
/// call it fails loudly instead of silently becoming an open endpoint.
enum Gate {
    /// Nothing beyond a valid sign-in: the caller's own identity and state.
    Self_,
    /// An administrator only. The reason is quoted to the caller, because the
    /// console shows it in the refusal — "why can't I click that" should not
    /// need a Slack message to answer.
    Super(&'static str),
    /// Must govern the existing view named by this arg.
    Base(&'static str),
    /// Must govern every existing view named by these args.
    Bases(&'static [&'static str]),
    /// (Re)defines the view named by `name` from the text in `text`: must govern
    /// what the name already is (if anything) and what the new text is OVER.
    Define { name: &'static str, text: &'static str },
    /// Forks the base in `base` under the name in `name`: must govern the base,
    /// and the name if it already exists.
    Create { base: &'static str, name: &'static str },
    /// Must have staged the transfer handle named by this arg (an admin may
    /// finish anyone's transfer).
    Handle(&'static str),
    /// Must own the api key named by this arg (or be an admin).
    Key(&'static str),
    /// Must be an admin, or the account named by the `account` arg.
    SelfAccount,
    /// An uploaded dump being installed: the handle must be the caller's, and
    /// the target is a NEW base view (admin) unless `as_fork` names a base the
    /// caller governs (then it is that caller's fork). Stated as one gate
    /// because the two cases share the same args and only an admin may create a
    /// base view from anywhere.
    Import,
}

/// The access rule for one op. Kept adjacent to `control_dispatch`'s match: the
/// two lists are the same list, and a diff that adds an arm without adding a
/// line here is the mistake this shape exists to catch.
fn gates(op: &str) -> Result<Vec<Gate>, String> {
    let g = match op {
        "whoami" | "login" => vec![Gate::Self_],
        // Deployment-wide reads and writes.
        "info" => vec![Gate::Self_],
        "store.info" | "account.list" | "account.create" | "account.role" | "account.disable"
        | "account.delete" | "grant.add" | "grant.remove" | "grant.list" => {
            vec![Gate::Super("this describes the deployment, not one view")]
        }
        "account.passwd" => vec![Gate::SelfAccount],
        // View definition reads/writes.
        "def.list" | "physical.list" => vec![Gate::Self_],
        "def.get" | "stamp" | "physical.has" | "fork.has" | "def.remove" | "fork.drop" => {
            vec![Gate::Base("name")]
        }
        "view.stats" => vec![Gate::Base("view")],
        "view.export" => vec![Gate::Base("view")],
        "def.put" => vec![Gate::Define {
            name: "name",
            text: "text",
        }],
        "fork.create" => vec![Gate::Create {
            base: "base",
            name: "name",
        }],
        "fork.graft" => vec![Gate::Bases(&["dst", "src"])],
        // Chunked portable dumps: the name is checked when the handle is minted
        // (`view.export`, `view.import.commit`), the later chunks are guarded by
        // handle OWNERSHIP, which is the only thing that still identifies a
        // caller mid-transfer.
        "view.export.chunk" | "view.export.done" | "view.import.put" | "view.import.abort" => {
            vec![Gate::Handle("handle")]
        }
        "view.import.begin" => vec![Gate::Self_],
        "view.import.commit" => vec![Gate::Import],
        // Placement, refcounts and content-byte GC cross every tenant.
        "view.create" => vec![Gate::Super(
            "a new base view picks a storage home (PG instance / data root), which is a deployment decision",
        )],
        "view.rm_base" | "refs.rebuild" | "gc.scan" | "gc.apply" => vec![Gate::Super(
            "it destroys or rewrites state shared by more views than the caller's own",
        )],
        // Credentials: an admin sees all; a delegated account may hold keys for
        // its own views only (enforced in the arms, which is where the scope and
        // owner of a key are known).
        "apikey.create" | "apikey.list" | "apikey.calls" => vec![Gate::Self_],
        "apikey.revoke" | "apikey.set_ro" | "apikey.rotate" | "apikey.update" => {
            vec![Gate::Key("id")]
        }
        // Trimming the audit trail is the one verb that destroys OTHER keys'
        // history, so it cannot be delegated to the account whose actions it
        // records. It is also deliberately absent from the console's whitelist:
        // `grovefs apikey purge` on a CLI, never a button.
        "apikey.purge" => vec![Gate::Super(
            "it deletes audit rows belonging to every credential in the deployment",
        )],
        other => {
            return Err(format!(
                "control op '{other}' has no access rule (unknown, or added to the dispatcher \n                     without one) — refusing. Known ops: info, whoami, login, def.list/get/put/remove, \n                     stamp, physical.list/has, fork.has/create/drop/graft, view.create/stats/export/import, \n                     view.rm_base, refs.rebuild, gc.scan/apply, apikey.create/list/revoke/rotate/set_ro/update/calls/purge, \n                     account.list/create/passwd/role/disable/delete, grant.list/add/remove"
            ));
        }
    };
    Ok(g)
}

/// Is `op` one this plane knows and has RULED on? Exported for
/// [`crate::web`]'s startup test: the console's whitelist and this table are two
/// lists of the same ops kept in two files, and a name that drifts between them
/// is either a dead button or a promise the console cannot keep.
pub fn control_op_known(op: &str) -> bool {
    gates(op).is_ok()
}

/// Refuse unless `who` governs `name`. `allow_new` is for the verbs that CREATE
/// the name (`def put`, `fork create`): a delegated account may not overwrite a
/// view that belongs to someone else, but a name that has neither a definition
/// nor a tree yet belongs to whoever makes it — otherwise the one thing a
/// delegate must be able to do (build a lens on its own base) would be impossible.
fn may_govern(
    rt: &Arc<MetaRuntime>,
    who: &crate::webauth::Principal,
    name: &str,
    allow_new: bool,
) -> Result<(), String> {
    if who.is_super() {
        return Ok(());
    }
    if who.manages(rt.control.as_ref(), name).unwrap_or(false) {
        return Ok(());
    }
    if allow_new {
        // An unresolvable name that does not exist yet is free to claim. One that
        // resolves to a base the caller does not hold is NOT, and neither is a
        // broken chain: `manages` already refused it.
        if !(rt.control.has_def(name)? || rt.control.has_physical(name)?) {
            return Ok(());
        }
    }
    Err(who.refusal(rt.control.as_ref(), name))
}

/// Does this principal see this credential row? Its own, or bound to a view it
/// governs — the second half matters because a key outlives the account that
/// minted it, and the person who has to clean it up is whoever owns the tree.
fn key_visible(
    rt: &Arc<MetaRuntime>,
    who: &crate::webauth::Principal,
    k: &crate::apikeys::KeyInfo,
) -> bool {
    if who.is_super() {
        return true;
    }
    if k.owner.as_deref() == Some(who.account.as_str()) {
        return true;
    }
    match &k.view {
        Some(v) => who.manages(rt.control.as_ref(), v).unwrap_or(false),
        None => false,
    }
}

/// The api key row `id`, if it exists at all. Read as a row rather than as an
/// owner string because the refusal has to name the key, and because "no such
/// key" and "a key with no owner column value" are different answers.
fn key_row(rt: &Arc<MetaRuntime>, id: &str) -> Result<Option<crate::apikeys::KeyInfo>, String> {
    let keys = rt
        .ctl_keys
        .as_ref()
        .ok_or_else(|| "this server has no control key store".to_string())?;
    Ok(keys.list()?.into_iter().find(|k| k.id == id))
}

/// Enforce `gates(op)` against `who`. Every refusal names what the caller may
/// do instead, because the console renders this string to a user who is looking
/// at a button they just pressed.
fn authorize(
    rt: &Arc<MetaRuntime>,
    who: &crate::webauth::Principal,
    op: &str,
    a: &serde_json::Value,
) -> Result<(), String> {
    for gate in gates(op)? {
        match gate {
            Gate::Self_ => {}
            Gate::Super(reason) => {
                if !who.is_super() {
                    return Err(format!(
                        "account '{}' is not an administrator, and '{op}' is not per-view governance: {reason} — an admin must run it (or grant you the base view it acts on)",
                        who.account
                    ));
                }
            }
            Gate::Base(arg) => may_govern(rt, who, &req_str(a, arg)?, false)?,
            Gate::Bases(args) => {
                for arg in args {
                    may_govern(rt, who, &req_str(a, arg)?, false)?;
                }
            }
            Gate::Define { name, text } => {
                let name = req_str(a, name)?;
                let text = req_str(a, text)?;
                let def = crate::layout::LayoutRules::parse_def(&text)
                    .map_err(|e| format!("cannot govern '{name}': the new definition is unreadable: {e}"))?;
                let parent = match def {
                    crate::layout::ViewDef::Derived(b, _) | crate::layout::ViewDef::Fork(b) => b,
                    crate::layout::ViewDef::Alias(r) => r,
                };
                // Over a base the caller does not hold, a lens is a way to read
                // someone else's tree; the name itself is checked separately so
                // an existing view cannot be re-pointed by a non-owner.
                may_govern(rt, who, &parent, false)?;
                may_govern(rt, who, &name, true)?;
            }
            Gate::Create { base, name } => {
                may_govern(rt, who, &req_str(a, base)?, false)?;
                may_govern(rt, who, &req_str(a, name)?, true)?;
            }
            Gate::Handle(arg) => rt.xfer_guard(&req_str(a, arg)?, who)?,
            Gate::Key(arg) => {
                let id = req_str(a, arg)?;
                if !who.is_super() {
                    match key_row(rt, &id)? {
                        // An id with no row is the op's own "nothing revoked"
                        // answer; the ACL has nothing to say about it.
                        None => {}
                        Some(k) if k.owner.as_deref() == Some(who.account.as_str()) => {}
                        Some(k) => {
                            return Err(format!(
                                "api key '{id}' ('{}') belongs to {}— its owner or an administrator must change it",
                                k.label,
                                match &k.owner {
                                    Some(a) => format!("account '{a}' "),
                                    None => "no account (a pre-account key) ".to_string(),
                                }
                            ));
                        }
                    }
                }
            }
            Gate::SelfAccount => {
                let acc = crate::webauth::normalize_account(&req_str(a, "account")?)?;
                if !who.is_super() && acc != who.account {
                    return Err(format!(
                        "account '{}' may change its own password, not '{acc}' — an admin acts on other accounts",
                        who.account
                    ));
                }
            }
            Gate::Import => {
                rt.xfer_guard(&req_str(a, "handle")?, who)?;
                let view = req_str(a, "view")?;
                match a.get("as_fork").and_then(|v| v.as_str()) {
                    Some(base) => {
                        may_govern(rt, who, base, false)?;
                        may_govern(rt, who, &view, true)?;
                    }
                    // No `as_fork` means this dump becomes a BASE view, which is
                    // `view.create` by another door.
                    None => {
                        if !who.is_super() {
                            return Err(format!(
                                "account '{}' cannot import a new base view '{view}' — an admin must create base views; to import a copy of a view you govern, pass as_fork=<your base>",
                                who.account
                            ));
                        }
                        may_govern(rt, who, &view, true)?;
                    }
                }
            }
        }
    }
    Ok(())
}

/// The view names a principal may see. An admin's list is returned untouched —
/// no `resolve_base` per name — which is the common case and keeps the
/// deployment-wide listing exactly as cheap as it was before accounts existed.
fn visible(
    rt: &Arc<MetaRuntime>,
    who: &crate::webauth::Principal,
    names: Vec<String>,
) -> Vec<String> {
    if who.is_super() {
        return names;
    }
    names
        .into_iter()
        // A name that fails to resolve is hidden rather than reported: to a
        // delegate it is not theirs either way, and a broken chain elsewhere in
        // the deployment is not information this listing needs to leak.
        .filter(|n| who.manages(rt.control.as_ref(), n).unwrap_or(false))
        .collect()
}

fn control_dispatch(
    rt: &Arc<MetaRuntime>,
    op: &str,
    a: &serde_json::Value,
    who: &crate::webauth::Principal,
) -> Result<serde_json::Value, String> {
    use serde_json::json;
    authorize(rt, who, op, a)?;
    match op {
        // What this server IS. An administrator sees the deployment: backend,
        // its self-description, the local base dir. A delegated account sees
        // only its own reach — `describe` names storage hosts and `base` names
        // server paths, and neither is anything a view manager needs to do a
        // job while holding the keys of one tree.
        "info" => {
            if who.is_super() {
                Ok(json!({
                    "backend": rt.control.backend(),
                    "describe": rt.control.describe(),
                    "base": rt.base.display().to_string(),
                    "actor": who.account,
                    "role": who.role.as_str(),
                }))
            } else {
                Ok(json!({
                    "backend": rt.control.backend(),
                    "actor": who.account,
                    "role": who.role.as_str(),
                    "bases": who.bases,
                }))
            }
        }
        // The whole deployment seen from the control plane: for the console's
        // overview page. Deployment-wide numbers are exactly what a delegated
        // account must not read about someone else's tree, so `Super`.
        "store.info" => {
            let live: usize = rt.subs.lock().values().map(|m| m.len()).sum();
            let engines = rt.cache.lock().len();
            let views = rt.views.lock().len();
            // A key past its deadline can no longer authenticate, so it is not
            // "live" in any sense an operator means. The two counts are still
            // reported apart: "4 rows expired and sit in the table" is the
            // fact that needs acting on, and a single number would hide it.
            let key_rows = rt.ctl_keys.as_ref().and_then(|k| k.list().ok());
            let (keys, expired_keys) = match &key_rows {
                Some(v) => (
                    Some(
                        v.iter()
                            .filter(|r| r.revoked_at.is_none() && !r.expired)
                            .count(),
                    ),
                    Some(
                        v.iter()
                            .filter(|r| r.revoked_at.is_none() && r.expired)
                            .count(),
                    ),
                ),
                None => (None, None),
            };
            let accounts = rt
                .web_auth
                .as_ref()
                .and_then(|w| w.list().ok().map(|v| v.len()));
            Ok(json!({
                "backend": rt.control.backend(),
                "describe": rt.control.describe(),
                "base": rt.base.display().to_string(),
                "role": format!("{:?}", rt.role()),
                "views": rt.control.list_physical()?.len(),
                "definitions": rt.control.list_defs()?.len(),
                "accounts": accounts,
                "liveKeys": keys,
                "expiredKeys": expired_keys,
                "liveSessions": live,
                "residentEngines": engines,
                "residentViews": views,
                // What the audit trail costs this deployment, as configured:
                // `null` means rows are kept until an operator purges them by
                // hand, which is the fact the console's audit page should say
                // out loud rather than leave as a silently growing table.
                "auditRetention": rt.audit_retention,
            }))
        }
        // A delegated caller's list is its own definitions: the same lens/fork
        // chain `may_govern` judges writes by, so what a user can SEE and what
        // a user can TOUCH cannot drift apart.
        "def.list" => {
            let defs = rt.control.list_defs()?;
            if who.is_super() {
                return Ok(json!(defs));
            }
            Ok(json!(visible_pairs(rt, who, defs)))
        }
        "def.get" => {
            let name = req_str(a, "name")?;
            Ok(json!(rt.control.get_def_raw(&name)?))
        }
        "def.put" => {
            let name = req_str(a, "name")?;
            let text = req_str(a, "text")?;
            rt.control.put_def(&name, &text)?;
            // A cached lens built from the OLD text must not keep serving: the
            // same eviction the local CLI relies on for hot reload.
            rt.evict();
            Ok(serde_json::Value::Null)
        }
        "def.remove" => {
            let name = req_str(a, "name")?;
            let removed = rt.control.remove_def(&name)?;
            rt.evict();
            Ok(json!(removed))
        }
        "stamp" => Ok(json!(rt.control.stamp(&req_str(a, "name")?))),
        "physical.list" => {
            let v = rt.control.list_physical()?;
            Ok(json!(visible(rt, who, v)))
        }
        "physical.has" => Ok(json!(rt.control.has_physical(&req_str(a, "name")?)?)),
        "fork.has" => Ok(json!(rt.control.has_fork_tree(&req_str(a, "name")?)?)),
        // The tree-copy verbs. They are served HERE — by the process that owns
        // the store — instead of being refused, because the copy reads and
        // writes nothing but the metadata store this server is already the only
        // holder of. Whether a backend CAN fork is the backend's own verdict
        // (PG has no per-view snapshot path),
        // and that verdict travels back verbatim rather than being re-invented
        // on the operator's machine.
        "fork.create" => {
            let base = req_str(a, "base")?;
            let name = req_str(a, "name")?;
            let seed = a.get("seed").and_then(|v| v.as_bool()).unwrap_or(true);
            // `with_def`: record `fork <base>` in the same call that builds the
            // tree. The CLI's `view fork` sequences the two itself (and prints the
            // definition it wrote), so it does not pass this flag and its behaviour
            // is untouched; the console does, because a browser can be closed
            // between two requests and leave a tree whose name resolves to nothing
            // — the one intermediate state that has no self-service repair.
            let with_def = a
                .get("with_def")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let text = format!("fork {base}\n");
            if with_def && rt.control.has_def(&name)? {
                return Err(format!(
                    "view '{name}' already has a definition — this call would overwrite it"
                ));
            }
            let st = rt.control.create_fork_tree(&base, &name, seed)?;
            if with_def {
                // Tree first, definition second, and the tree goes back if the
                // definition cannot be written: the reverse order is what strands
                // a name that resolves to a missing tree.
                if let Err(e) = rt.control.put_def(&name, &text) {
                    rt.control.destroy_fork_tree(&name);
                    rt.evict();
                    return Err(format!("cannot record the fork definition for '{name}': {e}"));
                }
            }
            // A cached engine for `name` could only be a stale tree handle.
            rt.evict();
            Ok(json!({
                "nodes": st.nodes,
                "blobs": st.blobs,
                "definition": if with_def { serde_json::Value::from(text) } else { serde_json::Value::Null },
            }))
        }
        "fork.drop" => {
            let name = req_str(a, "name")?;
            rt.control.drop_fork_tree(&name)?;
            rt.evict();
            Ok(serde_json::Value::Null)
        }
        "fork.graft" => {
            let dst = req_str(a, "dst")?;
            let src = req_str(a, "src")?;
            let items = graft_items(a)?;
            let dest = a.get("dest").and_then(|v| v.as_str()).unwrap_or("/");
            let skip = a
                .get("skip_existing")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let st = rt.control.graft_tree(&dst, &src, &items, dest, skip)?;
            rt.evict();
            Ok(json!({
                "nodes": st.nodes,
                "dirs": st.dirs,
                "blobs": st.blobs,
                "skipped": st.skipped,
            }))
        },
        // The portable-dump verbs. `export`/`import` used to be refused here
        // because they looked like "move a file on the operator's machine". They
        // are served now, and the dump travels over this port in CHUNKS: the
        // operator's `--out`/`--in` stay local paths, the tree stays here, and
        // neither side ever holds a whole dump in memory. What moves is
        // metadata only — the `data_ref` keys — so no byte of content crosses
        // the wire in either direction (see `view.export` in the reply for the
        // explicit note the CLI prints).
        "view.export" => view_export_stage(rt, &req_str(a, "view")?, who),
        "view.export.chunk" => {
            let handle = req_str(a, "handle")?;
            let offset = a.get("offset").and_then(|v| v.as_u64()).unwrap_or(0);
            let max = a.get("max").and_then(|v| v.as_u64()).unwrap_or(XFER_CHUNK);
            view_export_chunk(rt, &handle, offset, max)
        }
        "view.export.done" => view_xfer_drop(rt, &req_str(a, "handle")?),
        "view.import.begin" => view_import_begin(rt, who),
        "view.import.put" => {
            let handle = req_str(a, "handle")?;
            let offset = a.get("offset").and_then(|v| v.as_u64()).unwrap_or(0);
            let text = req_str(a, "text")?;
            view_import_put(rt, &handle, offset, &text)
        }
        "view.import.commit" => view_import_commit(rt, a),
        "view.import.abort" => view_xfer_drop(rt, &req_str(a, "handle")?),
        "view.create" => {
            let view = req_str(a, "view")?;
            // The manifest travels as TEXT: the file lives on the operator's
            // machine, the tree lives here, and the same `mkdir`/`touch` lines
            // are interpreted by the process that owns the store.
            let manifest = a.get("manifest").and_then(|v| v.as_str());
            // Same semantics as the CLI's `view create --pg-instance`: an
            // explicit instance name pins the home, otherwise least-loaded.
            let pg_instance = a.get("pg_instance").and_then(|v| v.as_str());
            if !crate::layout::valid_view_name(&view) {
                return Err(format!("invalid view name '{view}'"));
            }
            if rt.control.has_def(&view)? {
                return Err(format!(
                    "view '{view}' already has a derived/alias/fork definition — choose a different base name (`view list` shows it)"
                ));
            }
            if rt.control.has_physical(&view)? {
                return Err(format!(
                    "view '{view}' already exists — remove it first (`view rm --name {view} --base`)"
                ));
            }
            let report = match &rt.store {
                StoreKind::Pg(uri) => {
                    // Place BEFORE the first open: the home row decides which
                    // instance every later resolve binds this view to.
                    let (home, _) = crate::pg_store::place_new_view(uri, &view, pg_instance)
                        .map_err(|e| format!("view create: {e}"))?;
                    let eng = crate::pg_store::PgEngine::open(uri, &view, false)
                        .map_err(|e| format!("view create: {e}"))?;
                    // A PG deployment's bytes are owned by the data servers, so
                    // there is no local root to write a manifest file into.
                    let note = seed_if_manifest(&eng, None, manifest)?;
                    format!(
                        "created BASE view '{view}' (schema grovefs_v_<hex>) on instance '{home}'{note}"
                    )
                }
                StoreKind::Sqlite => {
                    let mp = rt.base.join("meta").join(format!("{view}.db"));
                    if let Some(dir) = mp.parent() {
                        std::fs::create_dir_all(dir)
                            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
                    }
                    let eng = crate::meta::MetaEngine::open(&mp)
                        .map_err(|e| format!("view create: {e}"))?;
                    // SQLite IS the local byte root: `blocks/` sits beside
                    // `meta/`, so a `touch` line can create its object here.
                    let be = crate::backend::open_local(&rt.base).ok();
                    let note = seed_if_manifest(&eng, be.as_deref(), manifest)?;
                    format!("created BASE view '{view}' at {}{note}", mp.display())
                }
                #[cfg(feature = "treedb")]
                StoreKind::Treedb(addr) => {
                    // The generic `has_physical` check above already sees the
                    // engine's trees (`mcp::TreedbControlStore`), so this is the
                    // race guard for a view created between that check and here —
                    // against the engine's own list, which is the only authority.
                    if crate::treedb_store::TreedbStore::list_views(addr)
                        .map_err(|e| format!("view create: {e}"))?
                        .iter()
                        .any(|v| v == &view)
                    {
                        return Err(format!(
                            "view '{view}' already exists in treedb at {addr} — remove it first (`view rm --view {view} --base`)"
                        ));
                    }
                    let eng = crate::meta::MetaStore::open_treedb(addr, &view, false)
                        .map_err(|e| format!("view create: {e}"))?;
                    // Like PG: the bytes belong to the data servers, so there is
                    // no local root a manifest `touch` line could write into.
                    let note = seed_if_manifest(&eng, None, manifest)?;
                    format!("created BASE view '{view}' in treedb at {addr}{note}")
                }
            };
            rt.evict();
            Ok(json!(report))
        }
        "view.rm_base" => {
            let view = req_str(a, "view")?;
            let data_root = a.get("data_root").and_then(|v| v.as_str());
            view_rm_base(rt, &view, data_root)
        }
        "refs.rebuild" => refs_rebuild(rt),
        "gc.scan" => gc_audit(rt, false, a),
        "gc.apply" => gc_audit(rt, true, a),
        "apikey.create" => {
            let keys = rt
                .ctl_keys
                .as_ref()
                .ok_or_else(|| "no control key store".to_string())?;
            let label = req_str(a, "label")?;
            let scope = crate::apikeys::Scope::parse(&req_str(a, "scope")?)?;
            let view = a
                .get("view")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let ro = a.get("ro").and_then(|v| v.as_bool()).unwrap_or(false);
            // Whitespace is not a remark: trim once and let a blank field be
            // NULL in the store (and so an empty cell in the console) rather
            // than a run of spaces that looks like an answer.
            let note = a
                .get("note")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_string);
            // A typo in `ttl` is refused HERE, so it cannot silently become a
            // key that never expires.
            let expires_at = match a.get("ttl").and_then(|v| v.as_str()) {
                Some(t) => crate::apikeys::expires_from_ttl(t)?,
                None => None,
            };
            let mut owner = a
                .get("owner")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            if !who.is_super() {
                use crate::apikeys::Scope;
                // A delegated account mints credentials for a view it governs,
                // on the two planes a view is actually used from. `admin` is
                // this port's own scope and `any` admits more than a view
                // manager may hand out: a key that can govern the deployment
                // would make the caller's delegation irrelevant, which is the
                // one thing the ACL exists to prevent.
                if scope == Scope::Admin || scope == Scope::Any {
                    return Err(format!(
                        "account '{}' may mint `--for mount` or `--for mcp` keys, not `--for {}`: \
                         those scopes govern the whole deployment, and only an administrator may \
                         hand that out",
                        who.account,
                        scope.as_str()
                    ));
                }
                // Unbound (`view: None`) would let the key mount EVERY tree — the
                // binding is not a nicety here, it is the delegation itself.
                let v = match &view {
                    Some(v) => v.clone(),
                    None => {
                        return Err(format!(
                            "account '{}' must name the view a key is bound to (`view=<name>'): \
                             an unbound key reaches every tree in the deployment",
                            who.account
                        ))
                    }
                };
                may_govern(rt, who, &v, false)?;
                // Attributed to the minter, whatever the caller claimed: the
                // console's "my keys" list and `apikey revoke`'s ownership test
                // both read this column, so it cannot be pointed at a colleague.
                owner = Some(who.account.clone());
            }
            let (id, full) = keys.create_extended(
                &label,
                scope,
                view.as_deref(),
                ro,
                owner.as_deref(),
                note.as_deref(),
                expires_at.as_deref(),
                Some(who.account.as_str()),
            )?;
            Ok(json!({
                "id": id,
                "key": full,
                "label": label,
                "scope": scope.as_str(),
                "owner": owner,
                "note": note,
                "expires_at": expires_at,
                "created_by": who.account,
            }))
        }
        "apikey.list" => {
            let keys = rt
                .ctl_keys
                .as_ref()
                .ok_or_else(|| "no control key store".to_string())?;
            let rows: Vec<serde_json::Value> = keys
                .list()?
                .into_iter()
                // An admin's list is every row (unchanged); a delegated
                // account's is its own keys plus the keys bound to its views,
                // so it can rotate and revoke what its trees are mounted with
                // without seeing a stranger's credential.
                .filter(|k| key_visible(rt, who, k))
                .map(|k| {
                    json!({
                        "id": k.id,
                        "owner": k.owner,
                        "label": k.label,
                        "scope": k.scope,
                        "view": k.view,
                        "ro": k.ro,
                        "created_at": k.created_at,
                        "last_used_at": k.last_used_at,
                        "revoked_at": k.revoked_at,
                        // The bookkeeping columns. `expired` is the store's own
                        // verdict (the same comparison the handshake makes), so
                        // a console cannot render a key as live that will be
                        // refused on its next mount.
                        "note": k.note,
                        "expires_at": k.expires_at,
                        "expired": k.expired,
                        "created_by": k.created_by,
                        "secret_hint": k.secret_hint,
                    })
                })
                .collect();
            Ok(json!(rows))
        }
        "apikey.revoke" => {
            let keys = rt
                .ctl_keys
                .as_ref()
                .ok_or_else(|| "no control key store".to_string())?;
            Ok(json!(keys.revoke(&req_str(a, "id")?)?))
        }
        // Replace the secret of one VISIBLE active key (the `Gate::Key` rule
        // above is what enforces that; it is revoke's and set_ro's rule). The
        // reply shape mirrors `apikey.create` — a plaintext key for one-time
        // display — because the operation it spares is "revoke, re-mint, and
        // re-point every binding by hand".
        "apikey.rotate" => {
            let keys = rt
                .ctl_keys
                .as_ref()
                .ok_or_else(|| "no control key store".to_string())?;
            let id = req_str(a, "id")?;
            let row = keys
                .list()?
                .into_iter()
                .find(|k| k.id == id)
                .ok_or_else(|| format!("no api key with id '{id}'"))?;
            if !key_visible(rt, who, &row) {
                return Err(format!(
                    "account '{}' may not rotate '{id}': the key belongs to another account \
                     and is not bound to a view it governs",
                    who.account
                ));
            }
            let full = keys
                .rotate(&id)?
                .ok_or_else(|| "api key is revoked — rotate revives nothing, revoke is final".to_string())?;
            Ok(json!({
                "id": id,
                "key": full,
                "label": row.label,
                "scope": row.scope,
                "owner": row.owner,
            }))
        }
        "apikey.set_ro" => {
            let keys = rt
                .ctl_keys
                .as_ref()
                .ok_or_else(|| "no control key store".to_string())?;
            let ro = a.get("ro").and_then(|v| v.as_bool()).unwrap_or(false);
            Ok(json!(keys.set_ro(&req_str(a, "id")?, ro)?))
        }
        // Edit the BOOKKEEPING of one visible active key: its remark and/or its
        // deadline. Deliberately nothing else — `scope` and the view binding
        // decide what a credential can reach, and moving those under a live
        // mount is revoke-and-re-mint territory (see `apikey.rotate`'s rule for
        // why the identity fields survive a secret change). `Gate::Key` is what
        // keeps a delegated account out of a stranger's key.
        "apikey.update" => {
            let keys = rt
                .ctl_keys
                .as_ref()
                .ok_or_else(|| "no control key store".to_string())?;
            let id = req_str(a, "id")?;
            let note = a
                .get("note")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .map(str::to_string);
            // Three distinct cases, and the console must be able to express all
            // of them: absent `ttl` = leave the deadline alone, `ttl: "never"` =
            // remove it, `ttl: "7d"` = set a new one.
            let expires_at = match a.get("ttl").and_then(|v| v.as_str()) {
                Some(t) => Some(crate::apikeys::expires_from_ttl(t)?),
                None => None,
            };
            if note.is_none() && expires_at.is_none() {
                return Err(
                    "apikey.update needs `note` and/or `ttl` — the rest of a key is fixed when it \
                     is minted (scope and view binding are, because a live mount must not have \
                     its reach moved under it)"
                        .to_string(),
                );
            }
            if !keys.update_fields(&id, note.as_deref(), expires_at.as_ref().map(Option::as_deref))? {
                return Err(format!(
                    "nothing updated for api key '{id}': no ACTIVE row — the id is unknown, or the \
                     key is revoked (revoke is final). A key that merely EXPIRED is extended here, \
                     with a new `ttl`"
                ));
            }
            Ok(json!({
                "ok": true,
                "id": id,
                "note": note,
                "expires_at": expires_at.flatten(),
            }))
        }
        "apikey.calls" => {
            let keys = rt
                .ctl_keys
                .as_ref()
                .ok_or_else(|| "no control key store".to_string())?;
            let limit = a.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
            let mine = who.is_super();
            // A delegated account's audit window is its own activity. Reading
            // more rows and filtering here (rather than pushing the predicate
            // into SQL) keeps one `recent_calls` for both backends; the window
            // is widened so a tenant with a busy deployment still sees `limit`
            // of ITS lines instead of a mostly-empty page.
            let want = if mine { limit } else { limit.saturating_mul(20).min(5000) };
            let owned: Vec<String> = if mine {
                Vec::new()
            } else {
                keys.list()?
                    .into_iter()
                    .filter(|k| key_visible(rt, who, k))
                    .map(|k| k.id)
                    .collect()
            };
            let tag = format!("account={} ", who.account);
            let rows: Vec<serde_json::Value> = keys
                .recent_calls(want)?
                .into_iter()
                .filter(|c| {
                    mine
                        || c.key_id.as_deref().is_some_and(|id| owned.iter().any(|k| k == id))
                        || c.detail.starts_with(&tag)
                        || c.detail.contains(&format!("account={}@", who.account))
                })
                .take(limit)
                .map(|c| {
                    json!({
                        "ts": c.ts,
                        "key_id": c.key_id,
                        "key_label": c.key_label,
                        "method": c.method,
                        "tool": c.tool,
                        "ok": c.ok,
                        "detail": c.detail,
                    })
                })
                .collect();
            Ok(json!(rows))
        }
        // Bound the audit trail's growth. `--before` is a duration (`90d`), NOT a
        // timestamp: the cutoff is computed against THIS process's clock, which
        // is the same clock that stamped every `ts` in the table (the rule
        // `apikey.update --ttl` already follows — a client measuring an age
        // against its own wall clock is how a purge cuts the wrong line).
        "apikey.purge" => {
            let keys = rt
                .ctl_keys
                .as_ref()
                .ok_or_else(|| "no control key store".to_string())?;
            let before = req_str(a, "before")?;
            let cutoff = crate::apikeys::cutoff_from_age(&before)?;
            let dry = a
                .get("dryRun")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let older = keys.count_calls_older_than(&cutoff)?;
            let deleted = if dry {
                0
            } else {
                keys.purge_calls_older_than(&cutoff)?
            };
            Ok(json!({
                "before": before,
                "cutoff": cutoff,
                "older": older,
                "deleted": deleted,
                "dryRun": dry,
            }))
        }
        // The console's own plane: sign-in identity, accounts and their base-view
        // delegation, and the per-view detail page. Every one of these is judged
        // by `gates` above, exactly like the view verbs.
        "whoami" | "account.list" | "account.create" | "account.passwd" | "account.role"
        | "account.disable" | "account.delete" | "grant.list" | "grant.add" | "grant.remove"
        | "view.stats" => web_dispatch(rt, who, op, a),
        other => Err(format!(
            "unknown control op '{other}' (known: info, store.info, whoami, def.list/get/put/remove, stamp, physical.list/has, fork.has/create/drop/graft, view.create/stats/export/import, view.rm_base, refs.rebuild, gc.scan/apply, apikey.create/list/revoke/rotate/set_ro/update/calls/purge, account.list/create/passwd/role/disable/delete, grant.list/add/remove)"
        )),
    }
}

/// `list_defs` yields `(name, text)` pairs; the visibility rule is the same as
/// [`visible`]'s, applied to the name.
fn visible_pairs(
    rt: &Arc<MetaRuntime>,
    who: &crate::webauth::Principal,
    defs: Vec<(String, String)>,
) -> Vec<(String, String)> {
    defs.into_iter()
        .filter(|(n, _)| who.manages(rt.control.as_ref(), n).unwrap_or(false))
        .collect()
}

/// The JSON shape of one account row: what the console lists and edits. NEVER
/// includes the password digest — there is no such field on `AccountInfo`, which
/// is why a list can be handed to any admin without thinking about it.
fn acct_json(a: &crate::webauth::AccountInfo) -> serde_json::Value {
    use serde_json::json;
    json!({
        "account": a.account,
        "role": a.role,
        "created_at": a.created_at,
        "disabled_at": a.disabled_at,
        "last_login_at": a.last_login_at,
        "bases": a.bases,
    })
}

/// The account, delegation and view-detail verbs the web console is built from.
/// `authorize` has already decided that this caller may run `op`; what is decided
/// HERE is the shape, and the invariants that are not access rules:
///
/// - a password is checked against the policy before it is stretched, and the
///   digest is never returned or logged;
/// - an account store's last enabled admin cannot be demoted, disabled or
///   deleted, because that locks every future sign-in out of the deployment;
/// - a delegation is always stored against the BASE a name resolves to, so
///   granting a lens cannot create a grant that never matches.
fn web_dispatch(
    rt: &Arc<MetaRuntime>,
    who: &crate::webauth::Principal,
    op: &str,
    a: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    use crate::webauth::{self, Role};
    use serde_json::json;
    // Every verb below needs the account store, so it is bound once and its
    // absence is a refusal of the whole plane rather than of one op: a server
    // without `--control-listen` has no accounts, and saying that here is what
    // stops each arm from inventing its own version of the same complaint.
    let s = rt.web_auth.as_deref().ok_or_else(|| {
        "this server has no web account store (it needs --control-listen) — accounts and \n         delegations live beside the control plane's api keys".to_string()
    })?;
    match op {
        "whoami" => Ok(json!({
            "account": who.account,
            "role": who.role.as_str(),
            "super": who.is_super(),
            "bases": who.bases,
        })),
        "account.list" => {
            let rows: Vec<serde_json::Value> = s.list()?.iter().map(acct_json).collect();
            Ok(json!(rows))
        }
        "account.create" => {
            let account = webauth::normalize_account(&req_str(a, "account")?)?;
            let password = req_str(a, "password")?;
            // Named before the policy check and before the store's own UNIQUE
            // error: the console's form needs to say "that name is taken", and a
            // raw `UNIQUE constraint failed: web_accounts.account` is the backend
            // leaking into a person's face. A race still lands on the constraint,
            // which is the real guard.
            if let Some(cur) = s.get(&account)? {
                return Err(if cur.disabled_at.is_some() {
                    format!("a web account '{account}' already exists and is disabled — enable it (`web enable --account {account}`) rather than creating it again")
                } else {
                    format!("a web account '{account}' already exists — change its password with `web passwd --account {account}` if that is what is intended")
                });
            }
            let role = match a.get("role").and_then(|v| v.as_str()) {
                Some(r) => Role::parse(r)?,
                // The conservative default: a new account manages nothing until
                // it is delegated, so creating one cannot accidentally create an
                // administrator.
                None => Role::User,
            };
            webauth::check_password(&account, &password)?;
            s.create(&account, &webauth::hash_password(&password), role)?;
            Ok(json!({
                "account": account,
                "role": role.as_str(),
                "bases": Vec::<String>::new(),
                "note": if role == Role::Admin {
                    "this account governs the whole deployment — an admin's grants are not read"
                } else {
                    "delegate a base view before this account can manage anything"
                },
            }))
        }
        "account.passwd" => {
            let account = webauth::normalize_account(&req_str(a, "account")?)?;
            let password = req_str(a, "password")?;
            webauth::check_password(&account, &password)?;
            if !s.set_password(&account, &webauth::hash_password(&password))? {
                return Err(format!("no web account '{account}'"));
            }
            Ok(json!({"account": account, "passwordChanged": true}))
        }
        "account.role" => {
            let account = webauth::normalize_account(&req_str(a, "account")?)?;
            let role = Role::parse(&req_str(a, "role")?)?;
            let cur = s
                .get(&account)?
                .ok_or_else(|| format!("no web account '{account}'"))?;
            // Guarded on the way DOWN only, and against the number that matters:
            // an ENABLED admin. A deployment whose last admin is disabled is
            // already locked out and cannot be reached by this call at all.
            if role != Role::Admin
                && cur.role == Role::Admin.as_str()
                && cur.disabled_at.is_none()
                && s.active_admins()? <= 1
            {
                return Err(format!(
                    "'{account}' is the only enabled administrator — create a second admin and \n                     verify you can sign in as it before demoting this one"
                ));
            }
            if !s.set_role(&account, role)? {
                return Err(format!("no web account '{account}'"));
            }
            Ok(json!({"account": account, "role": role.as_str()}))
        }
        "account.disable" => {
            let account = webauth::normalize_account(&req_str(a, "account")?)?;
            let disabled = a.get("disabled").and_then(|v| v.as_bool()).unwrap_or(true);
            let cur = s
                .get(&account)?
                .ok_or_else(|| format!("no web account '{account}'"))?;
            if disabled
                && cur.role == Role::Admin.as_str()
                && cur.disabled_at.is_none()
                && s.active_admins()? <= 1
            {
                return Err(format!(
                    "'{account}' is the only enabled administrator — disabling it locks every \n                     sign-in out of the deployment; enable a second admin first"
                ));
            }
            if !s.set_disabled(&account, disabled)? {
                return Err(format!("no web account '{account}'"));
            }
            Ok(json!({"account": account, "disabled": disabled}))
        }
        "account.delete" => {
            let account = webauth::normalize_account(&req_str(a, "account")?)?;
            let cur = s
                .get(&account)?
                .ok_or_else(|| format!("no web account '{account}'"))?;
            if cur.role == Role::Admin.as_str() && cur.disabled_at.is_none() && s.active_admins()? <= 1
            {
                return Err(format!(
                    "'{account}' is the only enabled administrator — it cannot be deleted out of \n                     the last lock on the deployment"
                ));
            }
            // Its credentials outlive it on purpose (see `WebAuthStore::delete`),
            // so the reply has to say what still needs doing rather than let a
            // clean-looking success hide live mounts attributed to nobody.
            let keys = rt
                .ctl_keys
                .as_ref()
                .map(|k| {
                    k.list()
                        .map(|rows| {
                            rows.iter()
                                .filter(|r| {
                                    r.owner.as_deref() == Some(account.as_str())
                                        && r.revoked_at.is_none()
                                        // An expired row needs no revoking — it
                                        // already cannot authenticate.
                                        && !r.expired
                                })
                                .count()
                        })
                        .unwrap_or_default()
                })
                .unwrap_or_default();
            if !s.delete(&account)? {
                return Err(format!("no web account '{account}'"));
            }
            Ok(json!({
                "account": account,
                "deleted": true,
                "liveKeys": keys,
                "note": if keys > 0 {
                    format!("{keys} api key(s) still minted by this account are NOT revoked — revoke them explicitly if they should stop working")
                } else {
                    "its grants are gone; it minted no live api keys".to_string()
                },
            }))
        }
        "grant.list" => {
            let filter = a.get("account").and_then(|v| v.as_str());
            let rows: Vec<serde_json::Value> = s
                .list_grants()?
                .into_iter()
                .filter(|g| filter.is_none_or(|f| g.account == f))
                .map(|g| {
                    json!({
                        "account": g.account,
                        "base_view": g.base_view,
                        "perm": g.perm,
                        "granted_at": g.granted_at,
                        "granted_by": g.granted_by,
                    })
                })
                .collect();
            Ok(json!(rows))
        }
        "grant.add" => {
            let account = webauth::normalize_account(&req_str(a, "account")?)?;
            let named = req_str(a, "base")?;
            if s.get(&account)?.is_none() {
                return Err(format!(
                    "no web account '{account}' — create it first (`grovefs web account create`)"
                ));
            }
            // Stored against the BASE the name resolves to. A grant on a lens
            // would be a row that matches nothing (the ACL resolves outward from
            // any name to its base), so normalizing here turns a mistake into a
            // working grant and says what it did.
            let base = webauth::resolve_base(rt.control.as_ref(), &named)?;
            s.grant(&account, &base, Some(&who.account))?;
            Ok(json!({
                "account": account,
                "base": base,
                "asNamed": base == named,
                "grantedBy": who.account,
            }))
        }
        "grant.remove" => {
            let account = webauth::normalize_account(&req_str(a, "account")?)?;
            let named = req_str(a, "base")?;
            let base = webauth::resolve_base(rt.control.as_ref(), &named)
                .unwrap_or_else(|_| named.clone());
            // Removing the delegation an active session relies on takes effect on
            // that session's NEXT call (the principal is re-read per request), so
            // there is nothing to expire and nothing to invalidate here.
            let removed = s.ungrant(&account, &base)? || (base != named && s.ungrant(&account, &named)?);
            Ok(json!({"account": account, "base": base, "removed": removed}))
        }
        // The per-view page. Deliberately built from the CONTROL plane's own
        // answers (definitions, trees, stamps, keys, live sessions) instead of
        // walking the tree: a node/blob count would open an engine and read
        // every row, which on a large PG view is not something a page load may
        // do. `view export` and the mount's own `du` are the paths that cost
        // that, and they are explicit.
        "view.stats" => {
            let view = req_str(a, "view")?;
            let def = rt.control.get_def_raw(&view)?;
            let kind = match &def {
                Some(text) => {
                    let parsed = crate::layout::LayoutRules::parse_def(text)
                        .map_err(|e| format!("bad view definition '{view}': {e}"))?;
                    match parsed {
                        crate::layout::ViewDef::Derived(_, _) => "derived",
                        crate::layout::ViewDef::Fork(_) => "fork",
                        crate::layout::ViewDef::Alias(_) => "alias",
                    }
                }
                None if rt.control.has_physical(&view)? => "base",
                None => "none",
            };
            let base = webauth::resolve_base(rt.control.as_ref(), &view).ok();
            // The view's FAMILY: every name that resolves to this same base.
            // Only computed for a base name (a delegate's own view list is
            // small, and this is one resolve per name).
            let mut family: Vec<String> = Vec::new();
            if base.as_deref() == Some(view.as_str()) {
                let mut names: Vec<String> = rt.control.list_physical()?;
                names.extend(rt.control.list_defs()?.into_iter().map(|(n, _)| n));
                // The two listings OVERLAP by design: a fork has a definition
                // (`fork <base>`) and a tree of its own, so it appears on both
                // sides. A family is a set of names — a repeated one would
                // render as two rows for one view and double the resolve work.
                names.sort();
                names.dedup();
                for n in names {
                    if n != view
                        && webauth::resolve_base(rt.control.as_ref(), &n).ok().as_deref()
                            == Some(view.as_str())
                    {
                        family.push(n);
                    }
                }
                family.sort();
            }
            let keys = rt
                .ctl_keys
                .as_ref()
                .and_then(|k| {
                    k.list().ok().map(|rows| {
                        rows.iter()
                            .filter(|r| r.view.as_deref() == Some(view.as_str()))
                            .count()
                    })
                });
            let sessions = rt.subs.lock().get(&view).map(|m| m.len()).unwrap_or(0);
            let resident = rt.cache.lock().contains_key(base.as_deref().unwrap_or(&view))
                || rt.views.lock().contains_key(&view);
            Ok(json!({
                "name": view,
                "kind": kind,
                "base": base,
                "definition": def,
                "stamp": rt.control.stamp(&view),
                "tree": rt.control.has_physical(&view)?,
                "forkTree": rt.control.has_fork_tree(&view)?,
                "family": family,
                "keys": keys,
                "sessions": sessions,
                "resident": resident,
            }))
        }
        other => Err(format!("control_dispatch routed an unknown account op '{other}'")),
    }
}

// ---- portable dumps over the wire (export / import) -----------------------
//
// A dump is metadata only (ids, names, inodes, `data_ref` keys), yet it can be
// large — one line per node. So it moves in CHUNKS through a staging file under
// `<base>/meta/.xfer/`, which never ends in `.db` and is therefore invisible to
// every view lister and to `refs rebuild`. Handles are MINTED HERE, so a client
// cannot name a path at all; the chunk boundary is always a line boundary, so a
// chunk is always valid UTF-8 and a row is never split across two messages.

/// Default chunk size for one request/reply body (4 MiB). The client asks for
/// what it wants; this is only the server's default.
const XFER_CHUNK: u64 = 4 << 20;

fn xfer_dir(rt: &MetaRuntime) -> std::path::PathBuf {
    rt.base.join("meta").join(".xfer")
}

/// Strict handle check: minted by [`new_xfer_handle`], so anything else is a
/// client trying to name a path. `..`, separators and dots are all refused.
fn xfer_path(rt: &MetaRuntime, handle: &str) -> Result<std::path::PathBuf, String> {
    let ok = !handle.is_empty()
        && handle.len() <= 96
        && handle
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !ok {
        return Err(format!(
            "malformed transfer handle '{handle}' — handles are minted by the server"
        ));
    }
    Ok(xfer_dir(rt).join(handle))
}

fn new_xfer_handle(prefix: char) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{prefix}-{}-{nanos}", std::process::id())
}

/// Drop staging files nobody finished. Best effort and deliberately age-gated:
/// a transfer that is in flight MUST survive, so only files older than this are
/// removed (a crashed client is the case this exists for).
fn sweep_xfer(dir: &std::path::Path) {
    const STALE: std::time::Duration = std::time::Duration::from_secs(6 * 3600);
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for ent in rd.flatten() {
        let Ok(md) = ent.metadata() else { continue };
        let old = md
            .modified()
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > STALE);
        if old {
            let _ = std::fs::remove_file(ent.path());
        }
    }
}

/// Refuse to export a name that has no tree of its own, before any staging file
/// exists — the same guard both other arms apply, stated by name.
fn refuse_virtual_for_export(view: &str, definition: Option<&str>) -> Result<(), String> {
    let Some(text) = definition else {
        return Ok(());
    };
    match crate::layout::LayoutRules::parse_def(text)
        .map_err(|e| format!("bad view definition '{view}': {e}"))?
    {
        crate::layout::ViewDef::Alias(real) => Err(format!(
            "'{view}' is a pure alias of real view '{real}' — export '{real}' instead (an alias \
             has no metadata tree of its own)"
        )),
        crate::layout::ViewDef::Derived(base, _) => Err(format!(
            "'{view}' is a derived (virtual) view over '{base}' — export '{base}' instead (a \
             derived view has no metadata tree of its own)"
        )),
        crate::layout::ViewDef::Fork(_) => Ok(()),
    }
}

/// Render one view's tree into a staging file and hand the client a handle. The
/// CLI then pulls it in chunks; a failure here leaves no file behind (every
/// backend's dump writes beside its destination and renames).
fn view_export_stage(
    rt: &Arc<MetaRuntime>,
    view: &str,
    who: &crate::webauth::Principal,
) -> Result<serde_json::Value, String> {
    use serde_json::json;
    if !crate::layout::valid_view_name(view) {
        return Err(format!("invalid view name '{view}'"));
    }
    let definition = rt.control.get_def_raw(view)?;
    refuse_virtual_for_export(view, definition.as_deref())?;
    // Refused BEFORE any staging file or handle exists: the dump readers below
    // are SQLite/PG-shaped SQL, and a treedb tree has no equivalent yet.
    #[cfg(feature = "treedb")]
    if matches!(rt.store, StoreKind::Treedb(_)) {
        return Err(
            "view export/import is not implemented for a treedb store — treedb snapshots its \
             own directory (`treedb snapshot`), which is the durable dump of these trees"
                .to_string(),
        );
    }
    let dir = xfer_dir(rt);
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    sweep_xfer(&dir);
    let handle = new_xfer_handle('x');
    let path = xfer_path(rt, &handle)?;
    // Ownership is recorded the moment the handle exists, so the chunk pulls
    // that follow (each on its own connection, with no session to carry an
    // identity) are still served only to the principal that staged the dump.
    rt.claim_xfer(&handle, &who.account);
    let st = match &rt.store {
        StoreKind::Sqlite => crate::meta::export_view(&rt.base, view, &path, true)?,
        StoreKind::Pg(uri) => {
            crate::pg_store::export_view(uri, view, &path, true, definition.clone())?
        }
        #[cfg(feature = "treedb")]
        // The guard above already refused treedb, and naming the variant here
        // instead of with a catch-all is what keeps a future backend from
        // quietly reusing SQLite's dump reader.
        StoreKind::Treedb(_) => unreachable!("export is refused for a treedb store above"),
    };
    let size = std::fs::metadata(&path)
        .map_err(|e| format!("cannot stat staged export {handle}: {e}"))?
        .len();
    Ok(json!({
        "handle": handle,
        "size": size,
        "nodes": st.nodes,
        "blobs": st.blobs,
        "kind": st.kind,
    }))
}

/// Hand back one chunk of a staged export, ending on a line boundary.
fn view_export_chunk(
    rt: &Arc<MetaRuntime>,
    handle: &str,
    offset: u64,
    max: u64,
) -> Result<serde_json::Value, String> {
    use serde_json::json;
    use std::io::{Read, Seek, SeekFrom};
    let path = xfer_path(rt, handle)?;
    let mut f = std::fs::File::open(&path).map_err(|e| {
        format!(
            "no staged export '{handle}' ({e}) — a staged dump does not survive a server restart; \
             re-run `view export`"
        )
    })?;
    let total = f
        .metadata()
        .map_err(|e| format!("cannot stat staged export {handle}: {e}"))?
        .len();
    if offset > total {
        return Err(format!(
            "chunk offset {offset} is past the end of the staged export ({total} bytes)"
        ));
    }
    f.seek(SeekFrom::Start(offset))
        .map_err(|e| format!("cannot seek staged export {handle}: {e}"))?;
    let want = max.clamp(1, 64 << 20).min(total - offset);
    let mut buf = vec![0u8; want as usize];
    f.read_exact(&mut buf)
        .map_err(|e| format!("cannot read staged export {handle}: {e}"))?;
    let mut next = offset + buf.len() as u64;
    let mut eof = next >= total;
    if !eof {
        // Never split a row (or a UTF-8 sequence): cut at the last newline.
        // A single row longer than the window grows the read to its end.
        while !eof && !buf.contains(&b'\n') && buf.len() < (64 << 20) {
            let mut more = vec![0u8; (buf.len().max(1 << 10)).min((total - next) as usize)];
            let n = f
                .read(&mut more)
                .map_err(|e| format!("cannot read staged export {handle}: {e}"))?;
            buf.extend_from_slice(&more[..n]);
            next = offset + buf.len() as u64;
            eof = next >= total;
        }
        if !eof {
            let cut = buf
                .iter()
                .rposition(|&b| b == b'\n')
                .map(|p| p + 1)
                .unwrap_or(buf.len());
            buf.truncate(cut);
            next = offset + buf.len() as u64;
        }
    }
    let text = String::from_utf8(buf)
        .map_err(|e| format!("staged export '{handle}' is not valid UTF-8: {e}"))?;
    Ok(json!({"text": text, "next": next, "eof": next >= total}))
}

/// Mint a handle for an upload. The file starts empty; every `put` must arrive
/// at exactly the current length, so a retried or reordered chunk fails loudly
/// instead of silently corrupting the dump.
fn view_import_begin(
    rt: &Arc<MetaRuntime>,
    who: &crate::webauth::Principal,
) -> Result<serde_json::Value, String> {
    use serde_json::json;
    let dir = xfer_dir(rt);
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    sweep_xfer(&dir);
    let handle = new_xfer_handle('i');
    let path = xfer_path(rt, &handle)?;
    rt.claim_xfer(&handle, &who.account);
    std::fs::File::create(&path).map_err(|e| format!("cannot create {}: {e}", path.display()))?;
    Ok(json!({"handle": handle}))
}

fn view_import_put(
    rt: &Arc<MetaRuntime>,
    handle: &str,
    offset: u64,
    text: &str,
) -> Result<serde_json::Value, String> {
    use serde_json::json;
    use std::io::Write;
    let path = xfer_path(rt, handle)?;
    let have = std::fs::metadata(&path)
        .map_err(|e| {
            format!("no upload staged as '{handle}' ({e}) — call `view.import.begin` first")
        })?
        .len();
    if have != offset {
        return Err(format!(
            "upload '{handle}' is at {have} byte(s) but this chunk starts at {offset} — the \
             previous chunk did not land; restart the import"
        ));
    }
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    f.write_all(text.as_bytes())
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    let size = have + text.len() as u64;
    Ok(json!({"size": size}))
}

/// Install the uploaded dump, then (for `--as-fork`) record the fork definition.
/// The definition write is part of the SAME verdict: if it fails, the imported
/// tree is torn down again rather than left as a base view the operator believes
/// is a fork.
fn view_import_commit(
    rt: &Arc<MetaRuntime>,
    a: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    use serde_json::json;
    let view = req_str(a, "view")?;
    let handle = req_str(a, "handle")?;
    let path = xfer_path(rt, &handle)?;
    let replace = a.get("replace").and_then(|v| v.as_bool()).unwrap_or(false);
    let as_fork = a
        .get("as_fork")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    if !crate::layout::valid_view_name(&view) {
        return Err(format!("invalid view name '{view}'"));
    }
    if rt.control.get_def_raw(&view)?.is_some() {
        return Err(format!(
            "view '{view}' already has a definition — remove it first (`view rm --name {view}`) \
             before importing"
        ));
    }
    if let Some(base) = &as_fork {
        if !crate::layout::valid_view_name(base) {
            return Err(format!("invalid base view name '{base}'"));
        }
        if base == &view {
            return Err("fork name must differ from its base".to_string());
        }
        if rt.control.get_def_raw(base)?.is_some() {
            return Err(format!(
                "base '{base}' is governed by a definition (alias/derived/fork) — fork the \
                 physical base tree instead"
            ));
        }
        if !rt.control.has_physical(base)? {
            return Err(format!(
                "no physical base tree '{base}' — import the base first, or omit --as-fork"
            ));
        }
    }
    let st = match &rt.store {
        StoreKind::Sqlite => crate::meta::import_view(&rt.base, &view, &path, replace)?,
        StoreKind::Pg(uri) => crate::pg_store::import_view(uri, &view, &path, replace)?,
        #[cfg(feature = "treedb")]
        // No dump reader for treedb (see `view_export_stage`): the tree can only
        // be created empty and written through a mount.
        StoreKind::Treedb(_) => {
            return Err(
                "view export/import is not implemented for a treedb store — create the base with \
                 `view create` and write it through a mount".to_string(),
            )
        }
    };
    if let Some(base) = &as_fork {
        if let Err(e) = rt.control.put_def(&view, &format!("fork {base}\n")) {
            let rollback = drop_imported_tree(rt, &view);
            return Err(format!(
                "imported the tree but cannot record `fork {base}` for '{view}': {e}; {rollback}"
            ));
        }
    }
    rt.evict();
    let _ = std::fs::remove_file(&path);
    rt.release_xfer(&handle);
    Ok(json!({
        "nodes": st.nodes,
        "blobs": st.blobs,
        "kind": st.kind,
        "replaced": st.replaced,
        "as_fork": as_fork,
    }))
}

/// Undo a just-imported tree (the `--as-fork` rollback). Bytes are NOT touched:
/// they are still named by whatever the source tree is, and the ledger follows
/// the trees that exist.
fn drop_imported_tree(rt: &Arc<MetaRuntime>, view: &str) -> String {
    match &rt.store {
        StoreKind::Sqlite => {
            for ext in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(rt.base.join("meta").join(format!("{view}.db{ext}")));
            }
            let _ = crate::meta::rebuild_ledger(&rt.base);
            "the imported tree was removed again".to_string()
        }
        StoreKind::Pg(uri) => match crate::pg_store::PgEngine::remove_view(uri, view) {
            Ok(_) => "the imported tree was removed again".to_string(),
            Err(e) => format!(
                "the imported tree could NOT be removed ({e}) — drop schema for '{view}' by hand"
            ),
        },
        #[cfg(feature = "treedb")]
        StoreKind::Treedb(addr) => {
            match crate::treedb_store::TreedbStore::drop_view(addr, view) {
                Ok(_) => "the imported tree was removed again".to_string(),
                Err(e) => format!(
                    "the imported tree could NOT be removed ({e}) — drop view '{view}' in treedb at {addr} by hand"
                ),
            }
        }
    }
}

fn view_xfer_drop(rt: &Arc<MetaRuntime>, handle: &str) -> Result<serde_json::Value, String> {
    let path = xfer_path(rt, handle)?;
    let _ = std::fs::remove_file(&path);
    rt.release_xfer(handle);
    Ok(serde_json::Value::Null)
}

fn view_rm_base(
    rt: &Arc<MetaRuntime>,
    view: &str,
    data_root: Option<&str>,
) -> Result<serde_json::Value, String> {
    use serde_json::json;
    if !crate::layout::valid_view_name(view) {
        return Err(format!("invalid view name '{view}'"));
    }
    if rt.control.has_def(view)? {
        return Err(format!(
            "'{view}' is governed by a view definition — remove it first with `view rm --name {view}` (without --base)"
        ));
    }
    for (dname, text) in rt.control.list_defs()? {
        let dep = match crate::layout::LayoutRules::parse_def(&text) {
            Ok(crate::layout::ViewDef::Derived(b, _)) => b == view,
            Ok(crate::layout::ViewDef::Fork(b)) => b == view,
            Ok(crate::layout::ViewDef::Alias(r)) => r == view,
            Err(_) => false,
        };
        if dep {
            return Err(format!(
                "view '{dname}' is governed by a definition over base '{view}' — remove it first (`view rm --name {dname}`)"
            ));
        }
    }
    // Mounts on THIS host are visible, mounts on client machines are not —
    // stated in the reply rather than silently assumed away.
    if let Some(mp) = mounted_here(view) {
        return Err(format!(
            "view '{view}' is mounted ON THE SERVER HOST at {mp} — unmount it first; mounts on other machines are not visible from here, stop them too"
        ));
    }
    let droot = data_root
        .map(PathBuf::from)
        .unwrap_or_else(|| rt.base.clone());
    let report = match &rt.store {
        StoreKind::Pg(uri) => {
            let rep = crate::pg_store::PgEngine::remove_view(uri, view)?;
            let bytes = free_unbooked(&rep.unbooked_refs, &droot);
            format!(
                "removed base view '{view}' from {}: {} node(s) gone, {} object(s) no longer held by any view{}; {bytes}",
                crate::pg_store::redact_uri(uri),
                rep.entries,
                rep.unbooked,
                if rep.still_held > 0 {
                    format!(", {} still held by other views", rep.still_held)
                } else {
                    String::new()
                }
            )
        }
        StoreKind::Sqlite => {
            let rm = crate::meta::remove_base_view(&rt.base, view)?;
            let (mut stayed, mut refs) = (0usize, Vec::new());
            for dr in &rm.blob_refs {
                if crate::meta::ledger_holds(&rt.base, dr).unwrap_or(true) {
                    stayed += 1;
                    continue;
                }
                refs.push(dr.clone());
            }
            let bytes = free_unbooked(&refs, &droot);
            format!(
                "removed base view '{view}': {} node(s) gone, {} blob reference(s) dropped, {stayed} still held by other views; {bytes}",
                rm.nodes,
                rm.blob_refs.len()
            )
        }
        #[cfg(feature = "treedb")]
        StoreKind::Treedb(addr) => {
            // treedb keeps the holder count inside the tree itself, so there is
            // no separate ledger to consult and nothing to report about refs
            // that other views still hold: dropping a view frees exactly its own
            // rows. The BYTES are a data server's business, as under PG.
            crate::treedb_store::TreedbStore::drop_view(addr, view)
                .map_err(|e| format!("cannot drop treedb view '{view}' at {addr}: {e}"))?;
            format!(
                "removed base view '{view}' from treedb at {addr}; content objects are not touched — they belong to the data server(s)"
            )
        }
    };
    // Drop cached engines: the tree they point at no longer exists.
    rt.evict();
    Ok(json!(report))
}

fn refs_rebuild(rt: &Arc<MetaRuntime>) -> Result<serde_json::Value, String> {
    use serde_json::json;
    match &rt.store {
        StoreKind::Pg(uri) => {
            let rep = crate::pg_store::rebuild_ledger(uri)?;
            Ok(json!(format!(
                "refs ledger rebuilt in {}: {} tree(s), {} content key(s), {} holder unit(s); {} row(s) corrected, {} row(s) deleted",
                crate::pg_store::redact_uri(uri),
                rep.trees, rep.refs, rep.holders, rep.rows_fixed, rep.rows_deleted
            )))
        }
        StoreKind::Sqlite => {
            let n = crate::meta::rebuild_ledger(&rt.base).map_err(|e| e.to_string())?;
            Ok(json!(format!(
                "refs ledger rebuilt at {}: {n} holder entries across the surviving views",
                rt.base.join("meta").join("refs.db").display()
            )))
        }
        #[cfg(feature = "treedb")]
        // There is no ledger to rebuild: treedb derives the holder count from
        // the live tree inside each op, so a stale count is not a state it can
        // reach. Refusing beats rewriting rows the engine owns.
        StoreKind::Treedb(_) => Err(
            "`refs rebuild` has nothing to do for a treedb store: holder counts are maintained \
             inside the engine, never in a separate ledger".to_string(),
        ),
    }
}

/// What this server's store says its trees name, in the one shape the audit
/// judges: the same [`crate::gc::Picture`] the offline scan builds from
/// `meta/*.db`. Reading the trees WHERE THEY LIVE is the whole reason this runs
/// on the server — an operator's machine has neither the local trees nor the
/// PG schemas, only the admin key.
fn gc_picture(rt: &Arc<MetaRuntime>) -> Result<crate::gc::Picture, String> {
    match &rt.store {
        StoreKind::Sqlite => crate::gc::picture_sqlite(&rt.base),
        StoreKind::Pg(uri) => {
            let p = crate::pg_store::reference_picture(uri)?;
            Ok(crate::gc::Picture {
                refs: crate::gc::Refs::from_keys(p.trees, p.refs.into_keys()),
                ledger: crate::gc::Ledger::from_keys(
                    p.booked.into_iter().filter(|(_, h)| *h > 0).map(|(k, _)| k),
                ),
            })
        }
        // An empty Picture is NOT an acceptable answer here: every byte would
        // look unreferenced and an audit that trusts it would delete live data.
        // treedb can dump its trees (`dump_tree` feeds `du`), but the ledger
        // side has no aggregate query yet, so the audit refuses to judge.
        #[cfg(feature = "treedb")]
        StoreKind::Treedb(_) => Err(
            "gc audit is not implemented for a treedb store — it would report every object as \
             garbage, which is worse than refusing".to_string(),
        ),
    }
}

/// Walk ONE data server's object pages to the end.
///
/// A server that cannot enumerate (an older build, or a backend with no
/// listing) ABORTS the audit: reporting a healthy-looking zero for bytes nobody
/// listed is the one answer worse than an error.
fn list_server(
    be: &crate::remote::client::RemoteBackend,
    addr: &str,
) -> Result<Vec<crate::backend::ObjectRow>, String> {
    use crate::backend::StorageBackend;
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = be.list_objects(cursor.as_deref(), 4096).map_err(|e| {
            format!(
                "data server {addr} cannot enumerate its objects ({e}) — refusing to audit bytes nobody listed"
            )
        })?;
        out.extend(page.rows);
        match page.next {
            // A cursor that does not advance would make this loop re-judge the
            // same keys forever, so it is an error rather than a retry.
            Some(next) if cursor.as_deref() != Some(next.as_str()) => cursor = Some(next),
            Some(next) => {
                return Err(format!(
                    "data server {addr} repeated page cursor '{next}' — it cannot enumerate a stable walk, so the audit would see the same objects twice"
                ))
            }
            None => return Ok(out),
        }
    }
}

/// The control-plane half of `gc`: the TREES are read here, where they live
/// (this server's backend), and the OBJECTS are listed from the data servers
/// named in the request — so `gc` works from any machine holding an admin key,
/// for every backend, instead of demanding a local `<data-dir>` that only exists
/// on the server host.
///
/// Two limits are reported, never papered over: the silence proof covers THIS
/// server's own sessions plus mounts on this host (a client machine's mount is
/// invisible here, exactly as it is to `view rm --base`), and the grace window
/// is judged with this host's clock against the data servers' file mtimes.
/// Reclaim deletes only orphans older than the window, through the same
/// [`crate::gc::audit`] verdict the offline scan uses; a ledger-held key is never
/// touched.
fn gc_audit(
    rt: &Arc<MetaRuntime>,
    apply: bool,
    a: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    use crate::backend::StorageBackend;
    use serde_json::json;
    let servers: Vec<String> = a
        .get("data_servers")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if servers.is_empty() {
        return Err(
            "gc needs at least one `--data-servers HOST:PORT`: the bytes are owned by the data \
             servers, not by the machine that runs this audit"
                .to_string(),
        );
    }
    let token = a
        .get("data_token")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let api_key = a
        .get("data_api_key")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let grace = crate::gc::parse_age(
        a.get("older_than")
            .and_then(|v| v.as_str())
            .unwrap_or("24h"),
    )?;
    let force = a.get("force").and_then(|v| v.as_bool()).unwrap_or(false);

    let pic = gc_picture(rt)?;
    // Silence, as far as THIS server can see it: every live mount session it
    // serves, plus a mount of the same view on its own host.
    let sessions: Vec<(String, usize)> = rt
        .subs
        .lock()
        .iter()
        .filter(|(_, m)| !m.is_empty())
        .map(|(v, m)| (v.clone(), m.len()))
        .collect();
    let mounts: Vec<String> = pic
        .refs
        .views
        .iter()
        .filter_map(|v| mounted_here(v).map(|mp| format!("{v} at {mp}")))
        .collect();
    if apply && (!sessions.is_empty() || !mounts.is_empty()) {
        let mut live: Vec<String> = sessions
            .iter()
            .map(|(v, n)| format!("{v} ({n} live session(s) on this server)"))
            .collect();
        live.extend(mounts.iter().cloned());
        return Err(format!(
            "gc --apply refused: this deployment is in use — {}; stop those mounts first (reclaim \
             must run offline, and a mount on another machine is not visible from here, so stop it too)",
            live.join("; ")
        ));
    }

    let mut objects: Vec<(String, u64, std::time::SystemTime)> = Vec::new();
    let mut backends: Vec<crate::remote::client::RemoteBackend> = Vec::new();
    for addr in &servers {
        let be = crate::remote::client::RemoteBackend::new_with_creds(addr, token, api_key)
            .map_err(|e| format!("cannot reach data server {addr}: {e}"))?;
        for row in list_server(&be, addr)? {
            let mtime = std::time::UNIX_EPOCH + Duration::from_secs(row.mtime_secs.max(0) as u64);
            objects.push((row.key, row.size, mtime));
        }
        backends.push(be);
    }
    let aud = crate::gc::audit(objects, &pic, grace);
    // The same wrong-root guard the offline scan applies: a data server that
    // shares ZERO object with any tree here is almost always the wrong
    // deployment's bytes, and deleting there would kill another store.
    if aud.objects > 0 && !pic.refs.keys.is_empty() && aud.matched == 0 && !force {
        return Err(format!(
            "gc: the data server(s) {} share no object with any view tree on this server ({} \
             object(s), {} referenced key(s)) — wrong --data-servers? Nothing was deleted; pass \
             --force only if this is expected",
            servers.join(","),
            aud.objects,
            pic.refs.keys.len()
        ));
    }
    let reclaimed = if apply {
        let mut rec = crate::gc::Reclaimed::default();
        for c in &aud.orphans {
            if c.within_grace {
                rec.within_grace += 1;
                continue;
            }
            // The key could have landed on any server AND any of its roots; ask
            // EVERY backend to drop it — a server that has it reclaims it, one
            // that has not answers "already gone". An Ok from the FIRST server
            // alone proved nothing about the others (and their roots), which is
            // exactly how a planted mis-rooted orphan stayed on disk while the
            // report claimed it freed. Held keys never get here.
            let mut last = String::new();
            let mut gone = true;
            for be in &backends {
                if let Err(e) = be.remove(&c.key) {
                    gone = false;
                    last = e.to_string();
                }
            }
            if gone {
                rec.deleted += 1;
                rec.bytes = rec.bytes.saturating_add(c.size);
            } else {
                rec.failed.push(format!("{}: {last}", c.key));
            }
        }
        Some(rec)
    } else {
        None
    };

    let meta = match &rt.store {
        StoreKind::Sqlite => rt.base.display().to_string(),
        StoreKind::Pg(_) => "PostgreSQL (one schema per view; the URI is never echoed)".to_string(),
        #[cfg(feature = "treedb")]
        StoreKind::Treedb(addr) => format!("treedb://{addr} (one tree per view)"),
    };
    let cand = |c: &crate::gc::Judged| {
        json!({
            "key": c.key,
            "size": c.size,
            "age_secs": crate::gc::age_secs(c.mtime),
            "kind": c.kind_str(),
            "within_grace": c.within_grace,
        })
    };
    Ok(json!({
        "backend": rt.control.backend(),
        "meta_root": meta,
        "data_roots": servers,
        "views": pic.refs.views,
        "referenced_keys": pic.refs.keys.len(),
        "striped_layouts": pic.refs.striped.len(),
        "objects": aud.objects,
        "object_bytes": aud.objects_bytes,
        "matched_objects": aud.matched,
        "orphans": aud.orphans.iter().map(cand).collect::<Vec<_>>(),
        "orphan_bytes": aud.orphan_bytes(),
        "reclaimable": aud.reclaimable(),
        "reclaimable_bytes": aud.reclaimable_bytes(),
        "within_grace": aud.within_grace(),
        "ledger_held": aud.held.iter().map(cand).collect::<Vec<_>>(),
        "ledger_held_bytes": aud.held_bytes(),
        "skipped": Vec::<String>::new(),
        "grace_secs": grace.as_secs(),
        "applied": reclaimed.is_some(),
        "deleted": reclaimed.as_ref().map(|r| r.deleted).unwrap_or(0),
        "deleted_bytes": reclaimed.as_ref().map(|r| r.bytes).unwrap_or(0),
        "delete_failed": reclaimed.as_ref().map(|r| r.failed.clone()).unwrap_or_default(),
        "live_sessions": sessions
            .iter()
            .map(|(v, n)| json!({"view": v, "sessions": n}))
            .collect::<Vec<_>>(),
        "server_mounts": mounts,
        "note": "the silence proof covers this server's sessions and mounts on its host; a mount on another machine is invisible here, and the grace window is judged with this host's clock against the data servers' mtimes",
    }))
}

/// The first mountpoint on THIS host whose FUSE source is `grovefs:<view>`.
/// Same predicate `view rm --base` uses locally; kept here because the server
/// must refuse to delete a tree out from under its own mount table.
fn mounted_here(view: &str) -> Option<String> {
    let content = std::fs::read_to_string("/proc/mounts").ok()?;
    let tag = format!("grovefs:{view}");
    for l in content.lines() {
        let mut it = l.split_whitespace();
        let (Some(dev), Some(mp)) = (it.next(), it.next()) else {
            continue;
        };
        if dev == tag {
            return Some(mp.replace("\\040", " "));
        }
    }
    None
}

/// Millisecond epoch clock for the engine-cache idle stamps. Coarse is fine:
/// the only consumer compares it against a seconds-scale idle window.
fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Ask every cached engine to shed the burst connections it is holding above
/// its warm floor, and total up what closed. Handles are cloned out under the
/// cache lock and called after it is released — reaping talks to PostgreSQL,
/// and no dispatch path may wait on that behind this lock.
///
/// Deliberately NOT gated on idleness: a mounted view whose burst has already
/// passed is exactly the case eviction cannot serve (sessions pin the engine),
/// and its idle writers are the ones costing `max_connections` right now.
fn reap_pools(rt: &MetaRuntime) -> usize {
    let engines: Vec<Arc<MetaStore>> = rt
        .cache
        .lock()
        .values()
        .map(|(store, _)| store.clone())
        .collect();
    engines.iter().map(|s| s.shrink_pools()).sum()
}

/// Retire engine cache entries for base views nobody is using: no mounted
/// session (subscriber) AND no dispatch within `engine_idle_ms`. This is what
/// lets one meta-server host far more views than the PG backend could keep
/// CONNECTED: a reclaimed engine closes its pooled writers/readers, and the
/// next op re-opens against the same tree — the reopen path is the one already
/// exercised wholesale by the HA/fence `MetaRuntime::evict` flush.
///
/// The sweeper itself is backend-agnostic (it drops cache entries; it does not
/// know what an engine holds), and that is the right seam, but WHAT it frees
/// differs per backend and the difference is not cosmetic: under PG a dropped
/// entry closes the view's whole pool — warm floor and any burst connections
/// retained above it — which is why the knob is the PG scalability gate at all.
/// See [`MetaServer::with_engine_idle_secs`]. No branch is needed here
/// precisely because eviction is "forget the entry, reopen on next op" for both
/// stores. Burst shedding without eviction is the job of [`reap_pools`].
///
/// In-flight ops are safe by construction: they hold their own `Arc` clones, so
/// removing a map entry only releases the CONNECTIONS once the last clone
/// drops; an op dispatching into a just-evicted view transparently rebuilds
/// the entry. Returns the number of base views evicted.
fn evict_idle(rt: &MetaRuntime) -> usize {
    if rt.engine_idle_ms == 0 {
        return 0;
    }
    let now = now_millis();
    let idle = rt.engine_idle_ms;
    let is_idle = |st: &AtomicU64| now.saturating_sub(st.load(Ordering::Relaxed)) >= idle;
    // One nested scope: no other path holds two of these maps at once (every
    // dispatch uses statement-scoped guards), so the sweeper cannot deadlock
    // against it.
    let subs = rt.subs.lock();
    let mut cache = rt.cache.lock();
    let mut views = rt.views.lock();
    let mut bases = rt.bases.lock();
    let mut defs = rt.defs.lock();
    // base → client names resolved onto it (lenses/aliases/forks and, per
    // `bases` semantics, the physical name itself).
    let mut groups: HashMap<String, Vec<String>> = HashMap::new();
    for (name, b) in bases.iter() {
        groups.entry(b.clone()).or_default().push(name.clone());
    }
    let candidates: Vec<String> = cache
        .iter()
        .filter(|(_b, (_s, st))| is_idle(st))
        .map(|(b, _)| b.clone())
        .collect();
    let mut gone = 0;
    for base in candidates {
        let mut names = groups.remove(&base).unwrap_or_default();
        if !names.contains(&base) {
            names.push(base.clone());
        }
        // The whole group must be idle: any busy provider entry (a lens being
        // traversed, a mounted physical view) pins the shared engine.
        if names
            .iter()
            .any(|n| views.get(n.as_str()).is_some_and(|(_p, st)| !is_idle(st)))
        {
            continue;
        }
        // Mounted sessions pin the tree even when nobody types anything.
        if subs.get(base.as_str()).is_some_and(|m| !m.is_empty()) {
            continue;
        }
        cache.remove(&base);
        for n in &names {
            views.remove(n.as_str());
            bases.remove(n.as_str());
            defs.remove(n.as_str());
        }
        gone += 1;
    }
    gone
}

/// Open (or reuse) the authoritative PHYSICAL engine for a base view name.
/// A HA standby opens its replicated copy strictly read-only (so a stray
/// write is refused at the SQLite layer, not merely discouraged); a primary
/// opens read-write. Under [`StoreKind::Pg`] the standby/readonly path never
/// fires (`serve()` refuses the combination), so `readonly` is passed through
/// as `false` — the same for [`StoreKind::Treedb`], which is refused against
/// `--role standby` at startup. Keyed by the on-disk tree name — derived display
/// views live in [`MetaRuntime::views`] and are resolved by [`view_provider`].
fn store_for(rt: &MetaRuntime, view: &str) -> io::Result<Arc<MetaStore>> {
    let mut m = rt.cache.lock();
    if let Some((e, stamp)) = m.get_mut(view) {
        stamp.store(now_millis(), Ordering::Relaxed);
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
            .map_err(|e| {
                // The URI carries the PG password and this error travels back to
                // the caller (and into the audit `detail` column), so report it
                // redacted — the account and host are what identify the home,
                // the secret never is.
                io::Error::other(format!(
                    "pg open for view='{view}' uri='{}': {e}",
                    crate::pg_store::redact_uri(uri)
                ))
            })?,
        #[cfg(feature = "treedb")]
        // Opening a treedb store resolves the view id and, for a writable
        // handle, creates the tree when it is missing — which `M_OPEN` must
        // never do (the banner promises no empty tree is ever auto-created).
        // So existence is confirmed against the engine's own list first; the
        // result is cached per view like every other backend, so this costs one
        // `ListViews` per cold view, not one per mount.
        StoreKind::Treedb(addr) => {
            let known = crate::treedb_store::TreedbStore::list_views(addr)
                .map_err(|e| io::Error::other(format!("treedb list views at {addr}: {e}")))?;
            if !known.iter().any(|v| v == view) {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no base tree '{view}' in treedb at {addr} — create it (`view create`)"),
                ));
            }
            MetaStore::open_treedb(addr, view, false)
                .map_err(|e| io::Error::other(format!("treedb open for view='{view}' at {addr}: {e}")))?
        }
    };
    let engine = Arc::new(store);
    m.insert(
        view.to_string(),
        (engine.clone(), Arc::new(AtomicU64::new(now_millis()))),
    );
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
    // A treedb tree is invisible to the control store's physical enumeration
    // (that lists local db files), so under this backend the ENGINE is asked —
    // the same source `store_for` gates on, so the two can never disagree about
    // what exists.
    #[cfg(feature = "treedb")]
    if let StoreKind::Treedb(addr) = &rt.store {
        return crate::treedb_store::TreedbStore::list_views(addr)
            .map(|vs| vs.iter().any(|v| v == view))
            .map_err(io::Error::other);
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
    let cached = rt.views.lock().get(view).map(|(p, stamp)| {
        stamp.store(now_millis(), Ordering::Relaxed);
        p.clone()
    });
    if let Some(p) = cached {
        // Hot reload: a cached DERIVED entry is only served while its
        // definition is unchanged in the control store (a file `size:mtime`, a
        // PG row `xmin`; a cheap probe per op is
        // negligible next to a FUSE round trip). An edited definition evicts the
        // lens and every live
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
            .is_some_and(|seen| control.defs_changed(view, seen));
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
            // under SQLite, a `grovefs_v_<hex>` schema under PG — both seed it in
            // one transaction). The
            // definition only records which physical view seeded it. It is
            // resolved like a physical store (same cache, same subscriptions) but
            // deliberately NOT recorded in `defs`: `is_derived` stays false, so
            // structure writes are admitted while the client-side ack flag
            // (below) keeps content writes out.
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
    rt.views.lock().insert(
        view.to_string(),
        (provider.clone(), Arc::new(AtomicU64::new(now_millis()))),
    );
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
/// TTL and without `--no-cache`.
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
        let has_subs = rt.subs.lock().get(&dname).is_some_and(|m| !m.is_empty());
        if !has_subs {
            continue;
        }
        let prov = rt.views.lock().get(&dname).map(|(p, _)| p.clone());
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
            // Fill the placeholders with the concrete store root and the real
            // view name, so a remote operator gets a command they can paste on
            // the meta-server host rather than `--meta-dir <meta-dir> --view
            // <name>` (which they must mentally expand) (§6-5).
            let root = rt.base.display();
            let hint = match &rt.store {
                // Under treedb the CONTROL store is local SQLite, so naming it
                // in the hint would tell the operator to create a db file this
                // server never reads: a treedb base tree is made through the
                // server's own governance port.
                #[cfg(feature = "treedb")]
                StoreKind::Treedb(addr) => format!(
                    "a treedb base view is a tree inside the engine at {addr}: create it against this meta-server (`grovefs view --control-server <host:port> --api-key <admin-key> create --view {view}`)"
                ),
                _ if rt.control.backend() == "postgres" => format!(
                    "a PG base view is a schema: create it first with `grovefs view --meta-store postgres://… --meta-dir {root} create --view {view}`, or restore the schema from a dump"
                ),
                _ => format!(
                    "create it first: `grovefs view --meta-dir {root} create --view {view}` (or `grovefs --home {root} init --view {view}`, or seed it with `grovefs view import`)"
                ),
            };
            // `view` is the POST-ALIAS real name; name the alias the caller
            // actually mounted too, so "pub does not exist" never reads as
            // if the alias itself were unknown.
            #[cfg(feature = "treedb")]
            let backend = if matches!(rt.store, StoreKind::Treedb(_)) {
                // The store that was actually asked about, not the one holding
                // the credentials.
                "treedb"
            } else {
                rt.control.backend()
            };
            #[cfg(not(feature = "treedb"))]
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
    // The ack body: three flag bytes, then — only when this node mints
    // Data-Access Capabilities (`--data-access-key`) — a u16-length-prefixed
    // capability the client must replay to its data-server. Appending it AFTER
    // the flags keeps old clients working (they read `..3` and ignore the tail);
    // a data-server NOT in verify mode likewise ignores it. The capability
    // proves this session passed a genuine meta `M_OPEN`, so a rogue meta that
    // lacks the shared key cannot arm the honest data plane (see
    // [`crate::remote::dac`]).
    let mut ack = vec![client_ro as u8, paged_cap, content_ro as u8];
    if let Some(key) = &rt.data_access_key {
        let expiry = if rt.data_access_ttl == 0 {
            0
        } else {
            crate::remote::dac::now_secs().saturating_add(rt.data_access_ttl)
        };
        proto::put_key(&mut ack, &crate::remote::dac::issue(key, expiry));
    }
    proto::send_response(&mut writer, 0, open_epoch, &ack)?;

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
    // handshake — and so does an elapsed `--ttl`, because `live_state` reports
    // both as not live. The same tick refreshes the key's CURRENT `ro` flag, so
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
                        // The real cause, in the handshake's own words: a mount
                        // cut off by an elapsed `--ttl` must not be told its key
                        // was revoked (one repair is `apikey update --ttl`, the
                        // other is re-minting). This sentence is also what the
                        // client prints on stderr and what the audit row stores.
                        let why = store.dead_reason(&a.id, crate::apikeys::Scope::Mount);
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
/// The control-plane stores are excluded by the same predicate every other
/// enumerator uses, so the api-key, web-account and ledger files are never
/// mistaken for view trees here.
fn list_users(base: &Path) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(base.join("meta")) {
        for ent in rd.flatten() {
            let p = ent.path();
            let Some(file) = p.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if crate::meta::is_view_db(file) {
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

/// The row a `CREATE`/`SYMLINK` just wrote, serialized into the reply.
///
/// The client needs the entry attributes to fill its FUSE reply; shipping them
/// here removes one `M_GETATTR` round trip per creation (a `touch` used to cost
/// three metadata RTTs). Empty when the row is already gone (unlinked in
/// between) — the client then falls back to its own `GETATTR`, so the payload is
/// purely an optimization and the old client/server pairing keeps working.
fn node_reply(engine: &dyn MetaProvider, id: i64) -> Vec<u8> {
    let mut d = Vec::new();
    if let Some(n) = engine.getattr(id) {
        put_node(&mut d, &n);
    }
    d
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
            match engine.lookup_checked(parent, &name) {
                Ok(Some(n)) => {
                    let mut d = Vec::new();
                    put_node(&mut d, &n);
                    (0, 0, d, Vec::new())
                }
                Ok(None) => (E_NOTFOUND, 0, Vec::new(), Vec::new()),
                // "Could not ask" is NOT "not there": the client maps E_DB to
                // EIO, which is what an operator sees during an engine outage
                // instead of a mass delete (see `MetaProvider::lookup_checked`).
                Err(e) => bail!(e),
            }
        }
        M_GETATTR => {
            let ino = match get_i64(body, &mut i) {
                Ok(v) => v,
                Err(_) => return (E_DB, 0, Vec::new(), Vec::new()),
            };
            match engine.getattr_checked(ino) {
                Ok(Some(n)) => {
                    let mut d = Vec::new();
                    put_node(&mut d, &n);
                    (0, 0, d, Vec::new())
                }
                Ok(None) => (E_NOTFOUND, 0, Vec::new(), Vec::new()),
                Err(e) => bail!(e),
            }
        }
        M_READDIR => {
            let parent = match get_i64(body, &mut i) {
                Ok(v) => v,
                Err(_) => return (E_DB, 0, Vec::new(), Vec::new()),
            };
            let nodes = match engine.readdir_checked(parent) {
                Ok(v) => v,
                // A failed listing must not go back as an empty one: the client
                // reads an empty page as "directory ended" and truncates `ls`.
                Err(e) => return bail!(e),
            };
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
            let nodes = match engine.readdir_page_checked(parent, offset as usize, limit as usize) {
                Ok(v) => v,
                Err(e) => return bail!(e),
            };
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
                    node_reply(engine, id),
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
                    node_reply(engine, id),
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
    // The blocking event wait has NO read timeout by design; keepalive is what
    // turns a vanished network (no FIN/RST) into a read error ~2 min later, so
    // the listener thread exits and its reconnect loop can restore pushes —
    // without this, a silently dead link pins the mount's invalidations forever.
    crate::remote::enable_tcp_keepalive(&s);
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

// Split a `--meta-server` value into a failover address list. Lives in the client
// half (`super::split_addrs`, always compiled); re-exported here via
// `use super::*;` so the meta-server failover paths keep calling it.

/// Keep this mount's kernel cache coherent with peers WITHOUT `--no-cache`: connect
/// an event stream to the meta-server and translate each pushed frame into a
/// [`Notifier`] call (`inval_entry` / `inval_inode`), so a peer's create/delete/
/// rename and even an overwrite of an already-cached file become visible right
/// away. Reconnects with a small bounded backoff across a server restart; stops
/// permanently only on a fatal token rejection. Spawned by the mount process for
/// `--meta-server` mounts; abandoned harmlessly when the process exits at unmount.
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
                crate::remote::signal_auth_loss(&e.to_string());
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
    fn create_and_symlink_replies_carry_the_row_in_one_round_trip() {
        // The FUSE layer needs the entry attributes immediately after a create.
        // The server now ships the row it just wrote inside the CREATE/MKDIR/
        // SYMLINK reply, so the client needs no follow-up GETATTR — a create
        // used to cost three metadata round trips, two after this (USAGE §4.1.1).
        // This pins both the payload and the client's decoding of it.
        use crate::meta::{NodeKind, ROOT_INO};

        let dir = tempfile::Builder::new()
            .prefix("yf_createattr_")
            .tempdir()
            .unwrap();
        let base = dir.path().to_path_buf();
        drop(MetaStore::open_sqlite(&base, "alice", false).unwrap());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || serve_on_meta_auth(listener, base, ""));

        let meta = RemoteMeta::new(&addr, "alice").expect("handshake with a live server");

        let d = meta
            .create_node_with_attr(ROOT_INO, "d", true, None, 0o750, 12, 34)
            .expect("mkdir reply carries the row");
        assert_eq!(d.name, "d");
        assert_eq!(d.kind, NodeKind::Dir);
        assert_eq!(d.mode & 0o777, 0o750, "mode must survive the reply");
        assert_eq!((d.uid, d.gid), (12, 34));

        let f = meta
            .create_node_with_attr(d.id, "f", false, None, 0o640, 12, 34)
            .expect("create reply carries the row");
        assert_eq!(f.parent_id, d.id);
        assert_eq!(f.kind, NodeKind::Regular);
        assert_eq!(f.mode & 0o777, 0o640);

        let l = meta
            .create_symlink_with_attr(d.id, "l", "f", 12, 34)
            .expect("symlink reply carries the row");
        assert_eq!(l.kind, NodeKind::Symlink);
        assert_eq!(l.link_target.as_deref(), Some("f"));

        // What the reply claimed must be what the store holds: a reply-only
        // attribute would be worse than the extra round trip it replaced.
        let stored = meta.getattr(l.id).expect("row exists");
        assert_eq!(stored.name, "l");
        assert_eq!(stored.ctime, l.ctime);
        assert_eq!(stored.mode, l.mode);
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
            data_access_key: None,
            data_access_ttl: 0,
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
            engine_idle_ms: 0,
            audit_retention: None,
            ctl_keys: None,
            web_auth: None,
            xfer_owner: Mutex::new(HashMap::new()),
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

    #[test]
    fn idle_engines_evict_only_without_sessions_or_traffic() {
        // A cache entry stamped older than the idle window is swept and
        // transparently re-opened afterwards; a derived name still fresh on
        // the base keeps the WHOLE group resident (the connection-relevant
        // guarantee: nothing is dropped while any live user of the tree can
        // still dispatch through it).
        let dir = tempfile::Builder::new()
            .prefix("yf_evict_")
            .tempdir()
            .unwrap();
        let mut rt = MetaRuntime {
            base: dir.path().to_path_buf(),
            token: String::new(),
            data_access_key: None,
            data_access_ttl: 0,
            keys: None,
            cache: Arc::new(Mutex::new(HashMap::new())),
            views: Arc::new(Mutex::new(HashMap::new())),
            bases: Arc::new(Mutex::new(HashMap::new())),
            defs: Arc::new(Mutex::new(HashMap::new())),
            control: crate::mcp::control_store_for(dir.path(), None)
                .expect("file control store is infallible"),
            subs: Arc::new(Mutex::new(HashMap::new())),
            ha: None,
            peer: None,
            forward: Mutex::new(Vec::new()),
            sync_repl: false,
            standby_push: Mutex::new(()),
            witness: None,
            store: StoreKind::Sqlite,
            engine_idle_ms: 60_000,
            audit_retention: None,
            ctl_keys: None,
            web_auth: None,
            xfer_owner: Mutex::new(HashMap::new()),
        };
        let a = store_for(&rt, "v1").unwrap();
        assert!(rt.cache.lock().contains_key("v1"));
        // Age the base stamp to epoch 0: idle under any real window.
        rt.cache
            .lock()
            .get("v1")
            .unwrap()
            .1
            .store(0, Ordering::Relaxed);
        // A fresh lens provider over the base marks the group busy → skip.
        rt.bases
            .lock()
            .insert("lens1".to_string(), "v1".to_string());
        rt.views.lock().insert(
            "lens1".to_string(),
            (
                a.clone() as Arc<dyn MetaProvider>,
                Arc::new(AtomicU64::new(now_millis())),
            ),
        );
        assert_eq!(evict_idle(&rt), 0, "a busy derived name pins its base");
        assert!(rt.cache.lock().contains_key("v1"));
        // The lens goes idle too → the whole group is swept in one pass.
        rt.views
            .lock()
            .get("lens1")
            .unwrap()
            .1
            .store(0, Ordering::Relaxed);
        assert_eq!(evict_idle(&rt), 1);
        assert!(!rt.cache.lock().contains_key("v1"));
        assert!(!rt.views.lock().contains_key("lens1"));
        assert!(!rt.bases.lock().contains_key("lens1"));
        // Re-open: a fresh engine for the same tree, old handle still usable.
        let b = store_for(&rt, "v1").unwrap();
        assert!(!Arc::ptr_eq(&a, &b), "eviction must actually replace");
        assert!(rt.cache.lock().contains_key("v1"));
        // `0` disables the sweeper outright (embedder / test-server shape).
        rt.cache
            .lock()
            .get("v1")
            .unwrap()
            .1
            .store(0, Ordering::Relaxed);
        rt.engine_idle_ms = 0;
        assert_eq!(evict_idle(&rt), 0, "idle=0 must never evict");
    }
}

/// P2 dispatch surface: which backend a `--meta-store` value selects, and what
/// happens when it selects one this binary cannot provide. The point of these
/// tests is that the ADDITION of a third arm changed nothing for the two
/// existing backends — every uri that used to mean SQLite still means SQLite,
/// and the only new outcome is an explicit refusal. A silent default change
/// here would relocate live metadata without saying so.
#[cfg(test)]
mod store_dispatch_tests {
    use super::*;

    /// The refusal must arrive as a plain "bad configuration" error, never as
    /// something an operator has to reverse-engineer: every consumer of these
    /// messages (the CLI prints `e.kind()`-derived advice, the tests grep for
    /// `InvalidInput`) depends on the kind being stable.
    fn refused(s: MetaServer, expect: &str) -> String {
        let err = s
            .serve()
            .expect_err(&format!("{expect}: serve() must refuse and return"));
        assert_eq!(
            err.kind(),
            io::ErrorKind::InvalidInput,
            "{expect} must be InvalidInput, not a connect/open failure"
        );
        err.to_string()
    }

    fn server() -> MetaServer {
        let dir = tempfile::tempdir().expect("tempdir");
        MetaServer::new("127.0.0.1:0", dir.path())
    }

    #[test]
    fn store_dispatch_selects_sqlite_or_pg_and_refuses_other_schemes() {
        // The shapes that must land on SQLite, and none of them may raise a
        // refusal.
        for uri in [
            None,
            Some("sqlite".to_string()),
            Some("file.db".to_string()),
        ] {
            let s = server().with_store(uri.clone());
            assert!(
                matches!(s.store, StoreKind::Sqlite),
                "uri {uri:?} must still select SQLite, got {:?}",
                s.store
            );
            assert_eq!(s.store.backend_name(), "sqlite");
            assert!(
                s.store_unavailable.is_none(),
                "a SQLite uri must not arm the refusal path (would fail serve()): {uri:?}"
            );
        }
        // PG keeps owning both spellings of its scheme.
        for uri in [
            "postgres://u@h:5432/db",
            "postgresql://u@h:5432/db",
            "postgres://u:pw@h/db",
        ] {
            let s = server().with_store(Some(uri.to_string()));
            assert!(
                matches!(&s.store, StoreKind::Pg(u) if u == uri),
                "{uri} must select PG verbatim, got {:?}",
                s.store
            );
            assert_eq!(s.store.backend_name(), "postgres");
            assert!(s.store_unavailable.is_none());
        }
        // Any other scheme is refused rather than quietly served by SQLite.
        for uri in ["other://host/db", "mysql://user@host/db"] {
            let s = server().with_store(Some(uri.to_string()));
            assert!(
                s.store_unavailable.is_some(),
                "{uri} must arm the refusal path, got {:?}",
                s.store_unavailable
            );
            let err = refused(s.with_ha(Role::Primary, None), "unsupported scheme");
            assert!(err.contains("not a --meta-store scheme"), "{err}");
        }
    }

    #[test]
    fn ha_refusal_text_is_per_backend() {
        // One refusal site: the message has to name the remedy of the backend
        // actually in use.
        let base = tempfile::tempdir().expect("tempdir");
        let peer = || Some("127.0.0.1:1".to_string());
        let pg = refused(
            MetaServer::new("127.0.0.1:0", base.path())
                .with_store(Some("postgres://u@h/db".to_string()))
                .with_ha(Role::Primary, peer()),
            "PG + --peer",
        );
        assert!(pg.contains("postgres://…"), "{pg}");
        assert!(pg.contains("streaming"), "{pg}");
    }

    #[test]
    fn banner_names_backend_without_printing_the_uri() {
        // The banner is stdout → the journal. A PG URI routinely carries the
        // password, so the layout line must describe the backend without
        // echoing the credential-bearing string.
        let s = server().with_store(Some(
            "postgres://grovefs:s3cr3t@db.internal/grovefs".to_string(),
        ));
        let layout = s.store.backend_layout();
        assert!(layout.contains("PostgreSQL"), "{layout}");
        assert!(
            !layout.contains("s3cr3t"),
            "banner must not leak the password"
        );
        assert!(!layout.contains("grovefs:s3cr3t"), "{layout}");
    }
}

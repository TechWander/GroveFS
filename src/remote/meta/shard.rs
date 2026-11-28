//! Multi-meta-server **view sharding**: the ring, its placement table in the
//! shared control plane, and the one routing verdict every `M_OPEN` pays.
//!
//! Why this exists as a separate axis from the two shufflers already in the
//! codebase: `--meta-store a,b,c` spreads *view trees across PostgreSQL
//! databases inside one meta-server process*, and `--peer`/`--witness` runs *one
//! meta-server as a primary/standby pair*. Neither of those moves the *process*.
//! A deployment whose bottleneck is meta workers, engine pools or one host's
//! disk does not get relief from a third PG schema; it needs the trees to be
//! served by several `meta-server` processes. That is what a ring is: the set of
//! processes, and — per view — which one of them is authoritative.
//!
//! The rule the whole module defends: **a view has exactly one serving
//! instance, and the placement row in the shared control plane is the only
//! answer to which**. Mounts reach it by redirect (`E_SHARD` at `M_OPEN`, never
//! a per-op proxy), so the entry point is a bootstrap address and not a
//! bottleneck; management verbs reach it because the operator points the CLI at
//! it, and a verb that would mutate a tree this process does not serve is
//! refused rather than performed twice.
//!
//! Two more rules the first pass of this module did not hold, and now must
//! (ADR-0006 §2, §5.6):
//!
//! * **The sharding key is the physical base view name.** A lens, a bias or a
//!   fork is a presentation over ONE base tree, so it travels with that tree —
//!   routing a derived name would put two rows behind one tree and let them
//!   name different instances, which is the exact breakage §2 calls out.
//!   `M_OPEN` therefore folds the post-alias name through its definition before
//!   asking anything (see [`super::server_ext::shard_routing_key`]).
//! * **A routing READ writes nothing.** An `M_OPEN` that is going to be refused
//!   — unknown name, malformed name, a base that was never created — must not
//!   leave a placement row behind: the row is what an anonymous client would
//!   grow for free, and once written it permanently mis-routes the very name it
//!   failed to serve. `peek` is the read, `place_if_absent` is the claim, and
//!   only a mount that has passed every refusal gate pays for the claim — and it
//!   pays BEFORE `view_provider` materializes the tree, which is the one property
//!   the historical pre-gate pin had that mattered.
//!
//! There is deliberately **no placement cache**. `resolve_view_home` for PG
//! instances may cache nothing either, and for good reason: a move that flips a
//! row while a process still routes the view from a stale table sends every
//! later op to a backend it no longer owns (found on the `view move` path
//! exactly once, and never again). Re-reading one PRIMARY KEY row per `M_OPEN`
//! costs a mount establishment, which is the rarest thing on this path.

use super::*;

/// Bound on every control-plane round trip this module makes. A sharded
/// deployment's control plane is PostgreSQL; if it stops answering, the honest
/// outcome is a refused mount with a named cause — never a meta worker parked
/// in `connect()` holding a socket open (the ADR-0005 budget rule, applied to
/// the shard path rather than to the HA path).
const SHARD_DB_TIMEOUT: Duration = Duration::from_secs(2);

/// Warm control-plane sessions this process keeps for placement reads and
/// claims. Two, not four ([`crate::control::CONTROL_POOL_SIZE`]): the shard path
/// is a one-row lookup per mount establishment, and its connection count is what
/// a shared PostgreSQL `max_connections` actually measures. See [`Shard::checkout`]
/// for the measurement that set this number.
const SHARD_POOL_MAX: usize = 2;

// MEDIUM — the control plane's pools in one `meta-server` process (resolved
// 2026-11-28, partly by merging and partly by NOT merging). Measured here on a
// sharded instance with no mount, counting this database's `pg_stat_activity`
// rows: THREE warm sessions, one per control-plane store (definitions, keys,
// web-auth) opened against the SAME URI, because `control::PgPool::open` did
// its own eager connect. Those three shapes are identical — plain session, no
// per-session `SET`, idempotent `IF NOT EXISTS` DDL under the same advisory
// lock — so they now take one `Arc<PgPool>` from `crate::control::shared`,
// keyed on the EXPANDED URI: idle warm sessions 3 → 1, and the post-burst
// retention ceiling 3 × CONTROL_POOL_SIZE (12) → 1 × 4.
//
// This pool is deliberately NOT in that registry. Its sessions are shaped at
// connect with `statement_timeout` and `idle_in_transaction_session_timeout` of
// [`SHARD_DB_TIMEOUT`] so a wedged control plane cannot park a meta worker
// (ADR-0005, applied to the shard path). Merging it in would either put that 2 s
// ceiling under credential checks, audit inserts and DDL, or need a `RESET` per
// use — more round trips than the sharing saves. Two shapes, one contract each:
// the four-way `max_connections` footprint of a PG meta-server is now
// 1 (control plane) + at most SHARD_POOL_MAX (only once a mount actually arrives
// — before it, this pool is empty and holds nothing, which is what the same
// measurement showed).

/// Why a control-plane dial really failed, in one line.
///
/// `postgres::Error`'s own `Display` is a CATEGORY ("db error", "error during
/// connection setup"); the sentence an operator needs — `FATAL: 53300: too many
/// connections for role "grove"` — sits one level down in the source chain. A
/// routing refusal that quotes only the category sends the reader to the server
/// log for what is a capacity answer, and it makes a connection-pool exhaustion
/// case impossible to assert on (`tests/shard_routing.rs`, the ceiling TODO).
/// Flattened because these strings reach a client as a single-line reply
/// (`server_ext::msg_shape_tests`).
fn db_cause(e: &(dyn std::error::Error + 'static)) -> String {
    let mut out = String::new();
    let mut cur: Option<&dyn std::error::Error> = Some(e);
    while let Some(c) = cur {
        let t = c.to_string();
        if !t.is_empty() && !out.contains(&t) {
            if !out.is_empty() {
                out.push_str(": ");
            }
            out.push_str(&t);
        }
        cur = c.source();
    }
    out.replace(['\n', '\r'], " ")
}

/// The shard placement table, in the control schema. `meta` names a RING ENTRY
/// (an identifier from `--shard-ring`), never a URI: like
/// `grovefs_control.view_placement`, a row must not carry a credential, and the
/// ring is the operator's file to edit.
///
/// `store` records the tree backend the home instance had when it wrote the row,
/// credential-free. It exists so an instance named as a home can notice that
/// its own `--meta-store` is not the one that created the tree and say so,
/// instead of opening an empty schema beside the real one and serving a
/// silently-empty view — the one outcome here that is worse than a refusal.
const META_PLACEMENT_TABLE: &str = "grovefs_control.meta_placement";

/// Idempotent DDL, serialized by the SAME advisory lock class the control
/// schema's own DDL and `view_placement` use ([`crate::control::DDL_LOCK_CLASS`]):
/// several instances creating their first view concurrently line up on one lock
/// rather than racing the catalog.
const META_PLACEMENT_DDL: &str = "CREATE TABLE IF NOT EXISTS grovefs_control.meta_placement (
    view       TEXT PRIMARY KEY,
    meta       TEXT NOT NULL,
    store      TEXT NOT NULL,
    updated_at TEXT NOT NULL
)";

/// One meta-server in the ring: what it is called in the placement table, and
/// the address(es) a CLIENT dials for it.
///
/// `addr` is `host:port`, or `host:port+host:port` for a ring member that is an
/// HA PAIR: the entry carries primary first, standby second, and an `E_SHARD`
/// redirect ships the whole list, so a sharded deployment and `--peer`
/// replication compose — the failover logic a mount already has does the rest.
/// The control port is not part of the ring: a redirect a client must act on
/// needs a mount address, and governance belongs to the port the operator
/// already chose to point `grovefs view --control-server` at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MetaNode {
    pub name: String,
    pub addr: String,
}

impl MetaNode {
    /// The dialable addresses of this ring member, in the client's preferred
    /// order (a single entry for a plain node, one per HA role for `a+b`).
    pub fn addrs(&self) -> Vec<&str> {
        self.addr
            .split('+')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// Whether `addr` is one of this member's addresses — the match
    /// `--shard-id`/`--advertise-addr` uses to find this process in the ring.
    fn carries(&self, addr: &str) -> bool {
        self.addrs().contains(&addr)
    }
}

/// Where a view's tree is served, as one `M_OPEN` needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Home {
    /// This process serves it: proceed with the handshake.
    Mine,
    /// Another ring member serves it; the client should dial `addr`.
    Other(MetaNode),
}

/// One placement row, as read from (or written to) the control plane. `meta` is
/// the ring NODE NAME it points at and `store` the tree backend fingerprint that
/// node had when it wrote the row — the two facts [`Shard::decide`] needs, and
/// nothing else, so a row can be read by an instance that disagrees with it.
#[derive(Clone, Debug)]
struct Placement {
    meta: String,
    store: String,
}

/// How often a sharded instance re-reads the placement table to notice it has
/// been deposed, when nobody configured it. Bounded by the same argument as
/// every other control-plane job: a move is a human action, so seconds are
/// fine, and one PK-less table read every few seconds is nothing next to the
/// mount traffic the same table serves. `--shard-sweep-secs` overrides it.
pub(crate) const DEFAULT_SHARD_SWEEP_MS: u64 = 5_000;

/// The sharding configuration of one `meta-server`: the ring, this process's
/// place in it, where the placement table lives, which tree backend this
/// process serves, and how hard it re-reads the placement table. Built once at
/// `serve()` and shared immutably; `None` in
/// [`MetaRuntime`] means sharding is off, which is the historical single-process
/// deployment and must cost zero queries.
#[derive(Clone)]
pub struct Shard {
    nodes: Vec<MetaNode>,
    me: usize,
    /// The control-plane PostgreSQL URI the placement table lives in.
    ctl: String,
    /// This process's tree backend, credential-free ([`store_fingerprint`]).
    store: String,
    /// The `--shard-ring` text, quoted verbatim in error messages.
    origin: String,
    /// `--shard-sweep-secs`, in milliseconds: how often this instance re-reads
    /// the placement table to notice a `view move` took a tree away. It lives
    /// on the ring and not on the server because the sweep only exists when a
    /// ring does — the rewritten ROW is the message and the ticker is the
    /// delivery, so this interval is the one window an operator can set between
    /// moving a view and the deposed instance stopping its writes. `0` is
    /// refused at start-up: "never sweep" would mean "keep writing to a tree the
    /// placement table says belongs to somebody else".
    sweep_ms: u64,
    /// Warm control-plane sessions ([`Shard::checkout`] / [`Shard::requeue`]).
    /// Behind an `Arc` because a `Shard` is cloned into every holder of the
    /// routing answer; the pool is what makes one, not one per clone.
    pool: Arc<parking_lot::Mutex<Vec<postgres::Client>>>,
}

/// `Debug` by hand: a live `postgres::Client` has nothing an operator should
/// read (and no `Debug` of its own), while the four facts that describe a
/// routing decision — ring, self, backend, where the table lives — are exactly
/// what a `--dry-run`-style diagnosis wants.
impl std::fmt::Debug for Shard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shard")
            .field("nodes", &self.nodes)
            .field("me", &self.me)
            .field("ctl", &redact_authority(&self.ctl))
            .field("store", &self.store)
            .field("origin", &self.origin)
            .field("sweep_ms", &self.sweep_ms)
            .field("pooled", &self.pool.lock().len())
            .finish()
    }
}

/// The name shape a ring entry and a placement row share: a plain identifier, no
/// quoting, so a row written by one instance can never name a different node by
/// smuggled punctuation. Same rule as the PG instance registry.
fn valid_node_name(name: &str) -> bool {
    (1..=24).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Parse `--shard-ring` into its nodes, in order.
///
/// Two entry shapes, because both deployments want one: `m1=10.0.0.1:50053` for
/// a ring that will grow (placement rows reference the NAME, so reordering the
/// list or changing a port moves nothing), and a bare `10.0.0.1:50053` for a
/// throwaway test ring, auto-named `m1..mN` by position — names that are stable
/// only while the order is, exactly the trade the PG registry documents.
///
/// Every address must be client-dialable ([`crate::manager::check_dialable`]):
/// this list is what an `E_SHARD` redirect tells a mount to connect to, so a
/// `0.0.0.0` entry would produce a refusal the client cannot act on.
pub fn parse_ring(spec: &str) -> Result<Vec<MetaNode>, String> {
    let mut nodes: Vec<MetaNode> = Vec::new();
    for (i, raw) in spec
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .enumerate()
    {
        let (name, addr) = match raw.split_once('=') {
            Some((n, a)) => (n.trim().to_string(), a.trim().to_string()),
            None => (format!("m{}", i + 1), raw.to_string()),
        };
        if !valid_node_name(&name) {
            return Err(format!(
                "ring entry '{raw}': node name '{name}' must be 1-24 chars of [A-Za-z0-9_-]"
            ));
        }
        if nodes.iter().any(|n| n.name == name) {
            return Err(format!("duplicate ring node name '{name}'"));
        }
        let mut dialed: Vec<String> = Vec::new();
        for part in addr.split('+') {
            let a = crate::manager::check_dialable(part)
                .map_err(|e| format!("ring entry '{raw}': {e}"))?;
            if dialed.iter().any(|d| d == &a) {
                return Err(format!("ring entry '{raw}': the same address twice"));
            }
            dialed.push(a);
        }
        nodes.push(MetaNode {
            name,
            addr: dialed.join("+"),
        });
    }
    if nodes.is_empty() {
        return Err("--shard-ring names no node (expected `m1=host:port,m2=host:port`)".into());
    }
    Ok(nodes)
}

impl Shard {
    /// Assemble the ring, decide which entry is this process, and record what it
    /// would have to say about itself.
    ///
    /// `id` is `--shard-id`: a node NAME, a positional INDEX, or an ADDRESS.
    /// Absent, `advertise` (`--advertise-addr`) picks the entry that matches it —
    /// a deployment that already names its own dialable address for the manager
    /// should not have to name it twice. Neither matching is fuzzy: an
    /// ambiguous or absent match fails the start, because a process that has to
    /// guess whether it owns `alice` will get that wrong eventually.
    pub fn new(
        spec: &str,
        id: Option<&str>,
        advertise: Option<&str>,
        ctl: String,
        store: String,
    ) -> Result<Self, String> {
        let nodes = parse_ring(spec)?;
        let origin = spec.trim().to_string();
        let me = match id.map(str::trim).filter(|s| !s.is_empty()) {
            Some(want) => nodes
                .iter()
                .position(|n| n.name.eq_ignore_ascii_case(want) || n.carries(want))
                .or_else(|| {
                    // A positional INDEX is the other spelling a small ring gets
                    // (`--shard-id 0`). A name match wins above, because `1` is a
                    // legal node NAME and an operator who named their nodes
                    // means the name.
                    want.parse::<usize>().ok().and_then(|k| {
                        if k < nodes.len() {
                            Some(k)
                        } else {
                            k.checked_sub(1).filter(|k| *k < nodes.len())
                        }
                    })
                }),
            None => advertise
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .and_then(|a| nodes.iter().position(|n| n.carries(a))),
        };
        let me = me.ok_or_else(|| {
            let how = if id.is_some() {
                format!("--shard-id '{}'", id.unwrap_or_default().trim())
            } else {
                "--shard-id (or --advertise-addr)".to_string()
            };
            format!(
                "{how} is not this ring ({origin}) — nodes are {} \
                 (name them, or fix the address)",
                nodes
                    .iter()
                    .map(|n| format!("'{}' at {}", n.name, n.addr))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
        Ok(Self {
            nodes,
            me,
            ctl,
            store,
            origin,
            sweep_ms: DEFAULT_SHARD_SWEEP_MS,
            pool: Arc::new(parking_lot::Mutex::new(Vec::new())),
        })
    }

    /// The placement re-read interval, in milliseconds.
    pub fn sweep_ms(&self) -> u64 {
        self.sweep_ms
    }

    /// Override the sweep interval ([`DEFAULT_SHARD_SWEEP_MS`] is the default).
    /// `0` is carried here as written and refused where the ring is built, so
    /// an embedder gets the same start-up error the CLI does.
    pub fn with_sweep_ms(mut self, ms: u64) -> Self {
        self.sweep_ms = ms;
        self
    }

    /// This process's own ring entry.
    pub fn me(&self) -> &MetaNode {
        &self.nodes[self.me]
    }

    /// Every node, in ring order (the banner, `store.info`).
    pub fn nodes(&self) -> &[MetaNode] {
        &self.nodes
    }

    /// The `--shard-ring` text, for error messages.
    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// The tree backend this process declared, credential-free.
    pub fn store_fingerprint(&self) -> &str {
        &self.store
    }

    /// Where the placement table lives.
    pub fn control_uri(&self) -> &str {
        &self.ctl
    }

    /// A one-node ring routes to itself without asking anything: the same
    /// "the overwhelmingly common deployment pays zero extra queries" rule
    /// [`crate::pg_store::resolve_view_home`] applies to a single-instance PG
    /// spec. `--shard-ring m1=x` therefore behaves exactly as no flag at all,
    /// which is what makes a ring written once and grown later safe to deploy.
    pub fn is_solo(&self) -> bool {
        self.nodes.len() <= 1
    }

    /// Which instance serves `view`, consulting the placement table.
    ///
    /// This is the GOVERNANCE read: `view create` and `view rm_base` use it, and
    /// for them the bootstrap arm below is the point — creating a tree claims
    /// its name in front of every other instance, before the tree exists, so a
    /// concurrent creator and a peer both see the same answer.
    ///
    /// Order, and why each arm is where it is:
    ///
    /// 0. a solo ring is the identity (no query at all);
    /// 1. a placement row decides it, including a row that names a node the ring
    ///    no longer defines — that is REFUSED, not hashed over: a view whose
    ///    tree lives on a retired host has to be moved or removed by a person,
    ///    and a mount that silently got a different answer would be data loss
    ///    wearing a success;
    /// 2. no row: pin THIS instance. That is the bootstrap arm for a deployment
    ///    that turned sharding on with views already on disk (or for a view
    ///    created by a `grovefs view --meta-dir` invocation that never saw the
    ///    ring). The instance that first serves a view keeps it — a ring growing
    ///    by one node never moves an existing view, and no tree ever has to be
    ///    copied for this flag to be turned on.
    ///
    /// The pin is one locked transaction (`INSERT … ON CONFLICT DO NOTHING`
    /// then read back), so two instances that see the same unplaced view at the
    /// same instant both end up with the SAME answer, and one of them redirects.
    pub fn route(&self, view: &str) -> Result<Home, String> {
        if self.is_solo() {
            return Ok(Home::Mine);
        }
        match self.read_placement(view)? {
            Some(row) => self.decide(view, Some(row)),
            None => self.decide(view, Some(self.pin(view)?)),
        }
    }

    /// Routing verdict WITHOUT the bootstrap arm of [`Shard::route`]: read the row,
    /// apply the same rule. `M_OPEN` and the per-op gate use THIS, because a data
    /// path must not grow placement rows on a name it is not going to serve —
    /// an unplaced view is simply this instance's until the mount gets past every
    /// refusal gate (the claim is [`Shard::place_if_absent`], which `M_OPEN` makes
    /// after them and before the tree is materialized), and a mount that is
    /// refused — unknown name, malformed name, missing base tree, an anonymous
    /// socket probe — must leave no trace in the shared table. Governance verbs
    /// that intend to TAKE a name use [`Shard::route`]; one that intends to give
    /// it back for good (`view rm`) uses [`Shard::peek_forget`], because a verb
    /// that can be refused before it touches a tree must not claim it on the way
    /// out.
    pub fn peek(&self, view: &str) -> Result<Home, String> {
        if self.is_solo() {
            return Ok(Home::Mine);
        }
        let row = self.read_placement(view)?;
        self.decide(view, row)
    }

    /// The routing verdict for a verb that is about to DESTROY the view: the
    /// read-only rule of [`Shard::peek`], plus one answer a data path must never
    /// give — **no row means this instance**, because a name with no row and no
    /// tree is exactly what a legacy (pre-sharding, or `--meta-dir`) deployment
    /// looks like from here, and the delete has to be allowed to clean it up.
    ///
    /// What this deliberately does NOT have is [`Shard::route`]'s bootstrap
    /// pin. That arm exists to claim a view a process is about to SERVE, and a
    /// destroy that cannot honour the row is the one verb that must not claim
    /// anything: with `route()`, a view whose tree this instance had already
    /// deleted (row gone with it) looked like an unplaced name, so the very next
    /// `view rm` at a peer pinned the row to ITSELF and then failed on "no
    /// physical base tree" — leaving a placement row that lied, and then
    /// refusing every future `view create` of that name with "placed on
    /// meta-server X" pointing at a host with nothing behind it (found by
    /// `tests/shard_routing.rs::a_three_node_ring_routes_by_the_row_not_by_adjacency`,
    /// 2026-11-28: it took one retired-node sequence on a three-instance ring).
    /// ADR-0006 §2 makes the row the single answer to "which instance serves
    /// this name", so a verb that cannot serve it must not write it.
    pub fn peek_forget(&self, view: &str) -> Result<Home, String> {
        if self.is_solo() {
            return Ok(Home::Mine);
        }
        let row = self.read_placement(view)?;
        if row.is_none() {
            return Ok(Home::Mine);
        }
        self.decide(view, row)
    }

    /// Claim `view` for THIS instance unless a row already exists — the write an
    /// `M_OPEN` makes once every refusal gate has passed and BEFORE it
    /// materializes a tree, never before. Idempotent (`ON CONFLICT DO NOTHING`),
    /// so two instances that race an unplaced name settle on whichever wrote
    /// first and every later mount is redirected; this is the one thing the
    /// historical pre-gate `route()` pin was actually for, and keeping it here
    /// (after the gates) is what stops a refused mount from claiming a name it
    /// cannot serve. Best-effort like [`Shard::forget`]: a control plane that
    /// cannot answer costs the bootstrap row for this mount, not the mount itself
    /// — the tree exists and is served, and the row re-appears on the next
    /// successful `M_OPEN`.
    pub fn place_if_absent(&self, view: &str) -> Result<(), String> {
        if self.is_solo() {
            return Ok(());
        }
        self.write_placement(view, &self.me().name, &self.store, false)
    }

    /// The verdict for one placement row, as a pure function: the routing RULE
    /// (name match, backend match, dangling row) is testable without a database.
    fn decide(&self, view: &str, row: Option<Placement>) -> Result<Home, String> {
        let row = match row {
            Some(r) => r,
            // `route` always pins first; a bare call with no row means this
            // process, which is the same answer the pin would have produced.
            None => return Ok(Home::Mine),
        };
        if row.meta == self.me().name {
            if !self.store.is_empty() && !row.store.is_empty() && row.store != self.store {
                return Err(format!(
                    "view '{view}' is placed on this instance ('{}'), but its tree was created \
                     on backend {} and this process serves {} — fix --meta-store here, or move \
                     the view to the instance that has that backend",
                    self.me().name,
                    row.store,
                    self.store
                ));
            }
            return Ok(Home::Mine);
        }
        match self.nodes.iter().find(|n| n.name == row.meta) {
            None => Err(format!(
                "view is placed on meta-server '{}', which --shard-ring {} does not define \
                 (ring: {}) — add the node back or re-place the view with `view move --to-meta`",
                row.meta,
                self.origin,
                self.nodes
                    .iter()
                    .map(|n| n.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
            Some(node) => Ok(Home::Other(node.clone())),
        }
    }

    /// Record that THIS instance has taken `view` over from a peer
    /// (`view move --to-meta`, run against the target's control port). The row
    /// is rewritten, and its `store` becomes this backend — which is what makes
    /// the next `decide` on any node describe the same truth the probe did.
    pub fn adopt(&self, view: &str) -> Result<(), String> {
        self.write_placement(view, &self.me().name, &self.store, true)
    }

    /// Drop a view's placement row. Best-effort like
    /// [`crate::pg_store::forget_view_placement`]: the tree is gone by the time
    /// this runs, so a control plane that cannot answer skews future counts and
    /// leaves one stale row — it never revives the view or loses data.
    pub fn forget(&self, view: &str) {
        let _ = self.with_client(|c| {
            c.execute(
                &format!("DELETE FROM {META_PLACEMENT_TABLE} WHERE view = $1"),
                &[&view],
            )
            .map(|_| ())
            .map_err(|e| format!("postgres delete {META_PLACEMENT_TABLE}: {e}"))
        });
    }

    /// Every placement row, as view → node name. This is the ROUTING read: the
    /// placement sweep (`evict_misplaced`) and the governance verbs decide on it.
    /// Read-only: it never CREATES the table, so an operator pointing a
    /// non-sharded deployment at the console does not grow an empty table — a
    /// missing one simply means nothing has been placed yet.
    pub fn placement_map(&self) -> Result<HashMap<String, String>, String> {
        self.with_client(|c| {
            if !placement_exists(c)? {
                return Ok(HashMap::new());
            }
            let rows = c
                .query(
                    &format!("SELECT view, meta FROM {META_PLACEMENT_TABLE} ORDER BY view"),
                    &[],
                )
                .map_err(|e| format!("postgres read {META_PLACEMENT_TABLE}: {e}"))?;
            Ok(rows
                .into_iter()
                .map(|r| (r.get::<_, String>(0), r.get::<_, String>(1)))
                .collect())
        })
    }

    /// Every row of the placement table verbatim: `(view, meta)` sorted by view.
    ///
    /// Read-only and non-creating like [`Shard::placement_map`], and kept as
    /// KEYS rather than folded into counts because it answers the question an
    /// operator actually asks when a ring looks wrong — "which names does the
    /// table hold, and does any of them name a tree that no longer exists?" A
    /// derived name in here is the ADR-0006 §2 breakage made visible; a name
    /// whose tree was deleted is an orphan no governance verb will remove.
    /// `tests/shard_routing.rs` asserts on it for exactly that reason — and
    /// sorts, because `placement_map` is a HashMap and a comparison whose element
    /// order came from a hash would be a flake, not a finding.
    pub fn placement_rows(&self) -> Result<Vec<(String, String)>, String> {
        let mut rows: Vec<(String, String)> = self.placement_map()?.into_iter().collect();
        rows.sort();
        Ok(rows)
    }

    /// Every row of the placement table with WHEN it was written:
    /// `(view, meta, updated_at)` verbatim, sorted by view.
    ///
    /// The third column is why this exists next to [`Shard::placement_rows`]: a
    /// count cannot say how stale its own answer is, and inside a sweep window
    /// the question an operator is asking is exactly "when did this row change,
    /// and is the instance still holding the tree past that moment". Read-only
    /// and non-creating like the other two, and only ever reached from
    /// `store.info` — the routing read stays on [`Shard::placement_map`].
    pub fn placement_audit(&self) -> Result<Vec<(String, String, String)>, String> {
        self.with_client(|c| {
            if !placement_exists(c)? {
                return Ok(Vec::new());
            }
            let rows = c
                .query(
                    &format!(
                        "SELECT view, meta, updated_at FROM {META_PLACEMENT_TABLE} ORDER BY view"
                    ),
                    &[],
                )
                .map_err(|e| format!("postgres read {META_PLACEMENT_TABLE}: {e}"))?;
            Ok(rows
                .into_iter()
                .map(|r| {
                    (
                        r.get::<_, String>(0),
                        r.get::<_, String>(1),
                        r.get::<_, String>(2),
                    )
                })
                .collect())
        })
    }

    /// A FRESH connection to the control plane, BOUNDED at both ends.
    /// `tokio-postgres` 0.7 exposes a connect timeout on `Config` and nothing for
    /// statements, so the statement budget is set on the session: without it, a
    /// wedged control plane would hand this process a parked meta worker per
    /// mount attempt (the rule ADR-0005 states for the HA path, applied here).
    /// [`Shard::checkout`] is what the callers below want; this is its cold path.
    fn connect(&self) -> Result<postgres::Client, String> {
        let mut cfg: postgres::Config = self
            .ctl
            .parse()
            .map_err(|e| format!("cannot parse the control-plane URI: {e}"))?;
        cfg.connect_timeout(SHARD_DB_TIMEOUT)
            .tcp_user_timeout(SHARD_DB_TIMEOUT * 2);
        let mut c = cfg.connect(postgres::NoTls).map_err(|e| {
            format!(
                "cannot reach the shard control plane for view placement ({}) — which \
                 instance serves the view is undecided, refusing to guess",
                db_cause(&e)
            )
        })?;
        let ms = SHARD_DB_TIMEOUT.as_millis().to_string();
        c.batch_execute(&format!(
            "SET statement_timeout = '{ms}'; SET idle_in_transaction_session_timeout = '{ms}'"
        ))
        .map_err(|e| {
            format!(
                "cannot bound the shard control-plane timeouts: {}",
                db_cause(&e)
            )
        })?;
        Ok(c)
    }

    /// A control-plane session, preferring one this process already opened.
    ///
    /// This matters more than it looks. The historical shape was one fresh
    /// `connect()` per routing read, which made this path's connection count
    /// proportional to MOUNT TRAFFIC rather than to concurrent mounts, and each
    /// of those sessions also took the control schema's global DDL advisory lock
    /// (the "ensure the table exists" step), so every instance serialized its
    /// mounts behind one lock while each waiter held a socket. Measured: the four
    /// `tests/shard_routing.rs` cases running in parallel (8 instances, one shared
    /// PostgreSQL, `max_connections = 100`) failed with `53300 remaining
    /// connection slots are reserved for non-replication superuser connections`,
    /// while any one of them alone passed. Warm sessions plus a lock-free read is
    /// the fix for both halves of that.
    fn checkout(&self) -> Result<postgres::Client, String> {
        loop {
            let warm = self.pool.lock().pop();
            match warm {
                None => break,
                // Reaped by the server (idle timeout, restart) or half-closed:
                // drop it, try the next, then connect cold.
                Some(c) if c.is_closed() => continue,
                Some(c) => return Ok(c),
            }
        }
        self.connect()
    }

    /// Hand a session back, bounded at [`SHARD_POOL_MAX`]: a burst above that is
    /// served and then shed (the client drops, closing the socket), so the steady
    /// state costs two connections and a peak costs the peak — never a count that
    /// grows with how many mounts this process has answered. A closed client is
    /// never requeued.
    fn requeue(&self, c: postgres::Client) {
        if c.is_closed() {
            return;
        }
        let mut p = self.pool.lock();
        if p.len() < SHARD_POOL_MAX {
            p.push(c);
        }
    }

    /// Run one use of a session and hand it back on success.
    ///
    /// Deliberately not a RAII guard: a call that fails returns through `?`, its
    /// client drops and the socket closes. That is the point — a statement that
    /// errored can leave a session inside an aborted transaction, and handing
    /// that to the next mount would turn one bad round trip into a sticky
    /// failure. Reconnecting is the honest reset, and the historical behaviour.
    fn with_client<T>(
        &self,
        f: impl FnOnce(&mut postgres::Client) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut c = self.checkout()?;
        let out = f(&mut c)?;
        self.requeue(c);
        Ok(out)
    }

    /// Read one row, and NOTHING else: the routing read has no side effect, not
    /// even a catalog one. An absent table is the same answer as an absent row —
    /// nothing has been placed yet, so this instance is home, and the claim that
    /// a successful mount makes ([`Shard::place_if_absent`]) creates the table.
    /// That keeps the contract [`crate::pg_store::lookup_placement`] states (a
    /// routing read must not depend on which process initialized the control
    /// plane) without paying a DDL lock per read; the fresh-ring case stays the
    /// normal answer it always was rather than a refused mount.
    fn read_placement(&self, view: &str) -> Result<Option<Placement>, String> {
        self.with_client(|c| {
            if !placement_exists(c)? {
                return Ok(None);
            }
            Ok(c
                .query(
                    &format!("SELECT meta, store FROM {META_PLACEMENT_TABLE} WHERE view = $1"),
                    &[&view],
                )
                .map_err(|e| format!("postgres read {META_PLACEMENT_TABLE}: {e}"))?
                .into_iter()
                .next()
                .map(|r| Placement {
                    meta: r.get(0),
                    store: r.get(1),
                }))
        })
    }

    /// Insert `view`'s row unless one exists, then read back whatever won. One
    /// transaction, one advisory lock: two instances pinning the same new view
    /// serialize, and the loser learns the winner's name from the same round
    /// trip rather than guessing that its own insert took effect.
    fn pin(&self, view: &str) -> Result<Placement, String> {
        self.with_client(|c| {
            create_placement(c)?;
            let mut tx = c
                .transaction()
                .map_err(|e| format!("postgres begin: {e}"))?;
            let now = chrono::Local::now().to_rfc3339();
            tx.execute(
                &format!(
                    "INSERT INTO {META_PLACEMENT_TABLE} (view, meta, store, updated_at) \
                     VALUES ($1, $2, $3, $4) ON CONFLICT (view) DO NOTHING"
                ),
                &[&view, &self.me().name, &self.store, &now],
            )
            .map_err(|e| format!("postgres pin {META_PLACEMENT_TABLE}: {e}"))?;
            let row = tx
                .query(
                    &format!("SELECT meta, store FROM {META_PLACEMENT_TABLE} WHERE view = $1"),
                    &[&view],
                )
                .map_err(|e| format!("postgres read {META_PLACEMENT_TABLE}: {e}"))?
                .into_iter()
                .next()
                .map(|r| Placement {
                    meta: r.get(0),
                    store: r.get(1),
                });
            tx.commit()
                .map_err(|e| format!("postgres commit (shard pin): {e}"))?;
            row.ok_or_else(|| format!("view placement for '{view}' vanished mid-transaction"))
        })
    }

    fn write_placement(
        &self,
        view: &str,
        meta: &str,
        store: &str,
        upsert: bool,
    ) -> Result<(), String> {
        self.with_client(|c| {
            create_placement(c)?;
            let mut tx = c
                .transaction()
                .map_err(|e| format!("postgres begin: {e}"))?;
            let now = chrono::Local::now().to_rfc3339();
            let conflict = if upsert {
                "DO UPDATE SET meta = EXCLUDED.meta, store = EXCLUDED.store, updated_at = EXCLUDED.updated_at"
            } else {
                "DO NOTHING"
            };
            tx.execute(
                &format!(
                    "INSERT INTO {META_PLACEMENT_TABLE} (view, meta, store, updated_at) \
                     VALUES ($1, $2, $3, $4) ON CONFLICT (view) {conflict}"
                ),
                &[&view, &meta, &store, &now],
            )
            .map_err(|e| format!("postgres write {META_PLACEMENT_TABLE}: {e}"))?;
            tx.commit()
                .map_err(|e| format!("postgres commit (shard placement): {e}"))
        })
    }
}

/// Whether the placement table exists yet — a read-only catalog probe, no DDL
/// and no lock, which is what makes it affordable on every routing read.
fn placement_exists(c: &mut postgres::Client) -> Result<bool, String> {
    Ok(c
        .query(
            // Cast to bool: the wire type `regclass` has no decoder here, so
            // reading the column as a name fails with "error deserializing
            // column 0" (found by `tests/shard_routing.rs`).
            "SELECT (to_regclass('grovefs_control.meta_placement') IS NOT NULL)",
            &[],
        )
        .map_err(|e| format!("postgres probe {META_PLACEMENT_TABLE}: {e}"))?
        .first()
        .map(|r| r.get::<_, bool>(0))
        .unwrap_or(false))
}

/// Create the table if absent, under the control schema's own DDL lock; if it is
/// already there, take no lock at all. Every WRITER calls it, so an engine open
/// never depends on which process happened to initialize the control plane
/// first, and the probe in front of it is what keeps the advisory lock — and
/// `CREATE TABLE IF NOT EXISTS`, which takes an ACCESS EXCLUSIVE lock on an
/// existing relation — off the path a mount takes. The schema is created too: a
/// ring pointed at a PostgreSQL that has never held a grovefs control plane has
/// no `grovefs_control` to put the table in.
fn create_placement(c: &mut postgres::Client) -> Result<(), String> {
    if placement_exists(c)? {
        return Ok(());
    }
    let lock = format!("SELECT pg_advisory_lock({})", crate::control::DDL_LOCK_CLASS);
    c.batch_execute(&lock)
        .map_err(|e| format!("postgres advisory lock: {e}"))?;
    let body = format!(
        "CREATE SCHEMA IF NOT EXISTS {};{META_PLACEMENT_DDL};",
        crate::control::CONTROL_SCHEMA
    );
    let created = c.batch_execute(&body);
    // Always release, even when the DDL failed, so a later process is not left
    // behind a stuck session lock (the rule `control::PgPool::init_schema` holds).
    let unlock = format!("SELECT pg_advisory_unlock({})", crate::control::DDL_LOCK_CLASS);
    let _ = c.batch_execute(&unlock);
    created.map_err(|e| format!("postgres ensure {META_PLACEMENT_TABLE}: {e}"))?;
    Ok(())
}

/// The credential-free identity of a tree backend, for the placement row.
///
/// Deliberately NOT the URI as configured: `--meta-store` embeds a password and
/// a placement row is readable by every instance in the deployment (and shows up
/// in `store.info`), so what belongs in the row is the part that identifies the
/// databases — scheme, host, port, path — and nothing else. A `file:` registry
/// contributes its PATH, which is the operator-owned name of the same book.
// TODO(severity=Medium): that last sentence holds only for a registry the
// operator NAMED. `MetaServer::serve()` ADOPTS a bare `--meta-store
// postgres://…` into `<base>/instances.conf`, so for that documented, common
// spelling the fingerprint carries this process's DATA DIRECTORY: two instances
// pointed at one database name themselves differently, and one instance started
// with a different `--meta-dir` renames itself. Measured 2026-11-28 by
// `tests/shard_routing.rs::a_shorter_sweep_shortens_the_window_a_deposed_instance_holds_the_tree`,
// where the two nodes report `postgres registry /tmp/grovefs_shard_<a>/instances.conf`
// and `…/<b>/instances.conf` for the identical URI (the race that case ends with
// asserts the pair stays one process's).
// What that costs: after a node's base directory moves, every placement row it
// wrote names a backend it no longer serves, so `Shard::decide`'s backend branch
// refuses its OWN views at `M_OPEN` — and `view move --to-meta` back onto that
// same node early-returns "already placed … nothing to move" without re-stamping
// `store`, so the repairs left are moving the view to a peer and back, or a
// hand-written `UPDATE` (both refusal and repair are walked end-to-end in
// `a_three_node_ring_routes_by_the_row_not_by_adjacency`).
// The fix is a decision, not a patch: either fingerprint the DATABASE (the
// registry's control instance, which the ring already resolves once for the
// placement table's own URI), or let the instance that a row names re-stamp that
// row when its backend really does hold the tree — the second option has to keep
// an actual `--meta-store` switch detectable, since that switch is exactly what
// the branch exists to refuse.
pub fn store_fingerprint(store: &StoreKind) -> String {
    match store {
        // Local disk: no two instances share it, so the fingerprint says what it
        // means and the reachability probe on `view move --to-meta` is what
        // refuses a move a SQLite tree cannot survive.
        StoreKind::Sqlite => "sqlite (local)".to_string(),
        StoreKind::Pg(spec) => match spec.strip_prefix("file:") {
            Some(path) => format!("postgres registry {path}"),
            None => spec
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(redact_authority)
                .collect::<Vec<_>>()
                .join(","),
        },
        #[cfg(feature = "treedb")]
        StoreKind::Treedb(addrs) => format!("treedb://{}", addrs.join(",")),
    }
}

/// `postgres://u:p@h:5432/db?x=1` → `postgres://h:5432/db`: the userinfo and the
/// query string go (parameters can carry a password file's path), the host, port
/// and database stay. A URI with no `@` is already credential-free and returns
/// with only its query dropped.
fn redact_authority(uri: &str) -> String {
    let Some(end) = uri.find("://") else {
        return uri.split('?').next().unwrap_or(uri).to_string();
    };
    let (scheme, rest) = uri.split_at(end);
    let rest = &rest[3..];
    let auth_end = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(auth_end);
    let host = match authority.rfind('@') {
        Some(at) => &authority[at + 1..],
        None => authority,
    };
    let path = tail.split('?').next().unwrap_or("");
    format!("{}://{}{}", scheme, host, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring(spec: &str, id: &str, store: &str) -> Shard {
        Shard::new(
            spec,
            Some(id),
            None,
            "postgres://gfs@127.0.0.1:5432/gfs".into(),
            store.into(),
        )
        .unwrap()
    }

    /// A named ring keeps the names an operator wrote; a bare list is auto-named
    /// by position, and both accept whitespace.
    #[test]
    fn ring_parses_named_and_positional_entries() {
        let named = parse_ring("a=10.0.0.1:50053, b=10.0.0.2:50053").unwrap();
        assert_eq!(
            named,
            vec![
                MetaNode {
                    name: "a".into(),
                    addr: "10.0.0.1:50053".into()
                },
                MetaNode {
                    name: "b".into(),
                    addr: "10.0.0.2:50053".into()
                },
            ]
        );
        let auto = parse_ring("10.0.0.1:1,10.0.0.2:2").unwrap();
        assert_eq!(
            auto.iter().map(|n| n.name.as_str()).collect::<Vec<_>>(),
            vec!["m1", "m2"]
        );
    }

    /// A ring address is dialed by a CLIENT, so a bind wildcard is a parse
    /// error — the same rule `manager define` applies to a roster.
    #[test]
    fn ring_refuses_wildcards_and_unnamed_junk() {
        assert!(parse_ring("m1=0.0.0.0:50053").is_err());
        assert!(parse_ring("m1=localhost").is_err());
        assert!(parse_ring("m1=a:1,m1=b:1").is_err());
        assert!(parse_ring("  ").is_err());
        assert!(parse_ring("too long a name!!=a:1").is_err());
    }

    /// The placement sweep interval is configuration, not a constant: it
    /// defaults to what every sharded deployment measured before the flag
    /// existed, and a `0` is carried through EXACTLY as written so the server can
    /// refuse it with its own message instead of silently serving a split write.
    #[test]
    fn sweep_interval_defaults_then_overrides() {
        let s = ring("a=10.0.0.1:50053,b=10.0.0.2:50053", "a", "s");
        assert_eq!(s.sweep_ms(), DEFAULT_SHARD_SWEEP_MS);
        assert_eq!(s.clone().with_sweep_ms(1_000).sweep_ms(), 1_000);
        assert_eq!(s.with_sweep_ms(0).sweep_ms(), 0);
    }

    /// Self-selection: by name (case-insensitive), by address, by index, or by
    /// the advertised address when no `--shard-id` was given. Anything else is a
    /// startup error, never a guess.
    #[test]
    fn shard_finds_itself_in_the_ring() {
        let spec = "m1=10.0.0.1:50053,m2=10.0.0.2:50053";
        assert_eq!(
            ring(spec, "m2", "s").me(),
            &MetaNode {
                name: "m2".into(),
                addr: "10.0.0.2:50053".into()
            }
        );
        assert_eq!(ring(spec, "M1", "s").me().name, "m1");
        assert_eq!(ring(spec, "10.0.0.2:50053", "s").me().name, "m2");
        assert_eq!(ring(spec, "0", "s").me().name, "m1");
        let by_advertise = Shard::new(
            spec,
            None,
            Some("10.0.0.2:50053"),
            "postgres://h/db".into(),
            "s".into(),
        )
        .unwrap();
        assert_eq!(by_advertise.me().name, "m2");
        assert!(Shard::new(spec, Some("m9"), None, "postgres://h/db".into(), "s".into()).is_err());
        assert!(Shard::new(spec, None, None, "postgres://h/db".into(), "s".into()).is_err());
    }

    /// A one-node ring never asks the control plane anything — the flag alone
    /// must not turn a working single-meta deployment into one that depends on
    /// an extra round trip per mount.
    #[test]
    fn solo_ring_routes_locally_without_a_query() {
        let s = ring("m1=10.0.0.1:50053", "m1", "s");
        assert!(s.is_solo());
        assert_eq!(s.route("alice").unwrap(), Home::Mine);
        assert!(!ring("m1=a:1,m2=b:1", "m1", "s").is_solo());
    }

    /// The routing rule, decided without a database: a row naming this node is
    /// Mine, a row naming a peer is a redirect to ITS address, and a row naming
    /// a node the ring does not define is refused rather than rerouted.
    #[test]
    fn placement_row_decides_home_or_redirect() {
        let s = ring("m1=a:1,m2=b:2", "m1", "tree:X");
        assert_eq!(
            s.decide(
                "v",
                Some(Placement {
                    meta: "m1".into(),
                    store: "tree:X".into()
                })
            )
            .unwrap(),
            Home::Mine
        );
        assert_eq!(
            s.decide(
                "v",
                Some(Placement {
                    meta: "m2".into(),
                    store: "tree:X".into()
                })
            )
            .unwrap(),
            Home::Other(MetaNode {
                name: "m2".into(),
                addr: "b:2".into()
            })
        );
        let dangling = s
            .decide(
                "v",
                Some(Placement {
                    meta: "m3".into(),
                    store: "tree:X".into(),
                }),
            )
            .unwrap_err();
        assert!(
            dangling.contains("'m3'") && dangling.contains("view move --to-meta"),
            "{dangling}"
        );
    }

    /// The verdict a DESTROY acts on: no row means "this instance's, clean it
    /// up", and a row naming a retired node stays a hard refusal. Both are the
    /// shape [`Shard::peek_forget`] reads out of the table, and neither may
    /// write: the second one is `view rm`'s whole answer to a dangling row, the
    /// first is what lets a pre-sharding tree be removed from the instance that
    /// has it. `route()`'s bootstrap arm would instead PIN the name here.
    #[test]
    fn a_destroy_verb_answers_from_the_row_it_never_writes() {
        let s = ring("m1=a:1,m2=b:2", "m1", "tree:X");
        // No row: the read-only verdict is Mine (so the delete may run), which is
        // exactly why the verb cannot use `route()`'s pin — the pin would record
        // this instance as the home of a name it may not even hold.
        assert_eq!(s.decide("v", None).unwrap(), Home::Mine);
        // A row naming a node the ring retired: refused, whatever the verb is.
        assert!(s
            .decide(
                "v",
                Some(Placement {
                    meta: "gone".into(),
                    store: "tree:X".into(),
                })
            )
            .unwrap_err()
            .contains("does not define"));
    }

    /// Being named home is not enough: if the row was written by a process on a
    /// DIFFERENT tree backend, this one cannot read the tree, and opening a
    /// fresh empty one behind the same name is the failure this guards.
    #[test]
    fn home_with_the_wrong_backend_refuses_instead_of_seeding() {
        let s = ring("m1=a:1,m2=b:2", "m1", "postgres://h/db");
        let e = s
            .decide(
                "v",
                Some(Placement {
                    meta: "m1".into(),
                    store: "sqlite (local)".into(),
                }),
            )
            .unwrap_err();
        assert!(e.contains("--meta-store"), "{e}");
        // The same backend (and a row written before the column existed) is fine.
        assert!(s
            .decide(
                "v",
                Some(Placement {
                    meta: "m1".into(),
                    store: "postgres://h/db".into()
                })
            )
            .is_ok());
        assert!(s
            .decide(
                "v",
                Some(Placement {
                    meta: "m1".into(),
                    store: String::new()
                })
            )
            .is_ok());
    }

    /// The fingerprint identifies databases, never credentials: the same store
    /// seen from two nodes must hash out equal, and a password must not survive
    /// into the row. (A bare `--meta-store postgres://…` adopted into
    /// `<base>/instances.conf` is the exception today — see the
    /// `TODO(severity=Medium)` on [`store_fingerprint`].)
    #[test]
    fn backend_fingerprint_keeps_hosts_and_drops_secrets() {
        let pg = StoreKind::Pg("postgres://u:s3cret@h1:5432/gfs,postgres://u:ot@h2/gfs".into());
        assert_eq!(
            store_fingerprint(&pg),
            "postgres://h1:5432/gfs,postgres://h2/gfs"
        );
        assert_eq!(
            store_fingerprint(&StoreKind::Pg("file:/etc/grovefs/instances.conf".into())),
            "postgres registry /etc/grovefs/instances.conf"
        );
        assert_eq!(store_fingerprint(&StoreKind::Sqlite), "sqlite (local)");
        assert_eq!(
            redact_authority("postgres://h/db?sslmode=disable"),
            "postgres://h/db"
        );
        assert_eq!(redact_authority("not-a-uri"), "not-a-uri");
    }
}

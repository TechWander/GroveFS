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

/// The sharding configuration of one `meta-server`: the ring, this process's
/// place in it, where the placement table lives, and which tree backend this
/// process serves. Built once at `serve()` and shared immutably; `None` in
/// [`MetaRuntime`] means sharding is off, which is the historical single-process
/// deployment and must cost zero queries.
#[derive(Clone, Debug)]
pub struct Shard {
    nodes: Vec<MetaNode>,
    me: usize,
    /// The control-plane PostgreSQL URI the placement table lives in.
    ctl: String,
    /// This process's tree backend, credential-free ([`store_fingerprint`]).
    store: String,
    /// The `--shard-ring` text, quoted verbatim in error messages.
    origin: String,
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
                "{how} is not this ring ({origin}) — nodes are {} \n                     (name them, or fix the address)",
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
        })
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
    /// apply the same rule. `M_OPEN` and the governance verbs use `route` (an
    /// unplaced view becomes this instance's, written once under lock); the
    /// per-op gate uses THIS, because a data path must not grow placement rows —
    /// a view with no row is simply this instance's, which is what a pin would
    /// have recorded anyway.
    pub fn peek(&self, view: &str) -> Result<Home, String> {
        if self.is_solo() {
            return Ok(Home::Mine);
        }
        let row = self.read_placement(view)?;
        self.decide(view, row)
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
        if let Ok(mut c) = self.connect() {
            let _ = c.execute(
                &format!("DELETE FROM {META_PLACEMENT_TABLE} WHERE view = $1"),
                &[&view],
            );
        }
    }

    /// Every placement row, as view → node name (`store.info`'s shard page).
    /// Read-only: it never CREATES the table, so an operator pointing a
    /// non-sharded deployment at the console does not grow an empty table — a
    /// missing one simply means nothing has been placed yet.
    pub fn placement_map(&self) -> Result<HashMap<String, String>, String> {
        let mut c = self.connect()?;
        let exists = c
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
            .unwrap_or(false);
        if !exists {
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
    }

    /// Placed-view count per node name, ring order preserved for the report.
    /// A node with no rows shows 0, which is the point of counting for the
    /// operator: an empty instance is visible, not merely absent.
    ///
    /// The error travels rather than becoming a zero: `store.info` is the page an
    /// operator opens when a deployment looks wrong, and "every instance holds 0
    /// views" reported from an unreadable table is the shape of a false alarm.
    pub fn placement_counts(&self) -> Result<Vec<(MetaNode, i64)>, String> {
        let counts = self.placement_map()?;
        Ok(self
            .nodes
            .iter()
            .map(|n| {
                (
                    n.clone(),
                    counts.values().filter(|v| *v == &n.name).count() as i64,
                )
            })
            .collect())
    }

    /// A connection to the control plane, BOUNDED at both ends. `tokio-postgres`
    /// 0.7 exposes a connect timeout on `Config` and nothing for statements, so
    /// the statement budget is set on the session: without it, a wedged control
    /// plane would hand this process a parked meta worker per mount attempt
    /// (the rule ADR-0005 states for the HA path, applied here).
    fn connect(&self) -> Result<postgres::Client, String> {
        let mut cfg: postgres::Config = self
            .ctl
            .parse()
            .map_err(|e| format!("cannot parse the control-plane URI: {e}"))?;
        cfg.connect_timeout(SHARD_DB_TIMEOUT)
            .tcp_user_timeout(SHARD_DB_TIMEOUT * 2);
        let mut c = cfg.connect(postgres::NoTls).map_err(|e| {
            format!(
                "cannot reach the shard control plane for view placement ({e}) — which \
                 instance serves the view is undecided, refusing to guess"
            )
        })?;
        let ms = SHARD_DB_TIMEOUT.as_millis().to_string();
        c.batch_execute(&format!(
            "SET statement_timeout = '{ms}'; SET idle_in_transaction_session_timeout = '{ms}'"
        ))
        .map_err(|e| format!("cannot bound the shard control-plane timeouts: {e}"))?;
        Ok(c)
    }

    /// Read one row, creating the table first under the control schema's DDL
    /// lock — exactly the contract [`crate::pg_store::lookup_placement`] states:
    /// a routing read must not depend on which process happened to initialize
    /// the control plane, and the fresh-ring case (no table yet, so nothing
    /// placed) is a normal answer rather than a refused mount.
    fn read_placement(&self, view: &str) -> Result<Option<Placement>, String> {
        let mut c = self.connect()?;
        let mut tx = c
            .transaction()
            .map_err(|e| format!("postgres begin: {e}"))?;
        ensure_placement(&mut tx)?;
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
            .map_err(|e| format!("postgres commit (shard placement read): {e}"))?;
        Ok(row)
    }

    /// Insert `view`'s row unless one exists, then read back whatever won. One
    /// transaction, one advisory lock: two instances pinning the same new view
    /// serialize, and the loser learns the winner's name from the same round
    /// trip rather than guessing that its own insert took effect.
    fn pin(&self, view: &str) -> Result<Placement, String> {
        let mut c = self.connect()?;
        let mut tx = c
            .transaction()
            .map_err(|e| format!("postgres begin: {e}"))?;
        ensure_placement(&mut tx)?;
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
    }

    fn write_placement(
        &self,
        view: &str,
        meta: &str,
        store: &str,
        upsert: bool,
    ) -> Result<(), String> {
        let mut c = self.connect()?;
        let mut tx = c
            .transaction()
            .map_err(|e| format!("postgres begin: {e}"))?;
        ensure_placement(&mut tx)?;
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
    }
}

/// Create the table if absent, under the control schema's own DDL lock. Every
/// writer calls it, so an engine open never depends on which process happened to
/// initialize the control plane first.
fn ensure_placement(tx: &mut postgres::Transaction) -> Result<(), String> {
    tx.execute(
        "SELECT pg_advisory_xact_lock($1)",
        &[&crate::control::DDL_LOCK_CLASS],
    )
    .map_err(|e| format!("postgres advisory lock: {e}"))?;
    tx.execute(META_PLACEMENT_DDL, &[])
        .map_err(|e| format!("postgres ensure {META_PLACEMENT_TABLE}: {e}"))?;
    Ok(())
}

/// The credential-free identity of a tree backend, for the placement row.
///
/// Deliberately NOT the URI as configured: `--meta-store` embeds a password and
/// a placement row is readable by every instance in the deployment (and shows up
/// in `store.info`), so what belongs in the row is the part that identifies the
/// databases — scheme, host, port, path — and nothing else. A `file:` registry
/// contributes its PATH, which is the operator-owned name of the same book.
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
    /// into the row.
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

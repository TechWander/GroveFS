//! MCP (Model Context Protocol) integration: lets an AI agent customize
//! data listings through the same governance API the `grovefs view` CLI drives.
//!
//! Two halves live here:
//! - **Shared view-governance ops** ([`define_view`], [`fork_view`], [`graft_view`],
//!   [`alias_view`], [`list_views`], [`remove_view`], [`preview_rules`]): every validation and
//!   file mutation of `grovefs view …` in one place, so the CLI and the MCP
//!   server cannot drift (one contract, two entrances).
//! - **A dependency-light MCP server over stdio** ([`serve`]): newline-delimited
//!   JSON-RPC 2.0 implementing the `initialize` / `tools/list` / `tools/call`
//!   core of the 2025-06-18 protocol revision, with ten tools: eight governance
//!   writes/validations mapping 1:1 to the ops above (`validate_definition` runs
//!   the FULL publish guard set as a no-write dry run, beyond `preview_rules`'
//!   verb-syntax check; [`fork_view`] seeds an independent tree — or, with
//!   `seed: false`, a root-only one — and [`graft_view`] fills it with a
//!   curated, renamable subset) plus two read-only
//!   introspection tools ([`view_tree`], [`describe_view`]) that let an agent
//!   discover the real paths a listing shows (rendering an existing view through
//!   its own lens, or simulating an unpublished `base` + `rules` pair) and read
//!   back a single definition verbatim. No framework crate: the protocol surface
//!   we implement is a handful of methods, and serde_json `Value` is enough.
//!
//! Beyond the tool calls the same governance is attachable as MCP *resources*
//! (`grovefs://guide`, `grovefs://views`, `grovefs://view/<name>/definition` and
//! `/tree{?path,depth,max}`), *prompts* (the two playbooks) and *completion*.
//! Tools are annotated (`readOnlyHint`/`destructiveHint`/`idempotentHint`) so a
//! client can auto-approve the read-only half, and the four machine-readable
//! ones declare an `outputSchema` plus `structuredContent`, so an agent gets a
//! typed payload instead of re-parsing the text block. Errors name what was
//! wrong AND what to do next (the whole tool surface, the required arguments,
//! the nearest tool name).
//!
//! The MCP entry point is `grovefs mcp`: an MCP client (Claude Desktop, any
//! agent runtime) spawns it as a subprocess, and the agent then lists views,
//! inspects their listings, and reshapes them (`hide`/`only`/`map`/`link` rules
//! inline as text — the agent never needs to touch a rules *file*) while live
//! `meta-server` mounts hot-reload the new shape within their cache TTL.
//!
//! The two introspection tools are metadata-only: they open a view's tree
//! READ-ONLY through the store's own engine (local SQLite, or the PG backend
//! the control plane is bound to) and walk it (through the same [`LayoutMeta`]
//! lens a mount uses), touching no object bytes, mounting nothing, and doing
//! no auth. That also means the `size` a tree render reports is the METADATA
//! field, not the object's byte count — see [`TreeEntry`] for the exact
//! per-kind meaning.

use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::apikeys::{ApiKeyStore, AuthedKey};
use crate::control::{schema_name_for, view_from_schema_name, PgPool};
use crate::layout::{canon, valid_view_name, LayoutMeta, LayoutRules, ViewDef};
use crate::meta::{MetaEngine, MetaProvider, NodeKind, ROOT_INO};

/// Protocol revision we advertise when the client does not name one.
pub const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

/// Protocol revisions this server can actually honor. `initialize` echoes a
/// revision from this list and answers an unknown one with
/// [`MCP_PROTOCOL_VERSION`] — the spec's "reply with a version you support"
/// rule, so a client never believes it negotiated something this server lacks.
pub const SUPPORTED_PROTOCOLS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

/// The whole tool surface, in `tools/list` order — the single source for the
/// unknown-tool hint and the dry-run/tool metadata tables below.
pub const TOOL_NAMES: [&str; 10] = [
    "list_views",
    "define_derived_view",
    "alias_view",
    "fork_view",
    "graft_view",
    "remove_view",
    "preview_rules",
    "validate_definition",
    "list_view_tree",
    "get_view",
];

/// Required arguments per tool, so a missing one names the tool and the full
/// required set instead of a bare `missing string argument 'name'`.
const TOOL_REQUIRED: [(&str, &[&str]); 10] = [
    ("list_views", &[]),
    ("define_derived_view", &["name", "base", "rules"]),
    ("alias_view", &["name", "real"]),
    ("fork_view", &["name", "base"]),
    ("graft_view", &["name", "map"]),
    ("remove_view", &["name"]),
    ("preview_rules", &["rules"]),
    ("validate_definition", &["name", "base", "rules"]),
    ("list_view_tree", &[]),
    ("get_view", &["name"]),
];

/// The server's operating manual, returned as `initialize.instructions` and as
/// the `grovefs://guide` resource. It is deliberately sectioned: an agent reads
/// it once and can skim to the part it needs (the previous one-paragraph form
/// was a wall of text). Every tool name must appear here — discovery that only
/// exists in `tools/list` is discovery an agent misses.
pub const INSTRUCTIONS: &str = "\
GroveFS view governance over ONE control-plane store (local SQLite files, or a PostgreSQL `grovefs_control` schema). You reshape how data LISTINGS are presented. This entrance never mounts a filesystem and never reads object bytes.

## Discover (read-only, metadata only)
- `list_views` — every slot as {backend, store, physical[], defined[]}
- `get_view` — one slot in full, including the verbatim `definition` rules (read before editing)
- `list_view_tree` — the exact listing a mount would show; pass `view`, or `base` + `rules` to preview an unpublished lens. Each entry `path` is the absolute DISPLAY path to reuse in rules. `size` is metadata (dirs 4096, files 0, symlinks target length), never a byte count.

## Reshape (writes <db>/meta/views/*.layout; live meta-server mounts hot-reload within their cache TTL)
- `define_derived_view` — read-only lens from rule text: `hide /x` (prune), `only /x` (serve just that), `map /src /dst` (re-home), `link /src /dst` (second name). Paths MUST come from `list_view_tree`; a `base` line inside rules is refused.
- `alias_view` — writable public name for a physical view; `retire` rotates the previous name gaplessly
- `remove_view` — drop a definition (a fork also drops its own tree; content bytes survive while any view holds them)

## Curate
- `fork_view` — independent metadata tree seeded from a base; `seed:false` starts EMPTY (root only). Structure writable, content read-only, bytes shared.
- `graft_view` — place a curated, renamable subset into a fork or base view; plan lines are `<src>` or `<src>TAB<dst>`, the whole plan lands in one transaction.

## Validate before publishing (writes nothing)
- `preview_rules` — verb syntax only, line-numbered errors
- `validate_definition` — the FULL guard set as a dry run; returns the exact refusal a real publish would give

## Golden order
`list_views` -> (`get_view` when editing) -> `list_view_tree` -> draft rules -> `preview_rules` -> `validate_definition` -> `define_derived_view` -> `list_view_tree` (verify the rendered shape). Do not skip the dry run, and do not treat a truncated tree as the whole tree.

## Beyond tools
MCP resources: `grovefs://guide` (this manual), `grovefs://views` (whole-store snapshot), `grovefs://view/<name>/definition`, and `grovefs://view/<name>/tree[?path=/sub&depth=6&max=800]` (drill into a big tree without a tool call). Prompts: `reshape-view`, `rotate-alias`. Completion fills view-name arguments. `logging/setLevel` streams `notifications/message` per call.

## Mechanics
Protocol 2025-06-18 over newline-delimited JSON-RPC 2.0 on stdio (2025-03-26 / 2024-11-05 clients are accepted too). Governance refusals arrive INSIDE the result as `isError:true` with the CLI's verbatim text; the JSON-RPC error layer is protocol-only. Tools are annotated (`readOnlyHint` etc.) and the JSON-returning ones also carry `structuredContent`. If the server runs with `--require-api-key`, pass `apiKey: '<id>:<secret>'` in the `initialize` params. A per-call `db_path` re-points a FILE store; a PG-bound server ignores it. Introspection walks the bound store's own backend — a remote tree is not reachable.";

// ---------------------------------------------------------------------------
// Shared view-governance operations (CLI `grovefs view` and MCP tools, one impl)
// ---------------------------------------------------------------------------

/// One printed line from a governance op: `Info` goes to stdout on the CLI,
/// `Note` is an advisory line the CLI keeps on stderr (MCP inlines both into
/// the tool result text — an agent reads stderr anyway).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutLine {
    Info(String),
    Note(String),
}

impl OutLine {
    pub fn text(&self) -> &str {
        match self {
            OutLine::Info(s) | OutLine::Note(s) => s,
        }
    }
}

/// What `<db-path>/meta/views/<name>.layout` declares, resolved for machines
/// (the MCP `list_views` tool answers with these, not just prose).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewEntry {
    pub name: String,
    /// "derived" (read-only shaped lens) or "alias" (writable public name).
    pub kind: &'static str,
    /// The physical view the definition reads from / maps onto.
    pub real_view: String,
    pub hide: usize,
    pub only: usize,
    pub reloc: usize,
}

/// Everything a store exposes: physical view DBs plus governed definitions.
#[derive(Debug, Default)]
pub struct StoreViews {
    /// Physical view trees (the storage identities): `<db-path>/meta/<view>.db`
    /// files under SQLite, `grovefs_v_<hex>` schemas under PostgreSQL.
    pub physical: Vec<String>,
    /// The governed definitions (derived / alias / fork), sorted by name:
    /// `meta/views/*.layout` slots under SQLite, `grovefs_control.view_defs`
    /// rows under PostgreSQL.
    pub defined: Vec<ViewEntry>,
}

// ---------------------------------------------------------------------------
// The control-plane store: ONE backend for definitions AND keys
// ---------------------------------------------------------------------------

/// Where view definitions (and, mirrored by [`crate::apikeys::ApiKeyStore`],
/// the API-key/audit store) actually live. The rule is simple: the control
/// plane follows the metadata backend —
/// - **SQLite** (default): definitions are plain-text slots under
///   `<db-path>/meta/views/<name>.layout`, keys stay in `<db-path>/meta/api_keys.db`;
/// - **PostgreSQL** (`--store postgres://…`): definitions are rows of
///   `grovefs_control.view_defs`, keys and audit rows land in
///   `grovefs_control.api_keys` / `grovefs_control.mcp_calls` — a single
///   system of record, shared by every meta-server / MCP / CLI pointing at
///   the same URI (nothing to sync, nothing to co-locate).
///
/// Every governance op in this module is written against the trait, so the
/// CLI, the MCP server and the `meta-server` resolver cannot drift per backend.
pub trait ControlStore: Send + Sync {
    /// "postgres" | "sqlite" — surfaced in tool JSON so an agent can tell
    /// which system of record it is talking to.
    fn backend(&self) -> &'static str;
    /// A human name for the store location, used in error messages.
    fn describe(&self) -> String;
    /// Every governed definition: (name, verbatim text), unsorted.
    fn list_defs(&self) -> Result<Vec<(String, String)>, String>;
    /// The verbatim definition text for `name`, if one exists.
    fn get_def_raw(&self, name: &str) -> Result<Option<String>, String>;
    /// Read and parse one definition (a malformed slot is an error, never a
    /// silently missing view).
    fn get_def(&self, name: &str) -> Result<Option<ViewDef>, String> {
        match self.get_def_raw(name)? {
            Some(text) => {
                Ok(Some(LayoutRules::parse_def(&text).map_err(|e| {
                    format!("bad view definition for '{name}': {e}")
                })?))
            }
            None => Ok(None),
        }
    }
    fn has_def(&self, name: &str) -> Result<bool, String> {
        Ok(self.get_def_raw(name)?.is_some())
    }
    fn put_def(&self, name: &str, text: &str) -> Result<(), String>;
    fn remove_def(&self, name: &str) -> Result<bool, String>;
    /// A cheap content identity for hot-reload: a cached lens records it and
    /// re-reads the definition once it differs (file mtime, or the row's
    /// `xmin` write version). `None` = no definition right now.
    fn stamp(&self, name: &str) -> Option<String>;
    /// Has `name`'s definition changed since the cached stamp `seen`? File
    /// stores answer from a `stat`; the PG store runs one single-row SELECT
    /// (the caller rate-probes at the ≤2s lens TTL, matching the file path's
    /// convergence window).
    fn defs_changed(&self, name: &str, seen: &str) -> bool {
        self.stamp(name).is_none_or(|s| s != seen)
    }
    /// The pure-alias map (alias name → real view) as a resolver would apply
    /// it: malformed or non-alias entries are skipped (the branches that need
    /// a definition parse it and surface the error).
    fn load_alias_map(&self) -> Result<HashMap<String, String>, String> {
        let mut out = HashMap::new();
        for (name, text) in self.list_defs()? {
            if let Ok(ViewDef::Alias(real)) = LayoutRules::parse_def(&text) {
                out.insert(name, real);
            }
        }
        Ok(out)
    }
    /// Every physical view tree stored here.
    fn list_physical(&self) -> Result<Vec<String>, String>;
    /// Is `name` a physical tree (its own db file / PG schema)?
    fn has_physical(&self, name: &str) -> Result<bool, String>;
    /// `--db-path`-style roots only make sense for a file store; the PG store
    /// is bound to its URI and ignores an override (reported honestly in the
    /// tool result rather than silently reading another store).
    fn store_for_db_path(&self, db: &Path) -> Arc<dyn ControlStore>;
    /// Does this store keep fork trees as files beside the definition (and
    /// therefore refuse to serve a fork whose tree vanished)?
    fn has_fork_tree(&self, name: &str) -> Result<bool, String>;
    /// Seed an independent tree for a fork of `base` (SQLite copy; PG
    /// not-implemented refusal naming the exact reason). `seed == false`
    /// creates the fork's OWN empty tree (schema + root) for [`Self::graft_tree`]
    /// to fill with a curated subset.
    fn create_fork_tree(
        &self,
        base: &str,
        name: &str,
        seed: bool,
    ) -> Result<crate::meta::ForkStats, String>;
    /// Graft a curated subset of `src`'s names into `dst`'s own tree, renaming
    /// each entry to its destination basename (SQLite only — a PG store refuses
    /// it for the same reason it refuses a fork: no per-view snapshot path).
    fn graft_tree(
        &self,
        dst: &str,
        src: &str,
        items: &[crate::meta::GraftItem],
        dest: &str,
        skip_existing: bool,
    ) -> Result<crate::meta::GraftStats, String>;
    /// Roll a freshly seeded fork tree back after its definition slot failed
    /// to be recorded (best effort, like the file path's unlink).
    fn destroy_fork_tree(&self, name: &str);
    /// Fork semantics after `remove_def`: delete the fork's own tree and
    /// rebuild the cross-view ledger so the base can GC again (SQLite); a
    /// no-op while the fork feature is not implemented on this backend.
    fn drop_fork_tree(&self, name: &str) -> Result<(), String>;
    /// Open one view's tree READ-ONLY for the metadata-only introspection
    /// tools (a missing tree is reported, never silently empty). A probe never
    /// creates anything on either backend.
    fn open_ro_view(&self, view: &str) -> Result<Arc<dyn MetaProvider>, String>;
    /// Whether a governance op should note that `view` has no local tree yet.
    /// File stores check the db file; the PG store never warns (a missing
    /// schema is reported by the op that needs it, not by a pre-flight note).
    fn base_tree_missing_note(&self, view: &str) -> Result<Option<String>, String>;
}

/// The local-file control plane: `<base>/meta/views/<name>.layout` slots plus
/// the physical `<base>/meta/<view>.db` trees (the SQLite system of record,
/// unchanged from before this abstraction existed).
pub struct FileControlStore {
    base: PathBuf,
}

impl FileControlStore {
    pub fn new(base: &Path) -> Self {
        Self {
            base: base.to_path_buf(),
        }
    }

    fn def_dir(&self) -> PathBuf {
        self.base.join("meta").join("views")
    }

    fn slot(&self, name: &str) -> PathBuf {
        self.def_dir().join(format!("{name}.layout"))
    }

    fn db(&self, name: &str) -> PathBuf {
        self.base.join("meta").join(format!("{name}.db"))
    }
}

impl ControlStore for FileControlStore {
    fn backend(&self) -> &'static str {
        "sqlite"
    }

    fn describe(&self) -> String {
        format!("{} (local files)", self.base.display())
    }

    fn list_defs(&self) -> Result<Vec<(String, String)>, String> {
        let dir = self.def_dir();
        let mut out = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for ent in rd.flatten() {
                let p = ent.path();
                if p.extension().and_then(|e| e.to_str()) != Some("layout") {
                    continue;
                }
                let Some(stem) = p.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                let text = std::fs::read_to_string(&p)
                    .map_err(|e| format!("cannot read {}: {e}", p.display()))?;
                out.push((stem.to_string(), text));
            }
        }
        Ok(out)
    }

    fn get_def_raw(&self, name: &str) -> Result<Option<String>, String> {
        let p = self.slot(name);
        if !p.is_file() {
            return Ok(None);
        }
        std::fs::read_to_string(&p)
            .map(Some)
            .map_err(|e| format!("cannot read {}: {e}", p.display()))
    }

    fn put_def(&self, name: &str, text: &str) -> Result<(), String> {
        let dir = self.def_dir();
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        std::fs::write(self.slot(name), text).map_err(|e| e.to_string())
    }

    fn remove_def(&self, name: &str) -> Result<bool, String> {
        let p = self.slot(name);
        if !p.is_file() {
            return Ok(false);
        }
        std::fs::remove_file(&p)
            .map(|()| true)
            .map_err(|e| format!("cannot remove {}: {e}", p.display()))
    }

    fn stamp(&self, name: &str) -> Option<String> {
        let m = std::fs::metadata(self.slot(name)).ok()?;
        let mt = m.modified().ok()?;
        let d = mt
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|d| format!("{}.{:09}", d.as_secs(), d.subsec_nanos()))
            .unwrap_or_default();
        Some(format!("{}:{d}", m.len()))
    }

    fn list_physical(&self) -> Result<Vec<String>, String> {
        let mut out = Vec::new();
        if let Ok(rd) = std::fs::read_dir(self.base.join("meta")) {
            for ent in rd.flatten() {
                let p = ent.path();
                if p.extension().and_then(|e| e.to_str()) == Some("db") {
                    if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                        // `api_keys.db` (the key/audit store) and `refs.db` (the
                        // cross-view holder ledger) are control-plane stores, not
                        // view trees — never surface them as mountable names.
                        if stem != "api_keys" && stem != "refs" {
                            out.push(stem.to_string());
                        }
                    }
                }
            }
        }
        out.sort();
        out.dedup();
        Ok(out)
    }

    fn has_physical(&self, name: &str) -> Result<bool, String> {
        Ok(self.db(name).is_file())
    }

    fn store_for_db_path(&self, db: &Path) -> Arc<dyn ControlStore> {
        Arc::new(FileControlStore::new(db))
    }

    fn has_fork_tree(&self, name: &str) -> Result<bool, String> {
        Ok(self.db(name).is_file())
    }

    fn create_fork_tree(
        &self,
        base: &str,
        name: &str,
        seed: bool,
    ) -> Result<crate::meta::ForkStats, String> {
        crate::meta::fork_view_with(&self.base, base, name, seed)
    }

    fn graft_tree(
        &self,
        dst: &str,
        src: &str,
        items: &[crate::meta::GraftItem],
        dest: &str,
        skip_existing: bool,
    ) -> Result<crate::meta::GraftStats, String> {
        crate::meta::graft_view(&self.base, dst, src, items, dest, skip_existing)
    }

    fn destroy_fork_tree(&self, name: &str) {
        let meta_dir = self.base.join("meta");
        for ext in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(meta_dir.join(format!("{name}.db{ext}")));
        }
    }

    fn drop_fork_tree(&self, name: &str) -> Result<(), String> {
        let meta_dir = self.base.join("meta");
        // Definition is gone first: the name stops resolving even if a db file
        // lingers (e.g. a live meta-server still holds its handle open).
        for ext in ["", "-wal", "-shm"] {
            let db = meta_dir.join(format!("{name}.db{ext}"));
            if db.exists() {
                std::fs::remove_file(&db).map_err(|e| {
                    format!(
                        "removed the definition but cannot delete the fork tree {}: {e}",
                        db.display()
                    )
                })?;
            }
        }
        // The fork's holds died with its tree: rewrite the ledger from what
        // survives so the baseline can actually free its own bytes again.
        crate::meta::rebuild_ledger(&self.base)
            .map_err(|e| format!("fork tree removed, but the ledger rebuild failed: {e}"))?;
        Ok(())
    }

    fn open_ro_view(&self, view: &str) -> Result<Arc<dyn MetaProvider>, String> {
        let p = self.db(view);
        if !p.is_file() {
            return Err(format!(
                "no metadata tree for view '{view}' ({}) — list_view_tree walks this store's own backend; a remote tree not reachable from here is not",
                p.display()
            ));
        }
        MetaEngine::open_ex(&p, true)
            .map(|e| -> Arc<dyn MetaProvider> { Arc::new(e) })
            .map_err(|e| format!("cannot open view '{view}' metadata read-only: {e}"))
    }

    fn base_tree_missing_note(&self, view: &str) -> Result<Option<String>, String> {
        Ok(if self.db(view).is_file() {
            None
        } else {
            // Name the concrete tree path the caller is missing rather than a
            // `<db-path>` placeholder, so the note is copy-paste actionable and
            // tells the user exactly which file `init`/`create` must produce (§6-5).
            Some(format!(
                "note: no local tree {} yet — create it with `grovefs --home {} init --view {view}` (or `view create`) first",
                self.db(view).display(),
                self.base.display()
            ))
        })
    }
}

/// The PostgreSQL control plane: definitions in `grovefs_control.view_defs`,
/// physical views resolved as the `grovefs_v_<hex>` schemas [`crate::pg_store`]
/// creates, and (through [`crate::apikeys::ApiKeyStore::open_pg`]) the key and
/// audit tables in the same schema. One URI, one system of record for
/// everything governance — no local config directory involved.
pub struct PgControlStore {
    pool: Arc<PgPool>,
}

/// The DDL the PG control plane owns for DEFINITIONS (keys/audit add their
/// own tables through [`crate::apikeys::ApiKeyStore::open_pg`]; all
/// statements are `IF NOT EXISTS`, so either store can open first).
const DEFS_DDL: &str = "CREATE TABLE IF NOT EXISTS grovefs_control.view_defs (
    name        TEXT PRIMARY KEY,
    definition  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
)";

impl PgControlStore {
    pub fn new(uri: &str) -> Result<Self, String> {
        let pool = PgPool::open(uri, &[DEFS_DDL])?;
        Ok(Self {
            pool: Arc::new(pool),
        })
    }

    /// `updated_at` is kept for operators; the hot-reload ticket is `xmin`
    /// (the row's write version, read as `xmin::text`), which changes on every
    /// UPDATE and never on an equal-content re-read — exactly the file store's
    /// mtime role.
    fn stamp_query(&self, name: &str) -> Option<String> {
        let rows = self
            .pool
            .query(
                "SELECT xmin::text FROM grovefs_control.view_defs WHERE name = $1",
                &[&name],
                |r| r.get::<_, String>(0),
            )
            .ok()?;
        rows.into_iter().next()
    }
}

impl ControlStore for PgControlStore {
    fn backend(&self) -> &'static str {
        "postgres"
    }

    fn describe(&self) -> String {
        format!("{} (PostgreSQL grovefs_control)", self.pool.uri())
    }

    fn list_defs(&self) -> Result<Vec<(String, String)>, String> {
        self.pool.query(
            "SELECT name, definition FROM grovefs_control.view_defs ORDER BY name",
            &[],
            |r| (r.get::<_, String>(0), r.get::<_, String>(1)),
        )
    }

    fn get_def_raw(&self, name: &str) -> Result<Option<String>, String> {
        let mut rows = self.pool.query(
            "SELECT definition FROM grovefs_control.view_defs WHERE name = $1",
            &[&name],
            |r| r.get::<_, String>(0),
        )?;
        Ok(rows.pop())
    }

    fn put_def(&self, name: &str, text: &str) -> Result<(), String> {
        let now = chrono::Local::now().to_rfc3339();
        self.pool.execute(
            "INSERT INTO grovefs_control.view_defs (name, definition, updated_at) VALUES ($1, $2, $3)
             ON CONFLICT (name) DO UPDATE SET definition = EXCLUDED.definition, updated_at = EXCLUDED.updated_at",
            &[&name, &text, &now],
        )?;
        Ok(())
    }

    fn remove_def(&self, name: &str) -> Result<bool, String> {
        Ok(self.pool.execute(
            "DELETE FROM grovefs_control.view_defs WHERE name = $1",
            &[&name],
        )? > 0)
    }

    fn stamp(&self, name: &str) -> Option<String> {
        self.stamp_query(name)
    }

    fn list_physical(&self) -> Result<Vec<String>, String> {
        let rows = self.pool.query(
            "SELECT schema_name FROM information_schema.schemata WHERE schema_name LIKE 'grovefs_v_%'",
            &[],
            |r| r.get::<_, String>(0),
        )?;
        let mut out: Vec<String> = rows
            .iter()
            .filter_map(|s| view_from_schema_name(s))
            .collect();
        out.sort();
        out.dedup();
        Ok(out)
    }

    fn has_physical(&self, name: &str) -> Result<bool, String> {
        let schema = schema_name_for(name);
        let rows = self.pool.query(
            "SELECT 1 FROM information_schema.schemata WHERE schema_name = $1",
            &[&schema],
            |_| 1,
        )?;
        Ok(!rows.is_empty())
    }

    fn store_for_db_path(&self, _db: &Path) -> Arc<dyn ControlStore> {
        // The PG control plane is bound to its URI: a per-call `db_path` can
        // only re-point a FILE store. The tool layer reports which store
        // served the call, so an override is never silently ignored.
        let me: Arc<PgControlStore> = Arc::new(PgControlStore {
            pool: Arc::clone(&self.pool),
        });
        me
    }

    fn has_fork_tree(&self, _name: &str) -> Result<bool, String> {
        // PG has no per-fork file to outlive its definition row.
        Ok(true)
    }

    fn create_fork_tree(
        &self,
        _base: &str,
        _name: &str,
        _seed: bool,
    ) -> Result<crate::meta::ForkStats, String> {
        Err(
            "fork is not implemented on the PostgreSQL backend yet (the tree copy needs a per-view snapshot path; PG-backed views are served through a meta-server or a mount)"
                .to_string(),
        )
    }

    fn graft_tree(
        &self,
        _dst: &str,
        _src: &str,
        _items: &[crate::meta::GraftItem],
        _dest: &str,
        _skip_existing: bool,
    ) -> Result<crate::meta::GraftStats, String> {
        Err(
            "graft is not implemented on the PostgreSQL backend yet (it copies rows between per-view trees, which only the file store keeps as separate databases)"
                .to_string(),
        )
    }

    fn destroy_fork_tree(&self, _name: &str) {}

    fn drop_fork_tree(&self, _name: &str) -> Result<(), String> {
        // A fork definition can only have been seeded on a file store (see
        // `create_fork_tree`), so there is no PG tree to delete here.
        Ok(())
    }

    fn open_ro_view(&self, view: &str) -> Result<Arc<dyn MetaProvider>, String> {
        // A read-only probe must not create anything. `PgEngine::open` seeds the
        // view's schema (DDL), so calling it optimistically would let
        // `list_view_tree` mint a phantom physical view out of a typo — the very
        // thing the file-store branch above refuses. So check for the schema
        // first and refuse honestly if it is not there.
        if !self.has_physical(view)? {
            return Err(format!(
                "no metadata tree for view '{view}' ({} schema {}) — list_view_tree walks this store's own backend; a remote tree not reachable from here is not",
                self.pool.uri(),
                schema_name_for(view)
            ));
        }
        crate::pg_store::PgEngine::open(self.pool.uri(), view, true)
            .map(|p| -> Arc<dyn MetaProvider> { Arc::new(p) })
            .map_err(|e| format!("cannot open view '{view}' from {}: {e}", self.pool.uri()))
    }

    fn base_tree_missing_note(&self, _view: &str) -> Result<Option<String>, String> {
        // Under PG the tree is a schema, not a local db file; a CLI/MCP process
        // has nothing local to miss, so never warn.
        Ok(None)
    }
}

/// Build the control-plane store for a metadata backend selection: `None` /
/// `"sqlite"` → the local file store under `base`; a `postgres[ql]://` URI →
/// the shared PG control tables. Same URI grammar as `meta-server --store`.
pub fn control_store_for(
    base: &Path,
    store_uri: Option<&str>,
) -> Result<Arc<dyn ControlStore>, String> {
    match store_uri {
        Some(u) if u.starts_with("postgres://") || u.starts_with("postgresql://") => {
            PgControlStore::new(u).map(|s| Arc::new(s) as Arc<dyn ControlStore>)
        }
        _ => Ok(Arc::new(FileControlStore::new(base))),
    }
}

/// The store a governance op runs against: the server's bound store, except
/// that a FILE-STORE server still honours a per-call `db_path` override (the
/// historical multi-root behavior). A PG-bound server cannot be re-pointed at
/// a local directory — one backend, one system of record per server process.
pub enum StoreSel<'a> {
    Bound(&'a Arc<dyn ControlStore>),
    FileDefault(&'a Arc<dyn ControlStore>),
}

pub fn route_store(sel: StoreSel, db: Option<&Path>) -> Arc<dyn ControlStore> {
    match sel {
        StoreSel::Bound(s) => Arc::clone(s),
        StoreSel::FileDefault(s) => match db {
            Some(p) => s.store_for_db_path(p),
            None => Arc::clone(s),
        },
    }
}

/// One human-readable definition record: (name, kind, real view, rule counts).
fn entry_from_def(name: &str, d: &ViewDef) -> ViewEntry {
    match d {
        ViewDef::Alias(real) => ViewEntry {
            name: name.to_string(),
            kind: "alias",
            real_view: real.clone(),
            hide: 0,
            only: 0,
            reloc: 0,
        },
        ViewDef::Derived(b, r) => ViewEntry {
            name: name.to_string(),
            kind: "derived",
            real_view: b.clone(),
            hide: r.hide.len(),
            only: r.only.len(),
            reloc: r.reloc.len(),
        },
        ViewDef::Fork(b) => ViewEntry {
            name: name.to_string(),
            kind: "fork",
            real_view: b.clone(),
            hide: 0,
            only: 0,
            reloc: 0,
        },
    }
}

/// Run every `define_derived_view` governance guard against a candidate
/// (name, base, rules) WITHOUT touching the filesystem. Any failure comes back
/// as the exact CLI refusal string, so `define_view` (validate-then-write) and
/// the `validate_definition` dry run can never disagree — the agent sees the
/// same wording a real publish would produce. Returns the parsed rules on
/// success so callers can report hide/only/reloc counts.
fn check_derived_definition(
    store: &dyn ControlStore,
    name: &str,
    base: &str,
    rules_text: &str,
) -> Result<LayoutRules, String> {
    if !valid_view_name(name) {
        return Err(format!(
            "invalid view name '{name}' (must be one safe path element)"
        ));
    }
    if !valid_view_name(base) {
        return Err(format!("invalid base view name '{base}'"));
    }
    if name == base {
        return Err("derived view name must differ from its base".into());
    }
    if store.has_physical(name)? {
        return Err(format!(
            "'{name}' already names a physical view db — pick another name"
        ));
    }
    // One slot per name: an existing ALIAS may be replaced (a deliberate
    // governance change) but silently turning a public alias into a read-only
    // shaped view is not `define`'s business.
    if let Some(d) = store.get_def(name)? {
        match d {
            ViewDef::Alias(_) => {
                return Err(format!(
                    "'{name}' is a pure alias — remove it first (`grovefs view rm --name {name}`) before defining a derived view over it"
                ));
            }
            ViewDef::Fork(_) => {
                return Err(format!(
                    "'{name}' is a fork (an independent metadata tree) — remove it first (`grovefs view rm --name {name}`) before defining a derived view over it"
                ));
            }
            ViewDef::Derived(..) => {}
        }
    }
    // `base` is supplied by --base, so a base line inside the rules text is
    // rejected here (LayoutRules::parse has no such verb) — one unambiguous
    // source of truth per definition.
    let r = LayoutRules::parse(rules_text).map_err(|e| format!("bad layout rules: {e}"))?;
    Ok(r)
}

/// Every governance op is a flat function over the control-plane store — the
/// same backend the metadata tree uses (local files under `--db-path`/
/// `--home`, or the `grovefs_control` schema under a `postgres://` URI).
/// Errors are the exact CLI strings, so an agent sees precisely what a human
/// would.
pub fn define_view(
    store: &dyn ControlStore,
    name: &str,
    base: &str,
    rules_text: &str,
) -> Result<Vec<OutLine>, String> {
    let r = check_derived_definition(store, name, base, rules_text)?;
    let mut out = Vec::new();
    if store.has_def(name)? {
        out.push(OutLine::Info(format!(
            "view '{name}' already defined — replacing it (live meta-server mounts hot-reload it within their cache TTL)"
        )));
    }
    store.put_def(name, &format!("base {base}\n{rules_text}"))?;
    out.push(OutLine::Info(format!(
        "defined derived view '{name}' over base '{base}': {} hide, {} only, {} reloc rule(s)",
        r.hide.len(),
        r.only.len(),
        r.reloc.len()
    )));
    out.push(OutLine::Note(format!(
        "note: mounts of '{name}' are read-only (virtual shape) — write to base view '{base}'"
    )));
    if let Some(note) = store.base_tree_missing_note(base)? {
        out.push(OutLine::Note(note));
    }
    Ok(out)
}

/// Dry-run a full `define_derived_view`: run every governance guard against
/// (name, base, rules) WITHOUT creating the `views/` dir or writing the slot.
/// A guard failure comes back as the exact CLI refusal string (byte-identical
/// to a real publish, proving the one-contract claim); success returns a JSON
/// verdict with the accepted rule counts, whether it would REPLACE an existing
/// definition, and whether the base view's local db is present.
pub fn validate_definition(
    store: &dyn ControlStore,
    name: &str,
    base: &str,
    rules_text: &str,
) -> Result<Value, String> {
    let r = check_derived_definition(store, name, base, rules_text)?;
    let replaces_existing = store.has_def(name)?;
    let base_present = store.has_physical(base)?;
    Ok(json!({
        "accepted": true,
        "dry_run": true,
        "kind": "derived",
        "name": name,
        "base": base,
        "backend": store.backend(),
        "hide": r.hide.len(),
        "only": r.only.len(),
        "reloc": r.reloc.len(),
        "would_replace_existing_definition": replaces_existing,
        "base_view_db_present": base_present,
        "mount": "read-only (derived)",
        "note": store.base_tree_missing_note(base)?.unwrap_or_default(),
    }))
}

/// Create/replace a PURE ALIAS (name governance), optionally retiring a
/// previous alias atomically (write-before-delete: the real view stays
/// governed at every instant). Validation mirrors the CLI exactly.
pub fn alias_view(
    store: &dyn ControlStore,
    name: &str,
    real: &str,
    retire: Option<&str>,
) -> Result<Vec<OutLine>, String> {
    if !valid_view_name(name) {
        return Err(format!(
            "invalid alias name '{name}' (must be one safe path element)"
        ));
    }
    if !valid_view_name(real) {
        return Err(format!("invalid real view name '{real}'"));
    }
    if name == real {
        return Err("alias name must differ from the real view name".into());
    }
    if let Some(old) = &retire {
        if !valid_view_name(old) {
            return Err(format!("invalid --retire alias name '{old}'"));
        }
        if old == &name {
            return Err(
                "--retire must name a DIFFERENT (previous) alias, not the one being created".into(),
            );
        }
    }
    if store.has_physical(name)? {
        return Err(format!(
            "'{name}' already names a physical view db — pick another name"
        ));
    }
    // The alias target must be a PHYSICAL view (the storage identity the
    // alias maps onto), and must not itself be governed: chaining aliases or
    // aliasing a derived view would create resolution ambiguity the server
    // does not model (M_OPEN resolution is single-hop by design).
    let mut alias_to: HashMap<String, String> = HashMap::new();
    let mut derived_names: Vec<String> = Vec::new();
    let mut fork_names: Vec<String> = Vec::new();
    for (stem, text) in store.list_defs()? {
        if let Ok(d) = LayoutRules::parse_def(&text) {
            match d {
                ViewDef::Alias(r) => {
                    alias_to.insert(stem, r);
                }
                ViewDef::Derived(..) => derived_names.push(stem),
                ViewDef::Fork(_) => fork_names.push(stem),
            }
        }
    }
    // A requested atomic retirement: verify the old name is in fact an alias
    // of THIS real view (never silently drop a derived definition or a
    // different view's alias), then drop it from the working map so the
    // steady-state check below reflects the post-rotation state.
    if let Some(old) = &retire {
        match alias_to.get(*old) {
            Some(r) if r == real => {
                alias_to.remove(*old);
            }
            Some(r) => {
                return Err(format!(
                    "--retire '{old}' currently aliases a different real view ('{r}'), not '{real}' — rotation must retire the previous name of the same view"
                ));
            }
            None => {
                return Err(format!(
                    "--retire '{old}' is not a pure alias here (a derived view is separate — `grovefs view list` shows what '{old}' actually is)"
                ));
            }
        }
    }
    if alias_to.contains_key(real) {
        return Err(format!(
            "alias target '{real}' is itself an alias (-> '{}') — chains are not supported; alias the physical view instead",
            alias_to[real]
        ));
    }
    if derived_names.iter().any(|d| d == real) {
        return Err(format!(
            "alias target '{real}' is a derived (virtual) view — aliases map onto a physical view only"
        ));
    }
    if fork_names.iter().any(|f| f == real) {
        return Err(format!(
            "alias target '{real}' is a fork (an independent metadata tree) — aliases map onto a physical view only; give the fork its own key binding (`apikey create --view {real}`) instead"
        ));
    }
    // The target must be a PHYSICAL view that ACTUALLY EXISTS, not just a name
    // that is none of the governed kinds. A previous gap let `alias <missing>`
    // be recorded and only fail at mount (§5-10): reject it here so both
    // entrances (CLI and MCP) name the create/import command before anything
    // is written. The kind checks above already handled alias/derived/fork
    // targets, so reaching here with no physical tree means a dangling name.
    if !store.has_physical(real)? {
        let hint = store
            .base_tree_missing_note(real)?
            // The store's pre-flight note is phrased for a governance OUT
            // (`note: no local tree … first`); drop that leading tag so the
            // concrete path/command reads as one sentence inside the refusal.
            .map(|n| format!(": {}", n.strip_prefix("note: ").unwrap_or(&n)))
            .unwrap_or_else(|| {
                format!(
                    ": create the physical view first (`grovefs view create --view {real}` against this store, or `grovefs --home <dir> init --view {real}`, or seed it with `grovefs view import`), then define the alias"
                )
            });
        return Err(format!(
            "alias target '{real}' has no physical view tree in {desc}{hint}",
            desc = store.describe()
        ));
    }
    if let Some(existing) = alias_to.iter().find(|(_, r)| **r == real) {
        if existing.0 != name {
            return Err(format!(
                "real view '{real}' is already governed by alias '{}' — one public name per real view; to rotate it gaplessly use `grovefs view alias --name {name} --real {real} --retire {}`",
                existing.0, existing.0
            ));
        }
    }
    let mut out = Vec::new();
    // Overwriting an existing definition slot is a governance replacement:
    // surface it as a note so both entrances report it identically.
    if store.has_def(name)? {
        out.push(OutLine::Note(format!(
            "note: view '{name}' already defined — replacing it with the alias (live mounts re-resolve at their next M_OPEN)"
        )));
    }
    // Write the new alias BEFORE removing the retired one: the real view is
    // governed by at least one name at every instant, so a leaked old name
    // never opens a window where the real name becomes mountable by itself.
    store.put_def(name, &format!("alias {real}\n"))?;
    if let Some(old) = &retire {
        let _ = store.remove_def(old);
        out.push(OutLine::Info(format!(
            "rotated public name for real view '{real}': '{old}' -> '{name}' (gapless — '{real}' stayed governed throughout, '{old}' no longer resolves)"
        )));
    } else {
        out.push(OutLine::Info(format!(
            "defined pure alias '{name}' -> real view '{real}'"
        )));
    }
    out.push(OutLine::Note(format!(
        "note: '{real}' is no longer a mount entry point while this definition exists (mounts by its real name are refused, locally and via meta-server); alias mounts show the FULL tree and are WRITABLE"
    )));
    if let Some(note) = store.base_tree_missing_note(real)? {
        out.push(OutLine::Note(note));
    }
    Ok(out)
}

/// Create an INDEPENDENT metadata tree `name` seeded from the physical view
/// `base`: a FORK. Content bytes are shared (ledger-booked), so the fork costs
/// metadata only; structure (mkdir/rename/unlink/symlink) is writable while
/// content writes are refused client-side with EROFS, and a delete removes
/// only the fork's own names — the base's tree stays exactly as it was.
/// Refused: a name that already has a definition or a physical db, a governed
/// base (fork a physical tree, not a lens/alias/fork), and any store whose
/// base tree is not a local SQLite db (PostgreSQL-backed views cannot be
/// forked yet — the authoritative tree lives in Postgres, not in a file).
pub fn fork_view(store: &dyn ControlStore, name: &str, base: &str) -> Result<Vec<OutLine>, String> {
    fork_view_seeded(store, name, base, true)
}

/// [`fork_view`] with the seeding choice spelled out: `seed == false` records
/// the same `fork <base>` definition but leaves the tree EMPTY (root only), to
/// be filled by [`graft_view`]. A fork's mount semantics do not depend on how
/// many names it started with.
pub fn fork_view_seeded(
    store: &dyn ControlStore,
    name: &str,
    base: &str,
    seed: bool,
) -> Result<Vec<OutLine>, String> {
    if !valid_view_name(name) {
        return Err(format!(
            "invalid fork name '{name}' (must be one safe path element)"
        ));
    }
    if !valid_view_name(base) {
        return Err(format!("invalid base view name '{base}'"));
    }
    if name == base {
        return Err("fork name must differ from its base".into());
    }
    if store.has_def(name)? {
        return Err(format!(
            "view '{name}' already has a definition — remove it first (`grovefs view rm --name {name}`) before forking"
        ));
    }
    if store.has_physical(name)? {
        return Err(format!(
            "'{name}' already names a physical view db — pick another name"
        ));
    }
    if store.has_def(base)? {
        return Err(format!(
            "base '{base}' is governed by a definition (alias/derived/fork) — fork the physical base tree instead"
        ));
    }
    if !store.has_physical(base)? {
        return Err(format!(
            "no physical base tree for '{base}' in {} — `fork` copies a LOCAL SQLite tree; create the base first (`grovefs view create --view {base}`). A PostgreSQL-backed view cannot be forked (its authoritative tree lives in Postgres, not in a file)",
            store.describe()
        ));
    }
    let stats = store.create_fork_tree(base, name, seed)?;
    // Record the definition only AFTER the tree exists, so a crash never leaves
    // a name that resolves to a missing tree. A failed write rolls the fresh
    // tree back (best effort) rather than leaving an unreferenced tree behind.
    if let Err(e) = store.put_def(name, &format!("fork {base}\n")) {
        store.destroy_fork_tree(name);
        return Err(format!("cannot record fork definition for '{name}': {e}"));
    }
    if !seed {
        // An EMPTY fork: root only. The natural next step is `view graft`, so
        // say so instead of claiming "0 shared objects" as if it had copied.
        return Ok(vec![
            OutLine::Info(format!(
                "forked view '{name}' from base '{base}' as an EMPTY tree: root only, nothing copied"
            )),
            OutLine::Note(format!(
                "note: place the subset you want with `grovefs view graft --name {name} --src {base} --map plan.tsv` (or the `graft_view` tool, same implementation) — a directory source brings its subtree; each destination basename is the rename"
            )),
            OutLine::Note(format!(
                "note: '{name}' has the same fork semantics — structure writable, content writes (write/truncate) refused with EROFS, bytes shared through the cross-view ledger"
            )),
        ]);
    }
    Ok(vec![
        OutLine::Info(format!(
            "forked view '{name}' from base '{base}': {} entries, {} shared object(s)",
            stats.nodes, stats.blobs
        )),
        OutLine::Note(format!(
            "note: '{name}' is an independent metadata tree — mkdir/rename/unlink are writable, content writes (write/truncate) are refused with EROFS; deletes remove only this view's names and never touch '{base}'"
        )),
        OutLine::Note(format!(
            "note: content bytes are SHARED with '{base}' (cross-view ledger): the object is freed only when no view holds a name for it anymore"
        )),
    ])
}

/// The tree a graft may land in or read out of: a base view or a fork (both own
/// a node table). Lens/alias names render or forward another tree, so they are
/// refused by role with the name of the tree that actually holds the nodes.
fn own_tree(store: &dyn ControlStore, name: &str, role: &str) -> Result<Option<ViewDef>, String> {
    match store.get_def(name)? {
        Some(def) => match def {
            fork @ ViewDef::Fork(_) => Ok(Some(fork)),
            ViewDef::Alias(real) => Err(format!(
                "{role} '{name}' is an alias of '{real}' — use '{real}' (an alias has no tree of its own)"
            )),
            ViewDef::Derived(_, _) => Err(format!(
                "{role} '{name}' is a derived lens: it renders another tree read-only and owns no nodes — use a base view or a fork"
            )),
        },
        None => {
            if store.has_physical(name)? {
                Ok(None)
            } else {
                Err(format!(
                    "no tree for {role} '{name}' in {} — create it first (`grovefs view create --view {name}` for a base view, or `grovefs view fork --name {name} --base <base> --empty` / `fork_view` with `seed:false` for an empty fork)",
                    store.describe()
                ))
            }
        }
    }
}

/// Graft a curated subset of `src`'s tree into `name` (a base view or a fork).
/// `src == None` means "the fork's recorded base", which is the common case and
/// keeps a curation script free of the base's name. `plan` is the plan text
/// itself (one `<src>[TAB<dst>]` per line), so a CLI caller can pipe a file or
/// stdin straight through and an MCP caller can hand it inline.
pub fn graft_view(
    store: &dyn ControlStore,
    name: &str,
    src: Option<&str>,
    plan: &str,
    dest: &str,
    skip_existing: bool,
) -> Result<Vec<OutLine>, String> {
    if !valid_view_name(name) {
        return Err(format!(
            "invalid destination name '{name}' (must be one safe path element)"
        ));
    }
    let dst_def = own_tree(store, name, "graft destination")?;
    let src_name = match src {
        Some(s) => s.to_string(),
        None => match &dst_def {
            Some(ViewDef::Fork(b)) => b.clone(),
            _ => {
                return Err(format!(
                    "'{name}' is a base view — pass --src to name the view its entries come from"
                ))
            }
        },
    };
    if !valid_view_name(&src_name) {
        return Err(format!("invalid source view name '{src_name}'"));
    }
    if src_name == name {
        return Err("a view cannot graft from its own tree — pick another source".into());
    }
    own_tree(store, &src_name, "graft source")?;
    let items = crate::meta::parse_graft_plan(plan)?;
    if items.is_empty() {
        return Err(
            "empty graft plan — one '<src>' or '<src>TAB<dst>' line per entry (blank lines and '#' comments are ignored)"
                .to_string(),
        );
    }
    let st = store.graft_tree(name, &src_name, &items, dest, skip_existing)?;
    let where_to = if dest.trim().is_empty() {
        "/".to_string()
    } else {
        format!("/{}", dest.trim().trim_matches('/'))
    };
    let mut out = vec![
        OutLine::Info(format!(
            "grafted {} entr{} ({} director{}) from '{src_name}' into '{name}' at {where_to}: {} shared object(s), {} skipped",
            st.nodes,
            if st.nodes == 1 { "y" } else { "ies" },
            st.dirs,
            if st.dirs == 1 { "y" } else { "ies" },
            st.blobs,
            st.skipped
        )),
        OutLine::Note(
            "note: bytes were not copied — every grafted name holds the same object as its source; the object is freed only when no view names it anymore"
                .to_string(),
        ),
    ];
    // Only a fork refuses content writes; a base destination keeps its own
    // writability, so claiming EROFS there would be false.
    out.push(OutLine::Note(match &dst_def {
        Some(ViewDef::Fork(_)) => "note: content stays read-only in a fork (EROFS); re-running the same plan with --skip-existing merges into what is already there".to_string(),
        _ => "note: the destination is a base view — content stays writable there; re-running the same plan with --skip-existing merges into what is already there".to_string(),
    }));
    Ok(out)
}

/// One human-readable line per definition, byte-identical to `grovefs view list`.
fn entry_line(e: &ViewEntry) -> String {
    match e.kind {
        "alias" => format!(
            "alias '{}' -> real view '{}' (full tree, writable; '{}' not mountable by its own name)",
            e.name, e.real_view, e.real_view
        ),
        "fork" => format!(
            "fork '{}' of base '{}' (independent tree: structure writable, content refused EROFS; deletes touch only this view)",
            e.name, e.real_view
        ),
        _ => format!(
            "view '{}' -> base '{}': {} hide, {} only, {} reloc rule(s) (read-only)",
            e.name, e.real_view, e.hide, e.only, e.reloc
        ),
    }
}

/// Enumerate the store: every physical view tree plus every governed
/// definition (derived or alias), resolved from the store's own records. A
/// malformed definition is an error, exactly as on the CLI (never silently
/// hidden from the agent).
pub fn list_views(store: &dyn ControlStore) -> Result<StoreViews, String> {
    let mut names: Vec<String> = Vec::new();
    for (stem, _) in store.list_defs()? {
        names.push(stem);
    }
    names.sort();
    let mut defined: Vec<ViewEntry> = Vec::new();
    for n in names {
        let d = store.get_def(&n)?.ok_or_else(|| {
            format!("bad view definition for '{n}': definition vanished mid-list")
        })?;
        defined.push(entry_from_def(&n, &d));
    }
    let physical = store.list_physical()?;
    Ok(StoreViews { physical, defined })
}

/// CLI-facing rendering of [`list_views`] (byte-identical to the old inline
/// `ViewAction::List` output, plus the BARE base views that carry no definition
/// of their own — the tree a fork or a lens is seeded from — so one command
/// answers "what views exist here, and what is each one?").
pub fn list_views_text(store: &dyn ControlStore) -> Result<Vec<OutLine>, String> {
    let s = list_views(store)?;
    // Bare base views: physical trees with NO definition slot. A defined name
    // is reported below (a fork's own db is its tree, not a separate base).
    // Computed first so the "no definitions" note can't contradict the base
    // view lines that immediately follow it (§6-1).
    let bare: Vec<&String> = s
        .physical
        .iter()
        .filter(|p| !s.defined.iter().any(|e| &e.name == *p))
        .collect();
    let mut out = Vec::new();
    if s.defined.is_empty() {
        out.push(OutLine::Info(if bare.is_empty() {
            format!(
                "no view definitions in the control plane store {} (every view name maps to its own tree)",
                store.describe()
            )
        } else {
            format!(
                "no alias/fork/lens definitions in {} — the {} base view(s) below each map to their own tree",
                store.describe(),
                bare.len()
            )
        }));
    }
    for e in &s.defined {
        out.push(OutLine::Info(entry_line(e)));
    }
    for p in bare {
        out.push(OutLine::Info(format!(
            "base view '{p}' -> own tree meta/{p}.db (read-write; deletes free a shared object only when no view holds it)"
        )));
    }
    Ok(out)
}

/// Remove a view DEFINITION (derived, alias or fork — the file slot is
/// shared). For a derived view or an alias the underlying data is untouched;
/// already-mounted clients keep their resolved lens until they remount (or the
/// server cache evicts it). For a FORK the definition owns an independent
/// metadata tree, so removing it also deletes `meta/<name>.db` (+ WAL/SHM) and
/// rebuilds the cross-view ledger from the surviving trees — content bytes the
/// base still holds stay, bytes no view holds anymore become freeable.
pub fn remove_view(store: &dyn ControlStore, name: &str) -> Result<Vec<OutLine>, String> {
    if !valid_view_name(name) {
        return Err(format!("invalid view name '{name}'"));
    }
    let def = store
        .get_def(name)?
        .ok_or_else(|| format!("no view definition '{name}' in {}", store.describe()))?;
    store.remove_def(name)?;
    if let ViewDef::Fork(base) = def {
        store.drop_fork_tree(name)?;
        return Ok(vec![
            OutLine::Info(format!(
                "removed fork view '{name}' of base '{base}' (its own metadata tree is gone; content bytes stay while any view — including '{base}' — still holds a name for them)"
            )),
            OutLine::Note(
                "note: a live mount of a removed fork keeps its already-resolved names until it remounts; new mounts are refused".to_string(),
            ),
        ]);
    }
    Ok(vec![OutLine::Info(format!(
        "removed view definition '{name}' (underlying data untouched)"
    ))])
}

/// Parse rules text without writing anything — a dry-run so an agent can
/// self-check a `hide/only/map/link` program before publishing it as a view.
pub fn preview_rules(rules_text: &str) -> Result<Vec<OutLine>, String> {
    let r = LayoutRules::parse(rules_text).map_err(|e| format!("bad layout rules: {e}"))?;
    let mut out = vec![OutLine::Info(format!(
        "rules OK: {} hide, {} only, {} reloc rule(s)",
        r.hide.len(),
        r.only.len(),
        r.reloc.len()
    ))];
    for h in &r.hide {
        out.push(OutLine::Info(format!("hide {h}")));
    }
    for o in &r.only {
        out.push(OutLine::Info(format!("only {o}")));
    }
    for rc in &r.reloc {
        out.push(OutLine::Info(format!(
            "{} {} {}",
            if rc.keep_src { "link" } else { "map" },
            rc.from,
            rc.to
        )));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Read-only introspection (the discovery half of "customize a data listing")
// ---------------------------------------------------------------------------

/// How deep / how many entries a metadata walk renders by default. These bound
/// the cost of a `list_view_tree` call on a large tree; the agent can raise
/// `depth`/`max` per call, and a truncated walk says so honestly.
const DEFAULT_DEPTH: usize = 6;
const DEFAULT_MAX_ENTRIES: usize = 800;

/// One rendered entry of a view listing: an absolute DISPLAY path (exactly the
/// string an agent feeds back into a `hide`/`only`/`map` rule), its kind, and
/// the `size` field of its metadata node. Because the walk is metadata-only (it
/// never asks the data backend for object bytes — that is what a mount's
/// `getattr` does), this is NOT a file's byte count: `dir` entries carry the
/// stat placeholder 4096, `file` entries carry 0, and `link` entries carry the
/// length of their target text. Treat `size` as "this tool does not measure
/// content"; take `du`/`stat` through a mount when you need real bytes.
#[derive(Debug)]
pub struct TreeEntry {
    pub path: String,
    pub kind: &'static str,
    /// Metadata `size` field: 4096 for a dir, 0 for a file, target length for a
    /// symlink. Never the object's byte count (see the type-level note).
    pub size: u64,
}

/// The rendered listing of a view, resolved through the SAME lens a mount uses
/// (`LayoutMeta`), so an agent sees the shape it is about to publish rather
/// than guessing paths.
#[derive(Debug)]
pub struct ShapeTree {
    /// "physical" | "derived" | "alias" | "simulated" (an uncommitted lens).
    pub kind: &'static str,
    /// The physical view whose metadata was actually walked.
    pub rendered_from: String,
    /// The display subtree the walk started at (`/` unless `path` was given).
    pub root_path: String,
    pub entries: Vec<TreeEntry>,
    /// True when `max`/`depth` cut the walk short.
    pub truncated: bool,
}

/// What to render for [`view_tree`]: either an EXISTING view name (physical,
/// derived or alias — resolved exactly like a mount would) or a `base` + inline
/// `rules` pair simulating a not-yet-published lens.
pub enum TreeTarget<'a> {
    Existing(&'a str),
    Simulate { base: &'a str, rules: &'a str },
}

/// Open a view's tree READ-ONLY through the store's own backend (local SQLite
/// db, or the PG schema the control plane is bound to). A missing tree is
/// reported honestly ([`ControlStore::open_ro_view`]) — this control-plane tool
/// never mounts and reads no data bytes.
fn open_meta_ro(store: &dyn ControlStore, view: &str) -> Result<Arc<dyn MetaProvider>, String> {
    store.open_ro_view(view)
}

/// Depth-first render of `provider`'s display tree under `ino`, appending
/// absolute display paths to `out`. `readdir` already sorts by name, so the
/// listing matches what a mount shows; synthesized scaffolding dirs carry a
/// virtual ino that `LayoutMeta::readdir` resolves on the next recursion.
#[allow(clippy::too_many_arguments)] // recursive walk: read-only context passed through
fn walk_tree(
    provider: &dyn MetaProvider,
    ino: i64,
    path: &str,
    depth: usize,
    max_depth: usize,
    max: usize,
    out: &mut Vec<TreeEntry>,
    truncated: &mut bool,
) {
    for n in provider.readdir(ino) {
        if out.len() >= max {
            *truncated = true;
            return;
        }
        let child = if path == "/" {
            format!("/{}", n.name)
        } else {
            format!("{}/{}", path, n.name)
        };
        let kind = match n.kind {
            NodeKind::Dir => "dir",
            NodeKind::Regular => "file",
            NodeKind::Symlink => "link",
        };
        out.push(TreeEntry {
            path: child.clone(),
            kind,
            size: n.size,
        });
        if n.is_dir() && depth < max_depth {
            walk_tree(
                provider,
                n.id,
                &child,
                depth + 1,
                max_depth,
                max,
                out,
                truncated,
            );
            if *truncated {
                return;
            }
        }
    }
}

/// Render the listing a mount of the target would show — metadata only, no
/// data IO, no mount. See [`TreeTarget`] for the two ways to ask.
pub fn view_tree(
    store: &dyn ControlStore,
    target: TreeTarget,
    path: Option<&str>,
    depth: usize,
    max: usize,
) -> Result<ShapeTree, String> {
    let max_depth = if depth == 0 { DEFAULT_DEPTH } else { depth };
    let cap = if max == 0 { DEFAULT_MAX_ENTRIES } else { max };
    let root_path = match path {
        Some(p) if !p.trim().is_empty() => canon(p)?,
        _ => "/".to_string(),
    };
    let (provider, kind, rendered_from): (Arc<dyn MetaProvider>, &'static str, String) =
        match target {
            TreeTarget::Existing(view) => {
                if !valid_view_name(view) {
                    return Err(format!("invalid view name '{view}'"));
                }
                match store.get_def(view)? {
                    Some(def) => match def {
                        ViewDef::Derived(base, rules) => {
                            let eng = open_meta_ro(store, &base)?;
                            let lens: Arc<dyn MetaProvider> = Arc::new(LayoutMeta::new(eng, rules));
                            (lens, "derived", base)
                        }
                        ViewDef::Alias(real) => (open_meta_ro(store, &real)?, "alias", real),
                        ViewDef::Fork(_) => (open_meta_ro(store, view)?, "fork", view.to_string()),
                    },
                    None => (open_meta_ro(store, view)?, "physical", view.to_string()),
                }
            }
            TreeTarget::Simulate { base, rules } => {
                if !valid_view_name(base) {
                    return Err(format!("invalid base view name '{base}'"));
                }
                let r = LayoutRules::parse(rules).map_err(|e| format!("bad layout rules: {e}"))?;
                let eng = open_meta_ro(store, base)?;
                let lens: Arc<dyn MetaProvider> = Arc::new(LayoutMeta::new(eng, r));
                (lens, "simulated", base.to_string())
            }
        };
    // Resolve the display subtree to start from, walking the lens by path so
    // synthesized dirs are allocated exactly as a mount would.
    let start_ino = if root_path == "/" {
        ROOT_INO
    } else {
        let mut cur = ROOT_INO;
        for c in root_path.split('/').filter(|s| !s.is_empty()) {
            cur = provider
                .lookup(cur, c)
                .ok_or_else(|| {
                    format!("no such path '{root_path}' in the rendered '{rendered_from}' view")
                })?
                .id;
        }
        cur
    };
    let mut entries = Vec::new();
    let mut truncated = false;
    walk_tree(
        provider.as_ref(),
        start_ino,
        &root_path,
        0,
        max_depth,
        cap,
        &mut entries,
        &mut truncated,
    );
    Ok(ShapeTree {
        kind,
        rendered_from,
        root_path,
        entries,
        truncated,
    })
}

/// Full detail of ONE view slot for read-modify-write editing: what `list_views`
/// summarizes, plus the verbatim definition text (derived rules or the alias
/// target) and the mount semantics — never hiding a malformed definition.
pub fn describe_view(store: &dyn ControlStore, name: &str) -> Result<Value, String> {
    if !valid_view_name(name) {
        return Err(format!("invalid view name '{name}'"));
    }
    if let Some(text) = store.get_def_raw(name)? {
        return Ok(match store.get_def(name)? {
            Some(ViewDef::Alias(real)) => json!({
                "name": name, "kind": "alias", "real_view": real,
                "backend": store.backend(),
                "mount": "read-write (full tree under the public name)",
                "real_name_mountable": false,
                "definition": text,
            }),
            Some(ViewDef::Derived(base, r)) => json!({
                "name": name, "kind": "derived", "base": base,
                "backend": store.backend(),
                "mount": "read-only (virtual shape; writes go to the base view)",
                "hide": r.hide.len(), "only": r.only.len(), "reloc": r.reloc.len(),
                "definition": text,
            }),
            Some(ViewDef::Fork(base)) => json!({
                "name": name, "kind": "fork", "base": base,
                "backend": store.backend(),
                "mount": "structure read-write / content read-only (independent tree; deletes touch only this view)",
                "own_tree": format!("meta/{name}.db"),
                "definition": text,
            }),
            None => {
                return Err(format!(
                    "bad view definition for '{name}': definition vanished mid-read"
                ))
            }
        });
    }
    if store.has_physical(name)? {
        return Ok(json!({
            "name": name, "kind": "physical",
            "backend": store.backend(),
            "mount": "read-write (this is a storage identity, not a governed definition)",
        }));
    }
    Err(format!(
        "no view '{name}' in {} (not a definition nor a physical view tree) — `list_views` names what exists",
        store.describe()
    ))
}

// ---------------------------------------------------------------------------
// MCP *resources*, *prompts* and *completion* over the same governance renderers
// ---------------------------------------------------------------------------

/// Canonical URI scheme exposing governed views as MCP resources, so an agent's
/// client can attach a view's definition or rendered tree as context WITHOUT a
/// tool call. `grovefs://view/<name>/definition` (verbatim governance text) and
/// `grovefs://view/<name>/tree` (the listing a mount actually shows). Both reuse
/// the exact renderers the tools use, so a resource and a tool call never
/// disagree about what a view is.
pub const VIEW_URI_PREFIX: &str = "grovefs://view/";

/// Canonical URIs of the two STORE-WIDE resources: the server's operating
/// manual and one whole-store snapshot — attachable context that does not name
/// a view, so a client can load the map before drilling into it.
pub const VIEWS_URI: &str = "grovefs://views";
pub const GUIDE_URI: &str = "grovefs://guide";

/// The resource templates advertised (fixed shape, `{name}` filled per view).
/// The tree template carries RFC 6570 query parameters, so a client can attach
/// a DRILLED-DOWN listing (`?path=/photos&depth=2&max=50`) instead of always
/// paying for the default depth-6 / 800-entry render.
pub fn resource_templates() -> Value {
    json!([
        { "uriTemplate": "grovefs://view/{name}/definition",
          "name": "view-definition",
          "description": "Verbatim governance definition of one view slot (derived rules or the alias target); a physical view reports its mount semantics instead.",
          "mimeType": "application/json" },
        { "uriTemplate": "grovefs://view/{name}/tree{?path,depth,max}",
          "name": "view-tree",
          "description": "The directory listing a mount of this view actually shows, rendered through its lens (metadata only — no mount, no data bytes; `size` is the metadata field, so dirs read 4096 and files read 0). Optional query parameters drill down: `path` (display subtree, default /), `depth` (default 6), `max` (default 800).",
          "mimeType": "application/json" },
    ])
}

/// The whole-store snapshot the `list_views` tool and the `grovefs://views`
/// resource both answer with — one builder, so the two can never disagree.
pub fn views_snapshot(store: &dyn ControlStore) -> Result<Value, String> {
    let sv = list_views(store)?;
    let defined: Vec<Value> = sv
        .defined
        .iter()
        .map(|e| {
            json!({
                "name": e.name, "kind": e.kind, "real_view": e.real_view,
                "hide": e.hide, "only": e.only, "reloc": e.reloc,
                "mount": match e.kind {
                    "alias" => "read-write (alias)",
                    "fork" => "structure read-write / content read-only (fork)",
                    _ => "read-only (derived)",
                },
                "line": entry_line(e),
            })
        })
        .collect();
    Ok(json!({
        "backend": store.backend(),
        "store": store.describe(),
        "physical": sv.physical,
        "defined": defined,
    }))
}

/// The JSON shape of a rendered tree, shared by the `list_view_tree` tool and
/// the `.../tree` resource (so a tool call and a resource read are byte-equal).
pub fn tree_json(t: &ShapeTree) -> Value {
    let dir_count = t.entries.iter().filter(|e| e.kind == "dir").count();
    let entries: Vec<Value> = t
        .entries
        .iter()
        .map(|e| json!({ "path": e.path, "kind": e.kind, "size": e.size }))
        .collect();
    json!({
        "kind": t.kind,
        "rendered_from": t.rendered_from,
        "root_path": t.root_path,
        "truncated": t.truncated,
        "dir_count": dir_count,
        "file_count": t.entries.len() - dir_count,
        "entries": entries,
    })
}

/// `grovefs://guide` — the operating manual as markdown, for clients that
/// attach resources as context instead of reading `initialize.instructions`.
pub fn guide_text() -> String {
    format!(
        "# GroveFS MCP guide\n\n{INSTRUCTIONS}\n\n## Resources\n- `{GUIDE_URI}` — this guide (text/markdown)\n- `{VIEWS_URI}` — whole-store snapshot (JSON, same as `list_views`)\n- `grovefs://view/<name>/definition` — one slot's verbatim definition\n- `grovefs://view/<name>/tree?path=&depth=&max=` — its rendered listing, optionally drilled down\n"
    )
}

/// Enumerate every view slot in the store as MCP resources (physical → tree;
/// derived/alias → definition + tree), preceded by the two store-wide ones.
pub fn list_resources(store: &dyn ControlStore) -> Result<Value, String> {
    let s = list_views(store)?;
    let mut resources: Vec<Value> = vec![
        json!({
            "uri": GUIDE_URI,
            "name": "guide",
            "description": "Operating manual: tool surface, golden order, resources, protocol mechanics",
            "mimeType": "text/markdown",
        }),
        json!({
            "uri": VIEWS_URI,
            "name": "views (snapshot)",
            "description": "Whole-store snapshot: backend, store root, physical trees, governed definitions",
            "mimeType": "application/json",
        }),
    ];
    for p in &s.physical {
        resources.push(json!({
            "uri": format!("{VIEW_URI_PREFIX}{p}/tree"),
            "name": format!("{p} (tree)"),
            "description": "physical view tree (a storage identity, mountable read-write)",
            "mimeType": "application/json",
        }));
    }
    for e in &s.defined {
        resources.push(json!({
            "uri": format!("{VIEW_URI_PREFIX}{}/definition", e.name),
            "name": format!("{} (definition)", e.name),
            "description": format!("{} view over '{}'", e.kind, e.real_view),
            "mimeType": "application/json",
        }));
        resources.push(json!({
            "uri": format!("{VIEW_URI_PREFIX}{}/tree", e.name),
            "name": format!("{} (tree)", e.name),
            "description": format!("rendered listing of the {} view '{}'", e.kind, e.name),
            "mimeType": "application/json",
        }));
    }
    Ok(json!({ "resources": resources }))
}

/// Percent-decode one URI query value (`%20`, `%2F`, …); `+` stays literal so
/// a display path containing it is not silently rewritten.
fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = std::str::from_utf8(&b[i + 1..i + 3]).ok();
            if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// One query parameter of a resource URI (`...tree?path=/docs&depth=2`).
fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v)
}

/// Parse an optional non-negative integer query parameter.
fn query_usize(uri: &str, query: &str, key: &str) -> Result<usize, String> {
    match query_param(query, key) {
        None => Ok(0),
        Some(v) => pct_decode(v)
            .parse::<usize>()
            .map_err(|_| format!("resource uri '{uri}': '{key}' must be a non-negative integer")),
    }
}

/// Read one resource by URI, delegating to `describe_view` / `view_tree` so the
/// bytes match the corresponding tool result exactly. The tree facet accepts
/// `path`/`depth`/`max` query parameters — the resource-side equivalent of the
/// `list_view_tree` arguments, so a client can drill into a big tree.
pub fn read_resource(store: &dyn ControlStore, uri: &str) -> Result<Value, String> {
    if uri == GUIDE_URI {
        return Ok(json!({ "contents": [
            { "uri": uri, "mimeType": "text/markdown", "text": guide_text() }
        ] }));
    }
    if uri == VIEWS_URI {
        let text = serde_json::to_string_pretty(&views_snapshot(store)?).unwrap_or_default();
        return Ok(json!({ "contents": [
            { "uri": uri, "mimeType": "application/json", "text": text }
        ] }));
    }
    let rest = uri.strip_prefix(VIEW_URI_PREFIX).ok_or_else(|| {
        format!(
            "unsupported resource uri '{uri}' (expected {VIEW_URI_PREFIX}<name>/(definition|tree), {VIEWS_URI} or {GUIDE_URI})"
        )
    })?;
    // Split the query off FIRST: `<name>` and the facet live in the path part.
    let (path_part, query) = match rest.split_once('?') {
        Some((p, q)) => (p, q),
        None => (rest, ""),
    };
    let (name, facet) = path_part.rsplit_once('/').ok_or_else(|| {
        format!("malformed resource uri '{uri}' (expected .../<name>/(definition|tree))")
    })?;
    if name.is_empty() {
        return Err(format!("resource uri '{uri}' names no view"));
    }
    let text = match facet {
        "definition" => {
            serde_json::to_string_pretty(&describe_view(store, name)?).unwrap_or_default()
        }
        "tree" => {
            let path = query_param(query, "path").map(pct_decode);
            let depth = query_usize(uri, query, "depth")?;
            let max = query_usize(uri, query, "max")?;
            let t = view_tree(
                store,
                TreeTarget::Existing(name),
                path.as_deref(),
                depth,
                max,
            )?;
            serde_json::to_string_pretty(&tree_json(&t)).unwrap_or_default()
        }
        other => {
            return Err(format!(
                "unknown resource facet '{other}' (use 'definition' or 'tree')"
            ))
        }
    };
    Ok(json!({ "contents": [ { "uri": uri, "mimeType": "application/json", "text": text } ] }))
}

/// Governance playbooks exposed as MCP prompts — reusable, argument-templated
/// multi-step procedures that read as an agent's operating manual for a task.
pub fn prompt_defs() -> Value {
    json!([
        {
            "name": "reshape-view",
            "description": "Playbook for giving a new read-only shape to the physical view {base}: explore real paths, draft hide/only/map/link rules, dry-run validate, then publish.",
            "arguments": [
                { "name": "base", "description": "existing physical view to shape", "required": true },
                { "name": "goal", "description": "plain-language intent for the new shape", "required": false }
            ]
        },
        {
            "name": "rotate-alias",
            "description": "Playbook for gaplessly retiring a leaked public alias {old} in favour of {new} over the real view {real}.",
            "arguments": [
                { "name": "real", "description": "the physical view behind the alias", "required": true },
                { "name": "new", "description": "the new public name", "required": true },
                { "name": "old", "description": "the public name being retired", "required": true }
            ]
        }
    ])
}

fn prompt_arg(args: &Value, name: &str) -> String {
    args.get(name)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// Instantiate one prompt with the supplied arguments into a single user message.
pub fn get_prompt(name: &str, arguments: &Value) -> Result<Value, String> {
    match name {
        "reshape-view" => {
            let base = prompt_arg(arguments, "base");
            if base.is_empty() {
                return Err("missing required argument 'base'".into());
            }
            let goal = prompt_arg(arguments, "goal");
            let goal_line = if goal.is_empty() {
                String::new()
            } else {
                format!("Goal: {goal}\n")
            };
            let text = format!(
"Reshape the read-only derived view over physical base '{base}' — all steps metadata-only (control plane, never a mount):\n{goal_line}1. list_view_tree {{\"view\":\"{base}\"}} — read the ACTUAL display paths; never guess.\n2. Draft inline rules using only hide/only/map/link over those absolute paths (a `base` line inside rules is rejected).\n3. validate_definition {{\"name\":\"<new>\",\"base\":\"{base}\",\"rules\":\"<draft>\"}} — dry-runs every governance guard, returns the exact refusal a real publish would give, writes nothing.\n4. define_derived_view {{\"name\":\"<new>\",\"base\":\"{base}\",\"rules\":\"<final>\"}} — publishes; mounts of <new> are read-only (writes go to '{base}').\n5. list_view_tree {{\"view\":\"<new>\"}} — confirm the rendered shape matches intent."
);
            Ok(json!({
                "description": format!("reshape a derived view over base '{base}'"),
                "messages": [ { "role": "user", "content": { "type": "text", "text": text } } ],
            }))
        }
        "rotate-alias" => {
            let real = prompt_arg(arguments, "real");
            let new = prompt_arg(arguments, "new");
            let old = prompt_arg(arguments, "old");
            if real.is_empty() || new.is_empty() || old.is_empty() {
                return Err("rotate-alias needs arguments 'real', 'new' and 'old'".into());
            }
            let text = format!(
"Gaplessly rotate the public alias for real view '{real}': replace '{old}' with '{new}' in ONE command so the storage name is never left mountable.\n1. alias_view {{\"name\":\"{new}\",\"real\":\"{real}\",\"retire\":\"{old}\"}} — write-before-delete: '{new}' resolves, '{old}' stops resolving, '{real}' stays governed (not directly mountable) throughout.\n2. list_views — confirm '{new}' -> '{real}' is present and '{old}' is gone.\n3. Any client still on '{old}' remounts under '{new}' (rotation only affects NEW mounts; established connections drain on close)."
);
            Ok(json!({
                "description": format!("rotate alias '{old}' -> '{new}' for '{real}'"),
                "messages": [ { "role": "user", "content": { "type": "text", "text": text } } ],
            }))
        }
        other => Err(format!("unknown prompt '{other}'")),
    }
}

/// Complete a prompt/resource argument: for view-name-ish arguments return the
/// existing names IN THE BOUND CONTROL-PLANE STORE matching the typed prefix
/// (physical trees and governed definitions alike, file or PG).
///
/// The `ref` is honored so the suggestion fits the role being asked about:
/// a prompt's `base`/`real` argument lists physical trees (the only legal
/// targets), `rotate-alias`' `new`/`old` list existing ALIASES, and a resource
/// URI's `name` lists every mountable name. Unknown refs still complete against
/// all names, so a hand-written client keeps working.
pub fn complete(store: &dyn ControlStore, params: &Value) -> Result<Value, String> {
    let argument = params
        .get("argument")
        .cloned()
        .unwrap_or(Value::Object(Map::new()));
    let name = argument.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let value = argument.get("value").and_then(|v| v.as_str()).unwrap_or("");
    let r = params.get("ref").cloned().unwrap_or(Value::Null);
    let ref_type = r.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let ref_name = r.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let wants_view = matches!(name, "name" | "base" | "view" | "real" | "new" | "old");
    if !wants_view {
        return Ok(json!({ "completion": { "values": [], "total": 0, "hasMore": false } }));
    }
    let s = list_views(store)?;
    let all = || -> Vec<String> {
        let mut n = s.physical.clone();
        n.extend(s.defined.iter().map(|e| e.name.clone()));
        n
    };
    let aliases = || -> Vec<String> {
        s.defined
            .iter()
            .filter(|e| e.kind == "alias")
            .map(|e| e.name.clone())
            .collect()
    };
    // Role-aware pools: only the names that can legally fill this argument.
    let pool: Vec<String> = match (ref_type, ref_name, name) {
        ("ref/prompt", "reshape-view", "base") => s.physical.clone(),
        ("ref/prompt", "rotate-alias", "real") => s.physical.clone(),
        ("ref/prompt", "rotate-alias", "new" | "old") => aliases(),
        _ => all(),
    };
    let lower = value.to_lowercase();
    let mut matched: Vec<String> = pool
        .into_iter()
        .filter(|n| n.to_lowercase().starts_with(&lower))
        .collect();
    matched.sort();
    matched.dedup();
    let total = matched.len();
    let values: Vec<String> = matched.into_iter().take(100).collect();
    let has_more = total > values.len();
    Ok(json!({ "completion": { "values": values, "total": total, "hasMore": has_more } }))
}

// ---------------------------------------------------------------------------
// The MCP server proper (stdio, newline-delimited JSON-RPC 2.0)
// ---------------------------------------------------------------------------

/// MCP tool annotations: a client uses these to decide what it may auto-approve
/// and how loudly to confirm. Every tool here is closed-world (it only touches
/// the bound store), so `openWorldHint` is always false.
fn annot(title: &str, read_only: bool, destructive: bool, idempotent: bool) -> Value {
    json!({
        "title": title,
        "readOnlyHint": read_only,
        "destructiveHint": destructive,
        "idempotentHint": idempotent,
        "openWorldHint": false,
    })
}

/// `outputSchema` of `list_views` / the `grovefs://views` resource.
fn views_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "backend": { "type": "string", "enum": ["sqlite", "postgres"] },
            "store": { "type": "string" },
            "physical": { "type": "array", "items": { "type": "string" } },
            "defined": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "kind": { "type": "string", "enum": ["derived", "alias", "fork"] },
                        "real_view": { "type": "string" },
                        "hide": { "type": "integer" },
                        "only": { "type": "integer" },
                        "reloc": { "type": "integer" },
                        "mount": { "type": "string" },
                        "line": { "type": "string" }
                    },
                    "required": ["name", "kind", "real_view"]
                }
            }
        },
        "required": ["backend", "store", "physical", "defined"]
    })
}

/// `outputSchema` of `list_view_tree` / the `.../tree` resource.
fn tree_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "kind": { "type": "string", "enum": ["physical", "derived", "alias", "fork", "simulated"] },
            "rendered_from": { "type": "string" },
            "root_path": { "type": "string" },
            "truncated": { "type": "boolean" },
            "dir_count": { "type": "integer" },
            "file_count": { "type": "integer" },
            "entries": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "kind": { "type": "string", "enum": ["dir", "file", "link"] },
                        "size": { "type": "integer" }
                    },
                    "required": ["path", "kind", "size"]
                }
            }
        },
        "required": ["kind", "rendered_from", "root_path", "truncated", "entries"]
    })
}

/// `outputSchema` of `get_view` — deliberately permissive: the fields differ by
/// view kind and `definition` is verbatim text, so extra keys are meaningful.
fn describe_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": { "type": "string" },
            "kind": { "type": "string", "enum": ["derived", "alias", "fork", "physical"] },
            "backend": { "type": "string" },
            "base": { "type": "string" },
            "real_view": { "type": "string" },
            "definition": { "type": "string" },
            "mount": { "type": "string" }
        },
        "required": ["name", "kind", "backend"]
    })
}

/// `outputSchema` of `validate_definition`'s accepted verdict.
fn validate_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "accepted": { "type": "boolean" },
            "dry_run": { "type": "boolean" },
            "kind": { "type": "string" },
            "name": { "type": "string" },
            "base": { "type": "string" },
            "backend": { "type": "string" },
            "hide": { "type": "integer" },
            "only": { "type": "integer" },
            "reloc": { "type": "integer" },
            "would_replace_existing_definition": { "type": "boolean" },
            "base_view_db_present": { "type": "boolean" },
            "mount": { "type": "string" },
            "note": { "type": "string" }
        },
        "required": ["accepted", "dry_run"]
    })
}

/// JSON-schema `inputSchema` block shared by the tools' `name`-style args.
fn str_prop(desc: &str) -> Value {
    json!({ "type": "string", "description": desc })
}

/// The nine exposed tools, in stable order, with self-describing schemas —
/// an agent reads these descriptions as its whole operating manual.
pub fn tool_defs() -> Value {
    json!([
        {
            "name": "list_views",
            "title": "List views",
            "annotations": annot("List views", true, false, true),
            "description": "List every view in the store: physical view trees (storage identities, mountable read-write) and governed definitions — derived views (read-only shaped lenses over a base view), pure aliases (writable public names for a real view; while an alias exists its real name is not a mount entry point) and forks (independent metadata trees seeded from a physical view: structure writable, content read-only, deletes touch only the fork). Answers as JSON: {backend, store, physical:[...], defined:[{name,kind,real_view,hide,only,reloc,mount,line}]} — `backend` is sqlite or postgres and `store` names the control-plane root the call read.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "db_path": str_prop("Metadata root holding meta/*.db and meta/views/*.layout; defaults to the server's --db-path/--home. Ignored when the server is bound to a `--store postgres://…` URI — the control plane is URI-bound there")
                }
            },
            "outputSchema": views_output_schema()
        },
        {
            "name": "define_derived_view",
            "title": "Define derived view",
            "annotations": annot("Define derived view", false, false, true),
            "description": "Create or replace a DERIVED view: a named, centrally-governed presentation lens over the physical base view, built from `rules` text (one verb per line, # comments allowed): `hide <src>` (prune a subtree), `only <src>` (serve just that subtree), `map <src> <dst>` (re-home entries to a virtual display path), `link <src> <dst>` (add a second name for the same entries). Mounts of the derived name are READ-ONLY — writes go to the base view and show up through the lens within the client cache TTL. Against a live meta-server, remounts are not needed: the server hot-reloads the definition and mounts converge within ≤2s. A `base` line inside rules is rejected (the base is the `base` argument).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": str_prop("Derived view name (one safe path element, must differ from base)"),
                    "base": str_prop("Physical view whose tree this lens displays"),
                    "rules": str_prop("Layout rules text, e.g. \"# only docs\\nonly /docs\""),
                    "db_path": str_prop("Metadata root; defaults to the server's --db-path/--home (ignored under a PG-bound server — one URI-bound control plane serves every slot)")
                },
                "required": ["name", "base", "rules"]
            }
        },
        {
            "name": "alias_view",
            "title": "Alias view",
            "annotations": annot("Alias view", false, true, false),
            "description": "Create or replace a PURE ALIAS: a public writable name for a physical view (name governance, not shape). The alias mounts the FULL tree read-write, and while it exists the real name is refused as a mount entry point — locally and via meta-server. Pass `retire` to rotate a previous alias of the SAME real view gaplessly (write-before-delete: the real name never becomes mountable). Chaining aliases and aliasing a derived view are rejected.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": str_prop("Public alias name (must differ from the real view name)"),
                    "real": str_prop("Physical view the alias maps onto"),
                    "retire": str_prop("Previous alias name of the same real view to retire atomically (optional)"),
                    "db_path": str_prop("Metadata root; defaults to the server's --db-path/--home (ignored under a PG-bound server — one URI-bound control plane serves every slot)")
                },
                "required": ["name", "real"]
            }
        },
        {
            "name": "fork_view",
            "title": "Fork view",
            "annotations": annot("Fork view", false, false, false),
            "description": "Create a FORK: an INDEPENDENT metadata tree named `name`, seeded as a point-in-time copy of the physical view `base` (`<db-path>/meta/<name>.db`). Content bytes are SHARED through the cross-view ledger (no copies), so the fork costs metadata only. Structure (mkdir/rename/unlink/symlink/link) is WRITABLE; content writes (write/truncate) are refused with EROFS client-side. Deleting a name in the fork removes only the fork's own metadata — the base tree is untouched, and a shared object is freed only when no view holds a name for it. With `seed: false` the fork's tree starts EMPTY (root only) so a curated subset can be placed with `graft_view`. Requires a local SQLite base tree (a PostgreSQL-bound control store refuses forks — its trees are PG schemas, not files).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": str_prop("Fork view name (one safe path element, must differ from base)"),
                    "base": str_prop("Physical base view whose tree seeds the fork"),
                    "seed": json!({"type":"boolean","description":"Copy the base's names into the fork (default true). false creates an EMPTY fork tree (root only) to fill with graft_view — nothing is copied, so a curated subset costs nothing extra."}),
                    "db_path": str_prop("Metadata root; defaults to the server's --db-path/--home (ignored under a PG-bound server — one URI-bound control plane serves every slot)")
                },
                "required": ["name", "base"]
            }
        },
        {
            "name": "graft_view",
            "title": "Graft view",
            "annotations": annot("Graft view", false, false, false),
            "description": "Copy a CURATED SUBSET of one view's names into `name` (a base view or a fork), renaming as it goes. `map` is a plan, one line per entry: `<src>` or `<src>TAB<dst>`, paths relative to each view's root; blank lines and `#` comments are ignored. A directory source brings its whole subtree (original names); a file/symlink brings just itself; the destination basename is the rename (this is how UUID-named objects become a human-shaped tree). Missing destination directories are created (0755), all entries land in ONE transaction (a bad line writes nothing), and bytes are SHARED via the cross-view ledger — a graft moves metadata, never content. Typical flow: fork_view with `seed: false`, then graft_view the subset you want. `src` defaults to the fork's recorded base.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": str_prop("Destination view: a BASE view or a fork (a lens/alias owns no tree and is refused)"),
                    "src": str_prop("Source view whose names are copied (default: the fork's recorded base)"),
                    "map": str_prop("Graft plan text: one '<src>' or '<src>TAB<dst>' per line, '#' comments allowed"),
                    "dest": str_prop("Directory inside `name` to place everything under (default '/')"),
                    "skip_existing": json!({"type":"boolean","description":"Leave a conflicting destination entry alone and keep going (merge into an existing directory; skip colliding files). Default false = any conflict refuses the whole plan."}),
                    "db_path": str_prop("Metadata root; defaults to the server's --db-path/--home (ignored under a PG-bound server — one URI-bound control plane serves every slot)")
                },
                "required": ["name", "map"]
            }
        },
        {
            "name": "remove_view",
            "title": "Remove view",
            "annotations": annot("Remove view", false, true, true),
            "description": "Remove a view DEFINITION by name (derived, alias or fork — they share a slot). For derived/alias the underlying data is untouched; already-mounted clients keep their resolved lens until remount, and removing an alias makes the real name mountable again for NEW mounts. For a fork this also deletes its independent metadata tree and rebuilds the cross-view ledger (content bytes survive while any view still holds them).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": str_prop("Definition name to remove"),
                    "db_path": str_prop("Metadata root; defaults to the server's --db-path/--home (ignored under a PG-bound server — one URI-bound control plane serves every slot)")
                },
                "required": ["name"]
            }
        },
        {
            "name": "preview_rules",
            "title": "Preview rules",
            "annotations": annot("Preview rules", true, false, true),
            "description": "Dry-run a rules text through the exact parser `define_derived_view` uses: returns an error with the offending line number, or the accepted rule counts and echo. Call this before publishing a view so a bad lens never lands.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "rules": str_prop("Layout rules text to validate")
                },
                "required": ["rules"]
            }
        },
        {
            "name": "validate_definition",
            "title": "Validate definition",
            "annotations": annot("Validate definition", true, false, true),
            "description": "Pre-publish DRY RUN of a whole derived-view definition — not just the verb syntax (`preview_rules`) but every governance guard `define_derived_view` enforces: invalid names, name==base, shadowing a physical <name>.db, replacing a pure alias, bad rules text. Returns the EXACT refusal string a real publish would give (so nothing lands by surprise), or on success a JSON verdict {accepted:true, dry_run:true, backend, hide/only/reloc counts, would_replace_existing_definition, base_view_db_present}. Writes NOTHING. Call this right before define_derived_view to confirm a name+base+rules trio is acceptable.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": str_prop("Candidate derived view name to validate"),
                    "base": str_prop("Physical base view the candidate lens sits over"),
                    "rules": str_prop("Candidate layout rules text (inline, same as define_derived_view)"),
                    "db_path": str_prop("Metadata root; defaults to the server's --db-path/--home (ignored under a PG-bound server — one URI-bound control plane serves every slot)")
                },
                "required": ["name", "base", "rules"]
            },
            "outputSchema": validate_output_schema()
        },
        {
            "name": "list_view_tree",
            "title": "List view tree",
            "annotations": annot("List view tree", true, false, true),
            "description": "DISCOVERY + SHAPE PREVIEW: render the directory listing a mount would actually show, read from the bound store's metadata (no mount, no data IO). Give `view` (physical, derived — rendered through its real lens including synthesized dirs, alias — the full real tree, or fork — the fork's OWN tree) to inspect an existing listing, OR `base` + inline `rules` to preview an UNPUBLISHED lens before committing it. Returns JSON {kind, rendered_from, root_path, dir_count, file_count, truncated, entries:[{path,kind,size}]} where each `path` is an absolute DISPLAY path — exactly the string to reuse in a hide/only/map rule. Use this to learn which paths exist instead of guessing. `size` is the metadata field, NOT a byte count: dirs report 4096, files 0, symlinks their target length (the walk never asks the data backend) — stat through a mount for real sizes. Trees are walked through the bound control store's own backend: a file store needs a local meta/<view>.db (a remote tree is not walked), while a PG-bound store reads the view's Postgres schema directly.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "view": str_prop("Existing view name to render (physical, derived, or alias)"),
                    "base": str_prop("With `rules`: physical view to overlay a SIMULATED (uncommitted) lens on"),
                    "rules": str_prop("With `base`: layout rules text, previewed without writing a definition"),
                    "path": str_prop("Display subtree to start from (default '/', e.g. '/photos')"),
                    "depth": json!({"type":"integer","description":"Max subtree depth (default 6)","minimum":0}),
                    "max": json!({"type":"integer","description":"Max entries to render (default 800; sets truncated=true when cut)"}),
                    "db_path": str_prop("Metadata root; defaults to the server's --db-path/--home")
                }
            },
            "outputSchema": tree_output_schema()
        },
        {
            "name": "get_view",
            "title": "Get view",
            "annotations": annot("Get view", true, false, true),
            "description": "Describe ONE view in full for read-modify-write editing: for a derived view the verbatim `definition` text plus base and hide/only/reloc counts and the read-only mount note; for an alias the real view and that its real name is not mountable; for a physical view that it is a storage identity. `list_views` summarizes every slot; use this to read a single lens's exact rules before changing them. Errors on an unknown name rather than hiding a malformed definition.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": str_prop("View name to describe (definition slot or physical view db)"),
                    "db_path": str_prop("Metadata root; defaults to the server's --db-path/--home")
                },
                "required": ["name"]
            },
            "outputSchema": describe_output_schema()
        }
    ])
}

/// Build the `tools/call` result for a machine-readable payload: the same JSON
/// as pretty text (every client can display it) AND as `structuredContent`
/// (a client that honors `outputSchema` hands the agent a typed object instead
/// of making it peel and re-parse the text block).
fn json_result(v: Value) -> Value {
    let text = serde_json::to_string_pretty(&v).unwrap_or_default();
    json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": v,
    })
}

/// The `(tool '…' requires: …)` tail of a missing-argument refusal, so an agent
/// learns the whole contract without a second `tools/list` round-trip.
fn required_hint(tool: &str) -> String {
    match TOOL_REQUIRED.iter().find(|(n, _)| *n == tool) {
        Some((_, reqs)) if !reqs.is_empty() => {
            format!(" (tool '{tool}' requires: {})", reqs.join(", "))
        }
        Some(_) => format!(" (tool '{tool}' takes no required arguments)"),
        None => String::new(),
    }
}

/// Levenshtein distance over chars — the tool surface is ten short names, so a
/// plain table walk is cheaper than any clever index.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Nearest tool name to a typo, for a "did you mean" hint.
fn closest_tool(typed: &str) -> Option<&'static str> {
    let typed = typed.to_lowercase();
    let mut best: Option<(usize, &'static str)> = None;
    for cand in TOOL_NAMES {
        let d = edit_distance(&typed, &cand.to_lowercase());
        if best.map(|(bd, _)| d < bd).unwrap_or(true) {
            best = Some((d, cand));
        }
    }
    match best {
        Some((d, n)) if d <= 3 => Some(n),
        _ => None,
    }
}

/// Dispatch one `tools/call`; returns the JSON-RPC `result` object.
/// Governance failures come back INSIDE the result (`isError: true` + the CLI
/// error text), per MCP convention — the JSON-RPC layer stays for protocol
/// errors only, so the agent's client surfaces the message instead of dying.
pub fn call_tool(name: &str, args: &Value, default_store: &Arc<dyn ControlStore>) -> Value {
    let err =
        |msg: String| json!({ "content": [{ "type": "text", "text": msg }], "isError": true });
    let ok_lines = |lines: Vec<OutLine>| {
        let text: String = lines
            .iter()
            .map(|l| l.text().to_string() + "\n")
            .collect::<Vec<_>>()
            .concat();
        json!({ "content": [{ "type": "text", "text": text }] })
    };
    let s = |key: &str| -> Result<String, Value> {
        args.get(key)
            .and_then(|v| v.as_str())
            .map(|v| v.to_string())
            .ok_or_else(|| {
                err(format!(
                    "missing string argument '{key}'{}",
                    required_hint(name)
                ))
            })
    };
    // A per-call `db_path` re-points a FILE store (the historical multi-root
    // behavior); a PG-bound store is URI-bound and serves the call itself.
    let store: Arc<dyn ControlStore> = match args.get("db_path").and_then(|v| v.as_str()) {
        Some(p) => default_store.store_for_db_path(Path::new(p)),
        None => Arc::clone(default_store),
    };
    let store: &dyn ControlStore = &*store;
    match name {
        "list_views" => match views_snapshot(store) {
            Ok(v) => json_result(v),
            Err(e) => err(e),
        },
        "define_derived_view" => {
            let vname = match s("name") {
                Ok(v) => v,
                Err(e) => return e,
            };
            let vbase = match s("base") {
                Ok(v) => v,
                Err(e) => return e,
            };
            let rules = match s("rules") {
                Ok(v) => v,
                Err(e) => return e,
            };
            match define_view(store, &vname, &vbase, &rules) {
                Ok(lines) => ok_lines(lines),
                Err(e) => err(e),
            }
        }
        "alias_view" => {
            let vname = match s("name") {
                Ok(v) => v,
                Err(e) => return e,
            };
            let real = match s("real") {
                Ok(v) => v,
                Err(e) => return e,
            };
            let retire = args.get("retire").and_then(|v| v.as_str());
            match alias_view(store, &vname, &real, retire) {
                Ok(lines) => ok_lines(lines),
                Err(e) => err(e),
            }
        }
        "fork_view" => {
            let vname = match s("name") {
                Ok(v) => v,
                Err(e) => return e,
            };
            let vbase = match s("base") {
                Ok(v) => v,
                Err(e) => return e,
            };
            let seed = args.get("seed").and_then(|v| v.as_bool()).unwrap_or(true);
            match fork_view_seeded(store, &vname, &vbase, seed) {
                Ok(lines) => ok_lines(lines),
                Err(e) => err(e),
            }
        }
        "graft_view" => {
            let vname = match s("name") {
                Ok(v) => v,
                Err(e) => return e,
            };
            let plan = match s("map") {
                Ok(v) => v,
                Err(e) => return e,
            };
            let vsrc = args.get("src").and_then(|v| v.as_str());
            let dest = args.get("dest").and_then(|v| v.as_str()).unwrap_or("");
            let skip = args
                .get("skip_existing")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            match graft_view(store, &vname, vsrc, &plan, dest, skip) {
                Ok(lines) => ok_lines(lines),
                Err(e) => err(e),
            }
        }
        "remove_view" => {
            let vname = match s("name") {
                Ok(v) => v,
                Err(e) => return e,
            };
            match remove_view(store, &vname) {
                Ok(lines) => ok_lines(lines),
                Err(e) => err(e),
            }
        }
        "preview_rules" => {
            let rules = match s("rules") {
                Ok(v) => v,
                Err(e) => return e,
            };
            match preview_rules(&rules) {
                Ok(lines) => ok_lines(lines),
                Err(e) => err(e),
            }
        }
        "validate_definition" => {
            let vname = match s("name") {
                Ok(v) => v,
                Err(e) => return e,
            };
            let vbase = match s("base") {
                Ok(v) => v,
                Err(e) => return e,
            };
            let rules = match s("rules") {
                Ok(v) => v,
                Err(e) => return e,
            };
            match validate_definition(store, &vname, &vbase, &rules) {
                Ok(v) => json_result(v),
                Err(e) => err(e),
            }
        }
        "get_view" => {
            let vname = match s("name") {
                Ok(v) => v,
                Err(e) => return e,
            };
            match describe_view(store, &vname) {
                Ok(v) => json_result(v),
                Err(e) => err(e),
            }
        }
        "list_view_tree" => {
            let view = args.get("view").and_then(|v| v.as_str());
            let base = args.get("base").and_then(|v| v.as_str());
            let rules = args.get("rules").and_then(|v| v.as_str());
            let path = args.get("path").and_then(|v| v.as_str());
            let depth = args
                .get("depth")
                .and_then(|v| v.as_u64())
                .map(|v| v as usize)
                .unwrap_or(0);
            let max = args
                .get("max")
                .and_then(|v| v.as_u64())
                .map(|v| v as usize)
                .unwrap_or(0);
            let target =
                match (view, base, rules) {
                    (Some(v), _, _) => TreeTarget::Existing(v),
                    (None, Some(b), Some(r)) => TreeTarget::Simulate { base: b, rules: r },
                    (None, Some(b), None) => {
                        return err(format!(
                        "'base' also needs inline 'rules' to simulate a lens (got base '{b}' alone)"
                    ))
                    }
                    (None, None, _) => return err(
                        "provide 'view' (an existing view) or 'base' + 'rules' (a simulated lens)"
                            .into(),
                    ),
                };
            match view_tree(store, target, path, depth, max) {
                Ok(t) => json_result(tree_json(&t)),
                Err(e) => err(e),
            }
        }
        other => {
            let hint = match closest_tool(other) {
                Some(n) => format!(" — did you mean '{n}'?"),
                None => String::new(),
            };
            err(format!(
                "unknown tool '{other}'{hint} (available: {})",
                TOOL_NAMES.join(", ")
            ))
        }
    }
}

fn result_frame(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_frame(id: Value, code: i64, message: String) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// Answer one request frame. `None` = a notification (no response is written).
pub fn handle_request(
    req: &Value,
    store: &Arc<dyn ControlStore>,
    client_version: &mut Option<String>,
) -> Option<Value> {
    // Notifications carry no id — they must never be answered.
    let id = req.get("id").filter(|v| !v.is_null()).cloned();
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let params = req
        .get("params")
        .cloned()
        .unwrap_or(Value::Object(Map::new()));
    let result: Value = match method {
        "initialize" => {
            *client_version = params
                .get("protocolVersion")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            // Echo a revision we can honor; an unknown one is answered with the
            // newest we speak (the spec's negotiation rule), never echoed blindly.
            let advertised = match client_version.as_deref() {
                Some(v) if SUPPORTED_PROTOCOLS.contains(&v) => v.to_string(),
                _ => MCP_PROTOCOL_VERSION.to_string(),
            };
            json!({
                "protocolVersion": advertised,
                "capabilities": {
                    "tools": { "listChanged": false },
                    "resources": { "subscribe": false, "listChanged": false },
                    "prompts": { "listChanged": false },
                    "completions": {},
                    "logging": {},
                },
                "serverInfo": {
                    "name": "grovefs",
                    "title": "GroveFS view governance",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "instructions": INSTRUCTIONS,
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({ "tools": tool_defs() }),
        "tools/call" => {
            let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
            if name.is_empty() {
                return id.map(|id| {
                    error_frame(
                        id,
                        -32602,
                        format!(
                            "tools/call requires a 'name' (one of: {})",
                            TOOL_NAMES.join(", ")
                        ),
                    )
                });
            }
            // Some clients serialize `arguments` as a JSON string rather than an
            // object; accept that instead of reporting every argument as missing.
            let raw = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| Value::Object(Map::new()));
            let args = match raw {
                Value::String(s) if s.trim().is_empty() => Value::Object(Map::new()),
                Value::String(s) => match serde_json::from_str::<Value>(&s) {
                    Ok(v @ Value::Object(_)) => v,
                    _ => {
                        return id.map(|id| {
                            error_frame(
                                id,
                                -32602,
                                "'arguments' must be a JSON object (or a string holding one)"
                                    .into(),
                            )
                        })
                    }
                },
                other => other,
            };
            call_tool(name, &args, store)
        }
        // Resources: the same governance exposed as attachable context, so a
        // client can read a view's definition or rendered tree without a tool
        // call. Errors are protocol-level (a bad URI is not a governance refusal).
        "resources/templates/list" => json!({ "resourceTemplates": resource_templates() }),
        "resources/list" => match list_resources(&**store) {
            Ok(v) => v,
            Err(e) => return id.map(|id| error_frame(id, -32603, e)),
        },
        "resources/read" => {
            let uri = match params.get("uri").and_then(|u| u.as_str()) {
                Some(u) => u,
                None => {
                    return id.map(|id| {
                        error_frame(id, -32602, "resources/read requires a 'uri'".into())
                    })
                }
            };
            match read_resource(&**store, uri) {
                Ok(v) => v,
                Err(e) => return id.map(|id| error_frame(id, -32002, e)),
            }
        }
        // Prompts: reusable governance playbooks, argument-templated.
        "prompts/list" => json!({ "prompts": prompt_defs() }),
        "prompts/get" => {
            let pname = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let pargs = params
                .get("arguments")
                .cloned()
                .unwrap_or(Value::Object(Map::new()));
            match get_prompt(pname, &pargs) {
                Ok(v) => v,
                Err(e) => return id.map(|id| error_frame(id, -32602, e)),
            }
        }
        // Completion: fill a view-name argument from the store's own names,
        // role-aware when the ref names a prompt argument.
        "completion/complete" => match complete(&**store, &params) {
            Ok(v) => v,
            Err(e) => return id.map(|id| error_frame(id, -32602, e)),
        },
        // Logging: the ack is stateless; the threshold itself is held per session
        // (see `Session::handle`), which decides which notifications/message to emit.
        "logging/setLevel" => json!({}),
        // Client notifications (no id) are accepted silently — never answered.
        m if m.starts_with("notifications/") || m == "initialized" => {
            return None;
        }
        // Anything else: method-not-found only when an id was present (a real
        // request); a bare unknown notification stays silent.
        _ => return id.map(|id| error_frame(id, -32601, format!("method not found: {method}"))),
    };
    id.map(|id| result_frame(id, result))
}

/// A compact, secret-free description of a tool call's arguments for the audit
/// row: the identifying names, and rules only by line count (their text can be
/// large and is already persisted verbatim in any `.layout` the call writes).
fn tool_detail(args: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    for k in ["name", "base", "real", "view", "retire", "path"] {
        if let Some(v) = args.get(k).and_then(|x| x.as_str()) {
            parts.push(format!("{k}={v}"));
        }
    }
    if let Some(r) = args.get("rules").and_then(|x| x.as_str()) {
        parts.push(format!("rules={}L", r.lines().count()));
    }
    parts.join(" ")
}

/// Whether a `tools/call` response represented success, plus a short refusal
/// reason to store. Protocol errors and in-result governance failures are both
/// logged `ok=false` with their message (so an audit reader sees WHY a call was
/// rejected), while a plain accepted call is `ok=true` with an empty detail.
fn call_outcome(resp: &Option<Value>) -> (bool, String) {
    let clip = |s: &str| -> String {
        let t = s.trim();
        if t.chars().count() <= 200 {
            t.to_string()
        } else {
            let cut: String = t.chars().take(200).collect();
            format!("{cut}…")
        }
    };
    match resp {
        None => (false, "no response (notification)".to_string()),
        Some(v) => {
            if let Some(err) = v.get("error") {
                (false, clip(err["message"].as_str().unwrap_or("error")))
            } else if v["result"]["isError"].as_bool().unwrap_or(false) {
                (
                    false,
                    clip(v["result"]["content"][0]["text"].as_str().unwrap_or("")),
                )
            } else {
                (true, String::new())
            }
        }
    }
}

/// Rank an MCP/JSON-RPC log level name to a comparable integer (unknown names
/// rank as `info`). Severity is monotone so `logging/setLevel` acts as a real
/// threshold: an event is emitted only when its rank meets the configured floor.
fn log_rank(level: &str) -> u8 {
    match level {
        "debug" => 10,
        "info" => 20,
        "notice" => 30,
        "warning" | "warn" => 40,
        "error" => 50,
        "critical" => 60,
        "alert" => 70,
        "emergency" => 80,
        _ => 20,
    }
}

/// Whether a method is a per-call action worth a `notifications/message` frame
/// (state reads/writes and tool invocations), as opposed to handshake or
/// pure-static surfaces (initialize/ping/prompts/templates) that stay quiet.
fn is_loggable_call(method: &str) -> bool {
    matches!(
        method,
        "tools/call"
            | "tools/list"
            | "resources/list"
            | "resources/read"
            | "prompts/get"
            | "completion/complete"
    )
}

/// The audit-log "subject" for a call: the tool name for a `tools/call`, the
/// requested URI for a `resources/read`, else empty.
fn audit_subject(method: &str, params: &Value) -> String {
    match method {
        "tools/call" => params
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .to_string(),
        "resources/read" => params
            .get("uri")
            .and_then(|u| u.as_str())
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}

/// Per-connection server state: the shared control-plane store the governance
/// ops run against, an optional API-key store (audit log + authentication —
/// the SAME backend: the local file or the PG control tables), whether the
/// gated methods (`tools/list`, `tools/call`) require an authenticated key,
/// and — once a key authenticates — which identity this session is acting as
/// (so every logged call is attributed). `initialize`/`ping` stay open so a
/// client can always complete the handshake and present its key.
pub struct Session {
    store: Arc<dyn ControlStore>,
    keys: Option<ApiKeyStore>,
    require_key: bool,
    authed: Option<AuthedKey>,
    client_version: Option<String>,
    /// Minimum log level for `notifications/message` frames, as set by
    /// `logging/setLevel` (JSON-RPC/MCP level rank). `None` = logging not
    /// requested, so the server never pushes notification frames.
    log_min: Option<u8>,
    /// Server-initiated notification frames queued during `handle`, drained and
    /// written by `serve` after the corresponding response.
    pending: Vec<Value>,
}

impl Session {
    /// Build a session over the control plane for `default_db` + `store_uri`
    /// (a `postgres://` URI binds governance AND keys/audit to PG; otherwise
    /// both stay the local file system of record). The stores are opened
    /// best-effort: if one cannot be opened the server still runs but cannot
    /// authenticate or log. When `require_key` is set and
    /// `GROVEFS_MCP_API_KEY` is exported, the whole server is pre-authenticated
    /// (for MCP clients that cannot customize the `initialize` handshake).
    pub fn new(
        default_db: PathBuf,
        store_uri: Option<&str>,
        require_key: bool,
    ) -> Result<Self, String> {
        let control = control_store_for(&default_db, store_uri)?;
        let keys = ApiKeyStore::open_for(&default_db, store_uri).ok();
        let mut authed = None;
        if require_key {
            if let Some(s) = &keys {
                if let Ok(env_key) = std::env::var("GROVEFS_MCP_API_KEY") {
                    authed = s.authenticate(&env_key).ok();
                }
            }
        }
        Ok(Self {
            store: control,
            keys,
            require_key,
            authed,
            client_version: None,
            log_min: None,
            pending: Vec::new(),
        })
    }

    /// Try to authenticate a key presented in `initialize`, logging the attempt
    /// either way (a failed key is a security-relevant event worth an audit
    /// row). Success stamps the session identity used to attribute later calls.
    fn authenticate(&mut self, key: &str) {
        if let Some(s) = &self.keys {
            match s.authenticate(key) {
                Ok(a) => {
                    self.authed = Some(a);
                    let _ = s.log_call(
                        self.authed.as_ref(),
                        "initialize",
                        "",
                        true,
                        "authenticated",
                    );
                }
                Err(e) => {
                    let _ = s.log_call(None, "initialize", "", false, &e);
                }
            }
        }
    }

    /// Answer one request frame with this session's auth + audit behavior layered
    /// over the pure protocol dispatch ([`handle_request`]). `None` = a
    /// notification (nothing written back).
    pub fn handle(&mut self, req: &Value) -> Option<Value> {
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let id = req.get("id").filter(|v| !v.is_null()).cloned();
        let params = req
            .get("params")
            .cloned()
            .unwrap_or(Value::Object(Map::new()));

        // `initialize` is the open method that may carry the caller's key.
        if method == "initialize" {
            if let Some(key) = params.get("apiKey").and_then(|v| v.as_str()) {
                self.authenticate(key);
            }
        }

        // `logging/setLevel` sets this session's notification threshold; it is
        // never gated (it only controls how much the server talks back).
        if method == "logging/setLevel" {
            if let Some(lvl) = params.get("level").and_then(|v| v.as_str()) {
                self.log_min = Some(log_rank(lvl));
            }
        }

        // The auth gate: with `--require-api-key`, every method that reads or
        // acts on governance state is closed to an unauthenticated session — the
        // tool surface, resource discovery/read, and completion alike. `initialize`
        // /`ping`/`prompts`/`logging` stay open (handshake, static playbooks, client control).
        let gated = matches!(
            method,
            "tools/list"
                | "tools/call"
                | "resources/list"
                | "resources/templates/list"
                | "resources/read"
                | "completion/complete"
        );
        if self.require_key && gated && self.authed.is_none() {
            if let Some(s) = &self.keys {
                let subject = audit_subject(method, &params);
                let _ = s.log_call(
                    None,
                    method,
                    &subject,
                    false,
                    "unauthorized: no valid api key",
                );
            }
            let msg = "authentication required: present a valid apiKey in the initialize params (or export GROVEFS_MCP_API_KEY for the server)";
            return id.map(|id| error_frame(id, -32001, msg.to_string()));
        }

        let resp = handle_request(req, &self.store, &mut self.client_version);

        // Audit the state-touching calls (authenticated or not, success or
        // failure), attributed to the session key when one authenticated. Tools
        // and resource reads both reveal or change per-view state, so both land
        // in the audit log; pure discovery/list templates do not.
        if matches!(method, "tools/call" | "resources/read") {
            if let Some(s) = &self.keys {
                let subject = audit_subject(method, &params);
                let args = params.get("arguments").cloned().unwrap_or(Value::Null);
                let (ok, reason) = call_outcome(&resp);
                let detail = match (reason.is_empty(), ok) {
                    (true, true) => {
                        if method == "tools/call" {
                            tool_detail(&args)
                        } else {
                            subject.clone()
                        }
                    }
                    _ => {
                        let base = if method == "tools/call" {
                            tool_detail(&args)
                        } else {
                            subject.clone()
                        };
                        format!("{base} {reason}").trim().to_string()
                    }
                };
                let _ = s.log_call(self.authed.as_ref(), method, &subject, ok, &detail);
            }
        }

        // Server-initiated logging: when a threshold is set, emit one
        // `notifications/message` per answered call whose severity meets it
        // (info on success, warning on any refusal/error). Queued here, written
        // by `serve` right after the response frame.
        if let (Some(min), Some(frame)) = (self.log_min, &resp) {
            if is_loggable_call(method) {
                let (ok, reason) = call_outcome(&Some(frame.clone()));
                // Name WHICH tool / URI was called — `tools/call ok` alone told a
                // log watcher nothing it could act on.
                let subject = audit_subject(method, &params);
                let what = if subject.is_empty() {
                    method.to_string()
                } else {
                    format!("{method} {subject}")
                };
                let (lvl, rank, msg) = if ok {
                    ("info", 20, format!("{what} ok"))
                } else {
                    ("warning", 40, format!("{what} failed: {reason}"))
                };
                if rank >= min {
                    self.pending.push(json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/message",
                        "params": { "level": lvl, "logger": "grove-mcp", "data": msg },
                    }));
                }
            }
        }
        resp
    }

    /// Take the notification frames queued since the last drain (written by
    /// `serve` after the response, keeping stdout a valid JSON-RPC stream).
    pub fn drain_notifications(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.pending)
    }
}

/// Run the server to EOF on stdin (one JSON frame per line). Returns when the
/// client closes the pipe. Log/diagnostic noise goes to stderr only, keeping
/// the stdout channel a clean protocol stream. With `require_key`, `tools/list`
/// and `tools/call` are refused until an API key authenticates; every tool call
/// is appended to the audit log either way — `<db>/meta/api_keys.db` under
/// SQLite, `grovefs_control.mcp_calls` under a `postgres://` `--store` URI.
pub fn serve(
    default_db: PathBuf,
    store_uri: Option<&str>,
    require_key: bool,
) -> std::io::Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut session =
        Session::new(default_db, store_uri, require_key).map_err(std::io::Error::other)?;
    if require_key {
        eprintln!(
            "[grovefs mcp] api-key auth REQUIRED (state-touching methods — tools/list, tools/call, resources/*, completion/complete — gated until a key in `initialize` authenticates; mint one with `grovefs apikey create`)"
        );
    }
    for line in stdin.lock().lines() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let frame = match serde_json::from_str::<Value>(line) {
            Ok(req) => session.handle(&req),
            Err(e) => Some(error_frame(
                Value::Null,
                -32700,
                format!("invalid JSON-RPC frame: {e}"),
            )),
        };
        let version_report = session.client_version.clone();
        if let Some(frame) = frame {
            serde_json::to_writer(&mut stdout, &frame)?;
            stdout.write_all(b"\n")?;
        }
        // Then any server-initiated notification frames this call queued
        // (`notifications/message` from the logging layer) — written after the
        // response, before the flush, so stdout stays a valid frame stream.
        for note in session.drain_notifications() {
            serde_json::to_writer(&mut stdout, &note)?;
            stdout.write_all(b"\n")?;
        }
        stdout.flush()?;
        if let Some(v) = version_report {
            eprintln!("[grovefs mcp] client speaks protocol {v} (serving tools + resources + prompts + completion + logging)");
            session.client_version = None;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> PathBuf {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().to_path_buf();
        std::mem::forget(dir); // test-only: leak the temp dir, cleaned by /tmp
        p
    }

    fn ask(req: &Value, store: &Arc<dyn ControlStore>) -> Option<Value> {
        let mut cv = None;
        handle_request(req, store, &mut cv)
    }

    fn fstore(p: &Path) -> Arc<dyn ControlStore> {
        Arc::new(FileControlStore::new(p))
    }

    /// Seed `<home>/meta/team.db` with `/docs/a.md` and `/photos/private/x.jpg`
    /// (the fixture the introspection tests walk). The writer is dropped before
    /// returning so a read-only probe opens a clean handle.
    fn seed_team_tree(home: &Path) {
        std::fs::create_dir_all(home.join("meta")).unwrap();
        {
            let m = MetaEngine::open(&home.join("meta/team.db")).unwrap();
            let docs = m
                .create_node(ROOT_INO, "docs", true, None, 0o0755, 0, 0)
                .unwrap();
            m.create_node(docs, "a.md", false, Some("k-docs-a"), 0o0644, 0, 0)
                .unwrap();
            let photos = m
                .create_node(ROOT_INO, "photos", true, None, 0o0755, 0, 0)
                .unwrap();
            let priv_ = m
                .create_node(photos, "private", true, None, 0o0755, 0, 0)
                .unwrap();
            m.create_node(priv_, "x.jpg", false, Some("k-priv-x"), 0o0644, 0, 0)
                .unwrap();
        }
    }

    #[test]
    fn initialize_echoes_client_version() {
        let r = ask(
            &json!({"jsonrpc":"2.0","id":1,"method":"initialize",
                    "params":{"protocolVersion":"2024-11-05","capabilities":{}}}),
            &fstore(Path::new("/")),
        )
        .unwrap();
        assert_eq!(r["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(r["result"]["serverInfo"]["name"], "grovefs");
        assert!(r["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn notifications_are_never_answered() {
        assert!(ask(
            &json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            &fstore(Path::new("/"))
        )
        .is_none());
        // A request (with id) for an unknown method gets method-not-found.
        let r = ask(
            &json!({"jsonrpc":"2.0","id":2,"method":"foo/bar"}),
            &fstore(Path::new("/")),
        )
        .unwrap();
        assert_eq!(r["error"]["code"], -32601);
    }

    #[test]
    fn tools_list_advertises_the_ten_governance_tools() {
        let r = ask(
            &json!({"jsonrpc":"2.0","id":3,"method":"tools/list"}),
            &fstore(Path::new("/")),
        )
        .unwrap();
        let names: Vec<&str> = r["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec![
                "list_views",
                "define_derived_view",
                "alias_view",
                "fork_view",
                "graft_view",
                "remove_view",
                "preview_rules",
                "validate_definition",
                "list_view_tree",
                "get_view"
            ]
        );
        for t in r["result"]["tools"].as_array().unwrap() {
            assert!(t["inputSchema"]["type"] == "object" && t["description"].is_string());
            // Every tool is annotated so a client can auto-approve the safe half.
            assert!(t["title"].is_string(), "tool {} has no title", t["name"]);
            assert_eq!(t["annotations"]["openWorldHint"], false);
            assert!(t["annotations"]["readOnlyHint"].is_boolean());
            // ...and the machine-readable answers declare their output shape.
            let name = t["name"].as_str().unwrap();
            let machine = matches!(
                name,
                "list_views" | "list_view_tree" | "get_view" | "validate_definition"
            );
            assert_eq!(
                t["outputSchema"].is_object(),
                machine,
                "outputSchema presence wrong for {name}"
            );
        }
    }

    #[test]
    fn read_only_tools_are_annotated_as_such() {
        let tools = tool_defs();
        let tools = tools.as_array().unwrap();
        let ro = |n: &str| -> bool {
            tools.iter().find(|t| t["name"] == n).unwrap()["annotations"]["readOnlyHint"]
                .as_bool()
                .unwrap()
        };
        for n in [
            "list_views",
            "list_view_tree",
            "get_view",
            "preview_rules",
            "validate_definition",
        ] {
            assert!(ro(n), "{n} must be readOnlyHint=true");
        }
        for n in [
            "define_derived_view",
            "alias_view",
            "fork_view",
            "graft_view",
            "remove_view",
        ] {
            assert!(!ro(n), "{n} writes and must not claim readOnlyHint");
        }
        assert_eq!(
            tools.iter().find(|t| t["name"] == "remove_view").unwrap()["annotations"]
                ["destructiveHint"],
            true
        );
    }

    #[test]
    fn unknown_protocol_version_negotiates_to_a_supported_one() {
        for v in SUPPORTED_PROTOCOLS {
            let r = ask(
                &json!({"jsonrpc":"2.0","id":1,"method":"initialize",
                        "params":{"protocolVersion":v}}),
                &fstore(Path::new("/")),
            )
            .unwrap();
            assert_eq!(r["result"]["protocolVersion"], v);
        }
        let r = ask(
            &json!({"jsonrpc":"2.0","id":2,"method":"initialize",
                    "params":{"protocolVersion":"1999-01-01"}}),
            &fstore(Path::new("/")),
        )
        .unwrap();
        assert_eq!(r["result"]["protocolVersion"], MCP_PROTOCOL_VERSION);
    }

    #[test]
    fn tool_errors_carry_actionable_hints() {
        let store = fstore(Path::new("/"));
        let text = |v: &Value| -> String {
            v["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .to_string()
        };
        // A typo gets the whole surface plus the nearest name.
        let r = ask(
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                    "params":{"name":"define_derived_vew","arguments":{}}}),
            &store,
        )
        .unwrap();
        let t = text(&r);
        assert!(t.starts_with("unknown tool 'define_derived_vew'"), "{t}");
        assert!(t.contains("did you mean 'define_derived_view'?"), "{t}");
        assert!(t.contains("available: list_views"), "{t}");
        // A missing argument names the tool and its whole required set.
        let r = ask(
            &json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
                    "params":{"name":"define_derived_view","arguments":{"name":"x"}}}),
            &store,
        )
        .unwrap();
        let t = text(&r);
        assert!(t.contains("missing string argument 'base'"), "{t}");
        assert!(t.contains("requires: name, base, rules"), "{t}");
        // `tools/call` with no name is a protocol error that names the surface.
        let r = ask(
            &json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{}}),
            &store,
        )
        .unwrap();
        assert_eq!(r["error"]["code"], -32602);
        assert!(r["error"]["message"]
            .as_str()
            .unwrap()
            .contains("list_views"));
    }

    #[test]
    fn arguments_may_arrive_as_a_json_string() {
        let home = db();
        let store = fstore(&home);
        seed_team_tree(&home);
        let r = ask(
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                    "params":{"name":"list_view_tree",
                              "arguments":"{\"view\":\"team\",\"depth\":2}"}}),
            &store,
        )
        .unwrap();
        assert!(r["result"]["isError"].is_null(), "{r}");
        assert_eq!(r["result"]["structuredContent"]["kind"], "physical");
        // A string that is not a JSON object is a protocol error, not silence.
        let r = ask(
            &json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
                    "params":{"name":"list_views","arguments":"not json"}}),
            &store,
        )
        .unwrap();
        assert_eq!(r["error"]["code"], -32602);
    }

    #[test]
    fn alias_refuses_a_dangling_physical_target() {
        let home = db();
        let store = fstore(&home);
        std::fs::create_dir_all(home.join("meta")).unwrap();
        std::fs::write(home.join("meta/team.db"), b"").unwrap();
        let call = |id, name: &str, args: Value| {
            ask(
                &json!({"jsonrpc":"2.0","id":id,"method":"tools/call",
                        "params":{"name":name,"arguments":args}}),
                &store,
            )
            .unwrap()
        };
        let text = |v: &Value| -> String {
            v["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .to_string()
        };
        // A real physical tree is aliasable.
        let okres = call(1, "alias_view", json!({"name": "pub", "real": "team"}));
        assert_eq!(okres["result"]["isError"], Value::Null);
        assert!(text(&okres).contains("defined pure alias 'pub' -> real view 'team'"));
        // A target with no physical tree is refused at definition time, not
        // deferred to mount (§5-10), and the refusal names the missing view.
        let bad = call(2, "alias_view", json!({"name": "ghost", "real": "no-such-tree"}));
        assert_eq!(bad["result"]["isError"], true);
        assert!(text(&bad).contains("no physical view tree"), "{}", text(&bad));
        assert!(text(&bad).contains("no-such-tree"), "{}", text(&bad));
        // The refused alias must not have been recorded in the control plane.
        let l = call(3, "list_views", json!({}));
        let parsed: Value = serde_json::from_str(&text(&l)).unwrap();
        assert!(parsed["defined"]
            .as_array()
            .unwrap()
            .iter()
            .all(|d| d["name"] != "ghost"));
    }

    #[test]
    fn full_governance_round_trip_over_tool_calls() {
        let home = db();
        let store = fstore(&home);
        // Seed one physical view db so define/alias see a realistic store.
        std::fs::create_dir_all(home.join("meta")).unwrap();
        std::fs::write(home.join("meta/team.db"), b"").unwrap();
        let call = |id, name: &str, args: Value| {
            ask(
                &json!({"jsonrpc":"2.0","id":id,"method":"tools/call",
                        "params":{"name":name,"arguments":args}}),
                &store,
            )
            .unwrap()
        };
        let text = |v: &Value| -> String {
            v["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .to_string()
        };
        // preview: a bad rule never lands, with line-accurate parser errors.
        let bad = call(10, "preview_rules", json!({"rules": "bogber /x"}));
        assert_eq!(bad["result"]["isError"], true);
        // define a derived lens with inline rules (no files for the agent).
        let d = call(
            11,
            "define_derived_view",
            json!({"name": "docsonly", "base": "team", "rules": "# cut\nonly /docs"}),
        );
        assert!(text(&d).contains(
            "defined derived view 'docsonly' over base 'team': 0 hide, 1 only, 0 reloc rule(s)"
        ));
        assert!(home.join("meta/views/docsonly.layout").is_file());
        // name == base is the same refusal the CLI prints.
        let same = call(
            12,
            "define_derived_view",
            json!({"name": "team", "base": "team", "rules": "only /docs"}),
        );
        assert_eq!(same["result"]["isError"], true);
        assert!(text(&same).contains("derived view name must differ from its base"));
        // validate_definition dry-runs the FULL guard set WITHOUT writing: a
        // rejected trio returns the byte-identical CLI refusal, so the agent
        // sees exactly what a real publish would say before anything lands...
        let vrej = call(
            19,
            "validate_definition",
            json!({"name": "team", "base": "team", "rules": "only /docs"}),
        );
        assert_eq!(vrej["result"]["isError"], true);
        assert!(text(&vrej).contains("derived view name must differ from its base"));
        // ...and an accepted trio returns a JSON verdict while touching nothing
        // (the no-write contract: no <name>.layout appears).
        let vok = call(
            20,
            "validate_definition",
            json!({"name": "newsafe", "base": "team", "rules": "hide /photos/private"}),
        );
        let vv: Value = serde_json::from_str(&text(&vok)).unwrap();
        assert_eq!(vv["accepted"], true);
        assert_eq!(vv["dry_run"], true);
        assert_eq!(vv["hide"], 1);
        assert_eq!(vv["would_replace_existing_definition"], false);
        assert_eq!(vv["base_view_db_present"], true);
        assert!(!home.join("meta/views/newsafe.layout").exists());
        // alias, then list sees BOTH governance shapes plus the physical tree.
        let a = call(13, "alias_view", json!({"name": "public", "real": "team"}));
        assert!(text(&a).contains("defined pure alias 'public' -> real view 'team'"));
        let l = call(14, "list_views", json!({}));
        let parsed: Value = serde_json::from_str(&text(&l)).unwrap();
        assert_eq!(parsed["physical"], json!(["team"]));
        assert_eq!(parsed["defined"][0]["name"], "docsonly"); // sorted: d < p
        assert_eq!(parsed["defined"][0]["kind"], "derived");
        assert_eq!(parsed["defined"][0]["only"], 1);
        assert_eq!(parsed["defined"][1]["kind"], "alias");
        // gapless rotation through the tool (write-before-delete contract).
        let rot = call(
            15,
            "alias_view",
            json!({"name": "public2", "real": "team", "retire": "public"}),
        );
        assert!(
            text(&rot).contains("rotated public name for real view 'team': 'public' -> 'public2'")
        );
        assert!(!home.join("meta/views/public.layout").exists());
        // remove both definitions; data (team.db) untouched.
        for (i, n) in [(16, "docsonly"), (17, "public2")] {
            let r = call(i, "remove_view", json!({"name": n}));
            assert!(text(&r).contains("removed view definition"));
        }
        assert!(home.join("meta/team.db").is_file());
        let gone = call(18, "remove_view", json!({"name": "docsonly"}));
        assert_eq!(gone["result"]["isError"], true);
        assert!(text(&gone).starts_with("no view definition 'docsonly' in"));
    }

    /// The fork→graft flow an agent is meant to use: an EMPTY fork, then a
    /// curated plan that renames as it places — `src` defaulting to the fork's
    /// recorded base, so a curation script never names the base again.
    #[test]
    fn graft_tool_fills_an_empty_fork_and_refuses_lens_targets() {
        let home = db();
        let store = fstore(&home);
        std::fs::create_dir_all(home.join("meta")).unwrap();
        {
            let e = crate::meta::MetaEngine::open(&home.join("meta/base.db")).unwrap();
            let docs = e
                .create_node(crate::meta::ROOT_INO, "docs", true, None, 0o755, 1000, 1000)
                .unwrap();
            e.create_node(docs, "uuid-1", false, Some("blob-1"), 0o644, 1000, 1000)
                .unwrap();
        }
        let call = |id, name: &str, args: Value| {
            ask(
                &json!({"jsonrpc":"2.0","id":id,"method":"tools/call",
                        "params":{"name":name,"arguments":args}}),
                &store,
            )
            .unwrap()
        };
        let text = |v: &Value| -> String {
            v["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .to_string()
        };
        // Default seed=true copies; seed=false leaves a bare root.
        let empty = call(
            40,
            "fork_view",
            json!({"name": "lab", "base": "base", "seed": false}),
        );
        let t = text(&empty);
        assert!(t.contains("as an EMPTY tree: root only"), "{t}");
        assert!(
            t.contains("view graft --name lab --src base --map plan.tsv"),
            "{t}"
        );
        // The plan renames into a directory the graft creates; src defaults to
        // the fork's recorded base.
        let g = call(
            41,
            "graft_view",
            json!({"name": "lab", "map": "docs/uuid-1\tby_target/M31_2025.fits"}),
        );
        let t = text(&g);
        assert!(
            t.contains("grafted 2 entries (1 directory) from 'base' into 'lab' at /"),
            "{t}"
        );
        assert!(t.contains("1 shared object(s)"), "{t}");
        let lab = crate::meta::MetaEngine::open(&home.join("meta/lab.db")).unwrap();
        let by = lab.lookup(crate::meta::ROOT_INO, "by_target").unwrap();
        let f = lab
            .lookup(by.id, "M31_2025.fits")
            .expect("renamed entry placed");
        assert_eq!(f.data_ref.as_deref(), Some("blob-1"));
        // A lens renders another tree: it owns no nodes, so it cannot host a graft.
        let d = call(
            42,
            "define_derived_view",
            json!({"name": "lens", "base": "base", "rules": "only /docs"}),
        );
        assert!(!d["result"]["isError"].as_bool().unwrap_or(false));
        let lens = call(
            43,
            "graft_view",
            json!({"name": "lens", "map": "docs/uuid-1"}),
        );
        assert_eq!(lens["result"]["isError"], true);
        assert!(text(&lens).contains("derived lens"), "{}", text(&lens));
        // A base view has no recorded base: --src is required, and said so.
        let nosrc = call(
            44,
            "graft_view",
            json!({"name": "base", "map": "docs/uuid-1"}),
        );
        assert_eq!(nosrc["result"]["isError"], true);
        assert!(text(&nosrc).contains("pass --src"), "{}", text(&nosrc));
        // A plan that tries to walk out of the view is refused line-accurately.
        let esc = call(
            45,
            "graft_view",
            json!({"name": "lab", "map": "../etc/passwd"}),
        );
        assert_eq!(esc["result"]["isError"], true);
        assert!(
            text(&esc).contains("line 1") && text(&esc).contains(".."),
            "{}",
            text(&esc)
        );
    }

    /// A fork is created, listed, described, protected from aliasing and
    /// removed THROUGH THE TOOLS (the same ops the CLI drives), and its own
    /// tree disappears on removal while the base tree survives.
    #[test]
    fn fork_lifecycle_over_tool_calls() {
        let home = db();
        let store = fstore(&home);
        std::fs::create_dir_all(home.join("meta")).unwrap();
        {
            let m = MetaEngine::open(&home.join("meta/base.db")).unwrap();
            let d = m
                .create_node(ROOT_INO, "docs", true, None, 0o0755, 0, 0)
                .unwrap();
            m.create_node(d, "a.md", false, Some("k-a"), 0o0644, 0, 0)
                .unwrap();
        }
        let call = |id: i64, name: &str, args: Value| {
            ask(
                &json!({"jsonrpc":"2.0","id":id,"method":"tools/call",
                        "params":{"name":name,"arguments":args}}),
                &store,
            )
            .unwrap()
        };
        let text = |v: &Value| -> String {
            v["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .to_string()
        };
        // A missing base tree is refused with the honest PG caveat.
        let nob = call(30, "fork_view", json!({"name": "lab", "base": "ghost"}));
        assert_eq!(nob["result"]["isError"], true);
        assert!(text(&nob).contains("no physical base tree"));
        assert!(text(&nob).contains("PostgreSQL"));
        // Fork the real tree: 3 entries (root+docs+a.md), 1 shared blob.
        let f = call(31, "fork_view", json!({"name": "lab", "base": "base"}));
        assert!(
            text(&f).contains("forked view 'lab' from base 'base': 3 entries, 1 shared object(s)")
        );
        assert_eq!(
            std::fs::read_to_string(home.join("meta/views/lab.layout")).unwrap(),
            "fork base\n"
        );
        assert!(home.join("meta/lab.db").is_file());
        // list_views reports the fork beside the physical trees (incl. its own).
        let l = call(32, "list_views", json!({}));
        let parsed: Value = serde_json::from_str(&text(&l)).unwrap();
        assert_eq!(parsed["physical"], json!(["base", "lab"]));
        assert_eq!(parsed["defined"][0]["name"], "lab");
        assert_eq!(parsed["defined"][0]["kind"], "fork");
        assert_eq!(parsed["defined"][0]["real_view"], "base");
        // describe_view spells out the mount semantics and the own tree.
        let d = call(33, "get_view", json!({"name": "lab"}));
        let dv: Value = serde_json::from_str(&text(&d)).unwrap();
        assert_eq!(dv["kind"], "fork");
        assert_eq!(dv["own_tree"], "meta/lab.db");
        assert!(dv["mount"].as_str().unwrap().contains("content read-only"));
        // A fork is a tree identity, not a public name: aliasing it is refused.
        let a = call(34, "alias_view", json!({"name": "pub", "real": "lab"}));
        assert_eq!(a["result"]["isError"], true);
        assert!(text(&a).contains("is a fork"));
        // A GOVERNED base name cannot be forked: 'lens' resolves through a
        // definition, and `base` without a definition still forks fine.
        let dv2 = call(
            35,
            "define_derived_view",
            json!({"name": "lens", "base": "base", "rules": "only /docs"}),
        );
        assert!(text(&dv2).contains("defined derived view 'lens' over base 'base'"));
        let f2 = call(36, "fork_view", json!({"name": "lab2", "base": "lens"}));
        assert_eq!(f2["result"]["isError"], true);
        assert!(text(&f2).contains("is governed by a definition"));
        // Re-forking an existing name is refused before any tree is copied.
        let f3 = call(37, "fork_view", json!({"name": "lab", "base": "base"}));
        assert_eq!(f3["result"]["isError"], true);
        assert!(text(&f3).contains("already has a definition"));
        // Removing the fork deletes its tree; the base tree stays.
        let r = call(38, "remove_view", json!({"name": "lab"}));
        assert!(text(&r).contains("removed fork view 'lab' of base 'base'"));
        assert!(!home.join("meta/lab.db").exists());
        assert!(home.join("meta/base.db").is_file());
    }

    #[test]
    fn missing_argument_is_a_tool_error_not_a_crash() {
        let home = db();
        let store = fstore(&home);
        let r = ask(
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                    "params":{"name":"define_derived_view","arguments":{"name":"x"}}}),
            &store,
        )
        .unwrap();
        assert_eq!(r["result"]["isError"], true);
        assert!(r["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("missing string argument 'base'"));
    }

    #[test]
    fn list_view_tree_renders_physical_and_lens_shapes() {
        let home = db();
        let store = fstore(&home);
        std::fs::create_dir_all(home.join("meta")).unwrap();
        // Seed a REAL tree with the normal engine, then drop the writer so the
        // read-only walk below opens a clean handle: /docs/a.md and
        // /photos/private/x.jpg.
        {
            let m = MetaEngine::open(&home.join("meta/team.db")).unwrap();
            let docs = m
                .create_node(ROOT_INO, "docs", true, None, 0o0755, 0, 0)
                .unwrap();
            m.create_node(docs, "a.md", false, Some("k-docs-a"), 0o0644, 0, 0)
                .unwrap();
            let photos = m
                .create_node(ROOT_INO, "photos", true, None, 0o0755, 0, 0)
                .unwrap();
            let priv_ = m
                .create_node(photos, "private", true, None, 0o0755, 0, 0)
                .unwrap();
            m.create_node(priv_, "x.jpg", false, Some("k-priv-x"), 0o0644, 0, 0)
                .unwrap();
            m.create_symlink(ROOT_INO, "latest", "/docs/a.md", 0, 0)
                .unwrap();
        }
        let call = |id: i64, name: &str, args: Value| {
            ask(
                &json!({"jsonrpc":"2.0","id":id,"method":"tools/call",
                        "params":{"name":name,"arguments":args}}),
                &store,
            )
            .unwrap()
        };
        let text = |v: &Value| -> String {
            v["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .to_string()
        };
        let paths = |v: &Value| -> Vec<String> {
            serde_json::from_str::<Value>(&text(v)).unwrap()["entries"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e["path"].as_str().unwrap().to_string())
                .collect()
        };
        // (1) physical listing = the whole stored tree.
        let t = call(20, "list_view_tree", json!({"view": "team", "depth": 10}));
        let parsed: Value = serde_json::from_str(&text(&t)).unwrap();
        assert_eq!(parsed["kind"], "physical");
        assert_eq!(parsed["rendered_from"], "team");
        let all = paths(&t);
        assert!(all.contains(&"/docs".to_string()));
        assert!(all.contains(&"/photos/private".to_string()));
        assert!(all.contains(&"/photos/private/x.jpg".to_string()));
        // (2) publish a lens hiding the private subtree, then render THROUGH it:
        // the discovery tool shows exactly the shape a mount would.
        let d = call(
            21,
            "define_derived_view",
            json!({"name": "safe", "base": "team", "rules": "hide /photos/private"}),
        );
        assert!(text(&d).contains("defined derived view 'safe'"));
        let lens = call(22, "list_view_tree", json!({"view": "safe", "depth": 10}));
        let pl: Value = serde_json::from_str(&text(&lens)).unwrap();
        assert_eq!(pl["kind"], "derived");
        assert_eq!(pl["rendered_from"], "team"); // walked the base THROUGH the lens
        let shown = paths(&lens);
        assert!(shown.contains(&"/photos".to_string()));
        assert!(!shown.contains(&"/photos/private".to_string())); // pruned by the rule
                                                                  // (3) SIMULATE an uncommitted lens — preview the effect without writing
                                                                  // any definition (no `safe2.layout` is created).
        let sim = call(
            23,
            "list_view_tree",
            json!({"base": "team", "rules": "only /docs", "depth": 10}),
        );
        let ps: Value = serde_json::from_str(&text(&sim)).unwrap();
        assert_eq!(ps["kind"], "simulated");
        let simpaths = paths(&sim);
        assert!(simpaths.contains(&"/docs".to_string()));
        assert!(!simpaths.contains(&"/photos".to_string()));
        assert!(!home.join("meta/views/safe2.layout").exists());
        // (4) get_view echoes the EXACT rules text for read-modify-write.
        let g = call(24, "get_view", json!({"name": "safe"}));
        let pg: Value = serde_json::from_str(&text(&g)).unwrap();
        assert_eq!(pg["kind"], "derived");
        assert_eq!(pg["base"], "team");
        assert!(pg["definition"]
            .as_str()
            .unwrap()
            .contains("hide /photos/private"));
        // (5) a real listing under an alias resolves to the FULL real tree.
        call(25, "alias_view", json!({"name": "pub", "real": "team"}));
        let a = call(26, "list_view_tree", json!({"view": "pub", "depth": 10}));
        let pa: Value = serde_json::from_str(&text(&a)).unwrap();
        assert_eq!(pa["kind"], "alias");
        assert!(paths(&a).contains(&"/photos/private/x.jpg".to_string()));
        // (6) `size` is the METADATA field, not a byte count — the tool contract
        // now says so explicitly, and this pins it: a dir carries the stat
        // placeholder 4096, a file 0, a symlink its target text length. A change
        // here (e.g. a real-bytes walk) must update that self-description too.
        let size_of = |p: &str, kind: &str| -> u64 {
            parsed["entries"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["path"] == p && e["kind"] == kind)
                .unwrap_or_else(|| panic!("no {kind} entry '{p}' in {parsed}"))["size"]
                .as_u64()
                .unwrap()
        };
        assert_eq!(size_of("/docs", "dir"), 4096);
        assert_eq!(size_of("/docs/a.md", "file"), 0);
        assert_eq!(size_of("/latest", "link"), "/docs/a.md".len() as u64);
    }

    #[test]
    fn list_view_tree_missing_local_db_is_a_tool_error() {
        let home = db();
        let store = fstore(&home);
        std::fs::create_dir_all(home.join("meta")).unwrap();
        // No meta/ghost.db exists → an honest control-plane refusal, not a crash
        // (the store names its own backend — files here, `grovefs_control` on PG).
        let r = ask(
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                    "params":{"name":"list_view_tree","arguments":{"view":"ghost"}}}),
            &store,
        )
        .unwrap();
        assert_eq!(r["result"]["isError"], true);
        assert!(r["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("no metadata tree for view 'ghost'"));
    }

    #[test]
    fn api_key_gate_refuses_then_admits_and_logs() {
        let home = db();
        std::fs::create_dir_all(home.join("meta")).unwrap();
        std::fs::write(home.join("meta/team.db"), b"").unwrap();
        // Mint a key out-of-band, exactly as `grovefs apikey create` would.
        let store = ApiKeyStore::open(&home).unwrap();
        let (_id, key) = store.create("agent-a").unwrap();

        let mut sess = Session::new(home.clone(), None, true).unwrap();
        // tools/list is gated: refused with a JSON-RPC error (not an in-result
        // isError) until a key authenticates.
        let refused = sess
            .handle(&json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}))
            .unwrap();
        assert_eq!(refused["error"]["code"], -32001);

        // initialize WITHOUT a key stays open but does not authenticate.
        sess.handle(&json!({"jsonrpc":"2.0","id":2,"method":"initialize",
                            "params":{"protocolVersion":"2025-06-18"}}));
        assert_eq!(
            sess.handle(&json!({"jsonrpc":"2.0","id":3,"method":"tools/list"}))
                .unwrap()["error"]["code"],
            -32001
        );

        // initialize WITH a bad key: logged failure, still not authed.
        sess.handle(&json!({"jsonrpc":"2.0","id":4,"method":"initialize",
                            "params":{"apiKey":"nosuchid:nosecret"}}));
        assert!(sess.authed.is_none());

        // initialize WITH the good key authenticates; now tools/list is admitted.
        sess.handle(&json!({"jsonrpc":"2.0","id":5,"method":"initialize",
                            "params":{"apiKey": key.clone()}}));
        assert!(sess.authed.is_some());
        let ok = sess
            .handle(&json!({"jsonrpc":"2.0","id":6,"method":"tools/list"}))
            .unwrap();
        assert!(ok["result"]["tools"].is_array());

        // A governance write through the gate is admitted AND logged against the key.
        let d = sess
            .handle(&json!({"jsonrpc":"2.0","id":7,"method":"tools/call",
                            "params":{"name":"define_derived_view",
                                      "arguments":{"name":"docs","base":"team","rules":"only /docs"}}}))
            .unwrap();
        assert_ne!(d["result"]["isError"], true);
        assert!(home.join("meta/views/docs.layout").is_file());

        let calls = store.recent_calls(50).unwrap();
        let define = calls
            .iter()
            .find(|c| c.tool == "define_derived_view")
            .unwrap();
        assert!(define.ok);
        assert_eq!(define.key_label.as_deref(), Some("agent-a"));
        // the refused tools/list and the bad-key initialize are BOTH auditable.
        assert!(calls
            .iter()
            .any(|c| !c.ok && c.detail.contains("unauthorized")));
        assert!(calls
            .iter()
            .any(|c| c.method == "initialize" && !c.ok && c.detail.contains("unknown api key id")));
    }

    #[test]
    fn default_session_serves_without_key_but_still_logs() {
        let home = db();
        std::fs::create_dir_all(home.join("meta")).unwrap();
        let store = ApiKeyStore::open(&home).unwrap();
        // require_key = false: the plain server behaves exactly as before (no
        // gate), while calls still land in the audit log for attribution.
        let mut sess = Session::new(home.clone(), None, false).unwrap();
        let r = sess
            .handle(&json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}))
            .unwrap();
        assert!(r["result"]["tools"].is_array());
        sess.handle(&json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
                            "params":{"name":"list_views","arguments":{}}}))
            .unwrap();
        let calls = store.recent_calls(10).unwrap();
        assert!(calls.iter().any(|c| c.tool == "list_views" && c.ok));
        // unauthenticated by design, so the row carries no key identity.
        assert!(calls.iter().all(|c| c.key_id.is_none()));
    }

    #[test]
    fn resources_are_enumerated_read_and_gated() {
        let home = db();
        let store = fstore(&home);
        std::fs::create_dir_all(home.join("meta/views")).unwrap();
        // A physical view backed by a real engine tree, plus a derived definition.
        {
            let m = MetaEngine::open(&home.join("meta/team.db")).unwrap();
            let docs = m
                .create_node(ROOT_INO, "docs", true, None, 0o0755, 0, 0)
                .unwrap();
            m.create_node(docs, "a.md", false, Some("k-a"), 0o0644, 0, 0)
                .unwrap();
        }
        std::fs::write(
            home.join("meta/views/release.layout"),
            b"base team\nhide /photos/private\n",
        )
        .unwrap();

        let list = ask(
            &json!({"jsonrpc":"2.0","id":1,"method":"resources/list"}),
            &store,
        )
        .unwrap();
        let uris: Vec<&str> = list["result"]["resources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["uri"].as_str().unwrap())
            .collect();
        assert!(uris.contains(&"grovefs://view/team/tree"));
        assert!(uris.contains(&"grovefs://view/release/definition"));
        assert!(uris.contains(&"grovefs://view/release/tree"));

        let tmpl = ask(
            &json!({"jsonrpc":"2.0","id":2,"method":"resources/templates/list"}),
            &store,
        )
        .unwrap();
        assert_eq!(
            tmpl["result"]["resourceTemplates"]
                .as_array()
                .unwrap()
                .len(),
            2
        );

        // The definition resource mirrors get_view (carries the verbatim rule).
        let def = ask(
            &json!({"jsonrpc":"2.0","id":3,"method":"resources/read",
                    "params":{"uri":"grovefs://view/release/definition"}}),
            &store,
        )
        .unwrap();
        assert!(def["result"]["contents"][0]["text"]
            .as_str()
            .unwrap()
            .contains("hide /photos/private"));
        assert_eq!(def["result"]["contents"][0]["mimeType"], "application/json");

        // The tree resource is the real listing a mount shows (metadata only).
        let tree = ask(
            &json!({"jsonrpc":"2.0","id":4,"method":"resources/read",
                    "params":{"uri":"grovefs://view/team/tree"}}),
            &store,
        )
        .unwrap();
        let ttext = tree["result"]["contents"][0]["text"].as_str().unwrap();
        assert!(ttext.contains("/docs") && ttext.contains("a.md"));

        // A bad facet is a protocol error (not an in-result refusal, not a crash).
        let bad = ask(
            &json!({"jsonrpc":"2.0","id":5,"method":"resources/read",
                    "params":{"uri":"grovefs://view/team/bogus"}}),
            &store,
        )
        .unwrap();
        assert_eq!(bad["error"]["code"], -32002);

        // Under --require-api-key, resources/read is gated like the tools.
        let mut sess = Session::new(home.clone(), None, true).unwrap();
        let refused = sess
            .handle(&json!({"jsonrpc":"2.0","id":6,"method":"resources/read",
                            "params":{"uri":"grovefs://view/release/definition"}}))
            .unwrap();
        assert_eq!(refused["error"]["code"], -32001);
    }

    #[test]
    fn prompts_are_instantiated_and_completion_lists_view_names() {
        let home = db();
        let store = fstore(&home);
        std::fs::create_dir_all(home.join("meta/views")).unwrap();
        std::fs::write(home.join("meta/team.db"), b"").unwrap();
        std::fs::write(
            home.join("meta/views/release.layout"),
            b"base team\nhide /x\n",
        )
        .unwrap();

        let pl = ask(
            &json!({"jsonrpc":"2.0","id":1,"method":"prompts/list"}),
            &store,
        )
        .unwrap();
        let names: Vec<&str> = pl["result"]["prompts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["reshape-view", "rotate-alias"]);

        let pg = ask(
            &json!({"jsonrpc":"2.0","id":2,"method":"prompts/get",
                    "params":{"name":"reshape-view","arguments":{"base":"team","goal":"drop privates"}}}),
            &store,
        )
        .unwrap();
        let msg = pg["result"]["messages"][0]["content"]["text"]
            .as_str()
            .unwrap();
        assert!(
            msg.contains("'team'")
                && msg.contains("drop privates")
                && msg.contains("validate_definition")
        );

        // A missing required argument is a protocol error.
        let perr = ask(
            &json!({"jsonrpc":"2.0","id":3,"method":"prompts/get",
                    "params":{"name":"rotate-alias","arguments":{"real":"team"}}}),
            &store,
        )
        .unwrap();
        assert_eq!(perr["error"]["code"], -32602);

        // Completion returns the store's own view names by prefix, case-insensitively.
        let c = ask(
            &json!({"jsonrpc":"2.0","id":4,"method":"completion/complete",
                    "params":{"ref":{"type":"ref/resource","value":"grovefs://view/{name}/tree"},
                              "argument":{"name":"name","value":"RE"}}}),
            &store,
        )
        .unwrap();
        let vals: Vec<&str> = c["result"]["completion"]["values"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(vals.contains(&"release"));
        assert!(!vals.contains(&"team"));

        // A non-view argument completes to nothing.
        let c2 = ask(
            &json!({"jsonrpc":"2.0","id":5,"method":"completion/complete",
                    "params":{"argument":{"name":"goal","value":"x"}}}),
            &store,
        )
        .unwrap();
        assert_eq!(
            c2["result"]["completion"]["values"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn logging_setlevel_emits_notifications_above_threshold() {
        let home = db();
        std::fs::create_dir_all(home.join("meta")).unwrap();
        std::fs::write(home.join("meta/team.db"), b"").unwrap();
        let mut sess = Session::new(home.clone(), None, false).unwrap();
        // No threshold yet: a call queues nothing.
        sess.handle(&json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                            "params":{"name":"list_views","arguments":{}}}));
        assert!(sess.drain_notifications().is_empty());

        // Threshold = info: an accepted call emits one info frame.
        sess.handle(&json!({"jsonrpc":"2.0","id":2,"method":"logging/setLevel",
                            "params":{"level":"info"}}));
        sess.handle(&json!({"jsonrpc":"2.0","id":3,"method":"tools/call",
                            "params":{"name":"list_views","arguments":{}}}));
        let notes = sess.drain_notifications();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0]["method"], "notifications/message");
        assert_eq!(notes[0]["params"]["level"], "info");
        // The message names WHICH tool ran, not just the method family.
        assert_eq!(notes[0]["params"]["data"], "tools/call list_views ok");

        // Threshold = warning: a refused call emits a warning frame...
        sess.handle(&json!({"jsonrpc":"2.0","id":4,"method":"logging/setLevel",
                            "params":{"level":"warning"}}));
        sess.handle(&json!({"jsonrpc":"2.0","id":5,"method":"tools/call",
                            "params":{"name":"list_view_tree","arguments":{"view":"ghost"}}}));
        let notes = sess.drain_notifications();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0]["params"]["level"], "warning");
        // ...while an accepted (info-severity) call is now suppressed.
        sess.handle(&json!({"jsonrpc":"2.0","id":6,"method":"tools/call",
                            "params":{"name":"list_views","arguments":{}}}));
        assert!(sess.drain_notifications().is_empty());
    }

    #[test]
    fn json_tools_return_structured_content_matching_the_text() {
        let home = db();
        let store = fstore(&home);
        seed_team_tree(&home);
        let call = |id: i64, name: &str, args: Value| {
            ask(
                &json!({"jsonrpc":"2.0","id":id,"method":"tools/call",
                        "params":{"name":name,"arguments":args}}),
                &store,
            )
            .unwrap()
        };
        let assert_mirrors = |r: &Value| -> Value {
            let text = r["result"]["content"][0]["text"].as_str().unwrap();
            let parsed: Value = serde_json::from_str(text).unwrap();
            assert_eq!(&r["result"]["structuredContent"], &parsed);
            parsed
        };
        let snap = assert_mirrors(&call(1, "list_views", json!({})));
        assert!(snap["physical"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p == "team"));

        let tree = assert_mirrors(&call(
            2,
            "list_view_tree",
            json!({"view": "team", "depth": 3}),
        ));
        assert_eq!(tree["kind"], "physical");
        assert!(tree["entries"].is_array());

        let view = assert_mirrors(&call(3, "get_view", json!({"name": "team"})));
        assert_eq!(view["name"], "team");

        let verdict = assert_mirrors(&call(
            4,
            "validate_definition",
            json!({"name": "safe", "base": "team", "rules": "hide /photos/private"}),
        ));
        assert_eq!(verdict["accepted"], true);
        assert_eq!(verdict["dry_run"], true);

        // A refusal stays a plain error, with no structured payload to trust.
        let bad = call(5, "get_view", json!({"name": "ghost"}));
        assert_eq!(bad["result"]["isError"], true);
        assert!(bad["result"]["structuredContent"].is_null());
    }

    #[test]
    fn resources_expose_the_guide_snapshot_and_a_drilled_tree() {
        let home = db();
        let store = fstore(&home);
        seed_team_tree(&home);
        let d: &dyn ControlStore = &*store;

        // Discovery lists the two store-wide resources first.
        let lr = list_resources(d).unwrap();
        let uris: Vec<&str> = lr["resources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["uri"].as_str().unwrap())
            .collect();
        assert_eq!(uris[0], GUIDE_URI);
        assert_eq!(uris[1], VIEWS_URI);
        assert!(uris.contains(&"grovefs://view/team/tree"));

        // The guide is markdown and still names every tool (the only map an
        // agent gets before tools/list).
        let g = read_resource(d, GUIDE_URI).unwrap();
        assert_eq!(g["contents"][0]["mimeType"], "text/markdown");
        let text = g["contents"][0]["text"].as_str().unwrap();
        for t in TOOL_NAMES {
            assert!(text.contains(t), "guide does not mention {t}");
        }

        // The snapshot is byte-identical to the list_views payload.
        let snap: Value = serde_json::from_str(
            read_resource(d, VIEWS_URI).unwrap()["contents"][0]["text"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(snap, views_snapshot(d).unwrap());

        // `?path=` + `?depth=` drill into one subtree (percent-encoded path).
        let drilled: Value = serde_json::from_str(
            read_resource(d, "grovefs://view/team/tree?path=%2Fphotos&depth=1").unwrap()
                ["contents"][0]["text"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(drilled["root_path"], "/photos");
        let paths: Vec<&str> = drilled["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["path"].as_str().unwrap())
            .collect();
        assert!(paths.contains(&"/photos/private"));
        assert!(!paths.contains(&"/docs"));
        assert!(paths.iter().all(|p| p.starts_with("/photos")));

        // `?max=` bounds the render and says so.
        let capped: Value = serde_json::from_str(
            read_resource(d, "grovefs://view/team/tree?max=1").unwrap()["contents"][0]["text"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(capped["truncated"], true);

        // A malformed bound is refused instead of silently ignored.
        let e = read_resource(d, "grovefs://view/team/tree?depth=x").unwrap_err();
        assert!(e.contains("'depth' must be a non-negative integer"), "{e}");
        // ...and an unknown scheme still names what is supported.
        let e = read_resource(d, "grovefs://bogus").unwrap_err();
        assert!(e.contains("unsupported resource uri"), "{e}");
        assert!(e.contains(GUIDE_URI), "{e}");
    }

    #[test]
    fn completion_is_role_aware() {
        let home = db();
        let store = fstore(&home);
        seed_team_tree(&home);
        let d: &dyn ControlStore = &*store;
        let call = |id: i64, name: &str, args: Value| {
            ask(
                &json!({"jsonrpc":"2.0","id":id,"method":"tools/call",
                        "params":{"name":name,"arguments":args}}),
                &store,
            )
            .unwrap()
        };
        call(1, "alias_view", json!({"name": "pub", "real": "team"}));
        call(
            2,
            "define_derived_view",
            json!({"name": "safe", "base": "team", "rules": "hide /photos/private"}),
        );
        let asked = |id: i64, params: Value| -> Vec<String> {
            ask(
                &json!({"jsonrpc":"2.0","id":id,"method":"completion/complete","params":params}),
                &store,
            )
            .unwrap()["result"]["completion"]["values"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect()
        };
        // A resource URI's {name} completes against every mountable name.
        let all = asked(
            3,
            json!({"ref":{"type":"ref/resource","value":"grovefs://view/{name}/tree"},
                   "argument":{"name":"name","value":""}}),
        );
        assert!(all.contains(&"team".to_string()));
        assert!(all.contains(&"pub".to_string()));
        assert!(all.contains(&"safe".to_string()));
        // reshape-view's `base` can only be a physical tree.
        let bases = asked(
            4,
            json!({"ref":{"type":"ref/prompt","name":"reshape-view"},
                   "argument":{"name":"base","value":""}}),
        );
        assert_eq!(bases, vec!["team".to_string()]);
        // rotate-alias' `old` can only be an existing alias.
        let olds = asked(
            5,
            json!({"ref":{"type":"ref/prompt","name":"rotate-alias"},
                   "argument":{"name":"old","value":""}}),
        );
        assert_eq!(olds, vec!["pub".to_string()]);
        let _ = d;
    }
}

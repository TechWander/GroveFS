//! MCP (Model Context Protocol) integration: lets an AI agent customize
//! data listings through the same governance API the `grovefs view` CLI drives.
//!
//! Two halves live here:
//! - **Shared view-governance ops** ([`define_view`], [`alias_view`],
//!   [`list_views`], [`remove_view`], [`preview_rules`]): every validation and
//!   file mutation of `grovefs view …` in one place, so the CLI and the MCP
//!   server cannot drift (one contract, two entrances).
//! - **A dependency-light MCP server over stdio** ([`serve`]): newline-delimited
//!   JSON-RPC 2.0 implementing the `initialize` / `tools/list` / `tools/call`
//!   core of the 2025-06-18 protocol revision, with eight tools: six governance
//!   writes/validations mapping 1:1 to the ops above (`validate_definition` runs
//!   the FULL publish guard set as a no-write dry run, beyond `preview_rules`'
//!   verb-syntax check) plus two read-only
//!   introspection tools ([`view_tree`], [`describe_view`]) that let an agent
//!   discover the real paths a listing shows (rendering an existing view through
//!   its own lens, or simulating an unpublished `base` + `rules` pair) and read
//!   back a single definition verbatim. No framework crate: the protocol surface
//!   we implement is a handful of methods, and serde_json `Value` is enough.
//!
//! The MCP entry point is `grovefs mcp`: an MCP client (Claude Desktop, any
//! agent runtime) spawns it as a subprocess, and the agent then lists views,
//! inspects their listings, and reshapes them (`hide`/`only`/`map`/`link` rules
//! inline as text — the agent never needs to touch a rules *file*) while live
//! `meta-server` mounts hot-reload the new shape within their cache TTL.
//!
//! The two introspection tools are metadata-only: they open a view's LOCAL
//! SQLite tree read-only and walk it (through the same [`LayoutMeta`] lens a
//! mount uses), touching no object bytes, mounting nothing, and doing no auth.

use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::apikeys::{ApiKeyStore, AuthedKey};
use crate::layout::{canon, valid_view_name, LayoutMeta, LayoutRules, ViewDef};
use crate::meta::{MetaEngine, MetaProvider, NodeKind, ROOT_INO};

/// Protocol revision we advertise when the client does not name one.
pub const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

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
    /// Physical `<db-path>/meta/<view>.db` trees (the storage identities).
    pub physical: Vec<String>,
    /// `meta/views/*.layout` definitions, sorted by name.
    pub defined: Vec<ViewEntry>,
}

/// Run every `define_derived_view` governance guard against a candidate
/// (name, base, rules) WITHOUT touching the filesystem. Any failure comes back
/// as the exact CLI refusal string, so `define_view` (validate-then-write) and
/// the `validate_definition` dry run can never disagree — the agent sees the
/// same wording a real publish would produce. Returns the parsed rules on
/// success so callers can report hide/only/reloc counts.
fn check_derived_definition(
    vbase: &Path,
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
    if vbase.join("meta").join(format!("{name}.db")).is_file() {
        return Err(format!(
            "'{name}' already names a physical view db — pick another name"
        ));
    }
    let dir = vbase.join("meta").join("views");
    // One file slot per name: an existing ALIAS may be replaced (a deliberate
    // governance change) but silently turning a public alias into a read-only
    // shaped view is not `define`'s business.
    let self_def_path = dir.join(format!("{name}.layout"));
    if let Ok(d) = LayoutRules::parse_def_file(&self_def_path) {
        if matches!(d, ViewDef::Alias(_)) {
            return Err(format!(
                "'{name}' is a pure alias — remove it first (`grovefs view rm --name {name}`) before defining a derived view over it"
            ));
        }
    }
    // `base` is supplied by --base, so a base line inside the rules text is
    // rejected here (LayoutRules::parse has no such verb) — one unambiguous
    // source of truth per definition.
    let r = LayoutRules::parse(rules_text).map_err(|e| format!("bad layout rules: {e}"))?;
    Ok(r)
}

/// Every governance op is a flat function over the metadata root — the same
/// `<db-path>` (falling back to `--home`) that `grovefs view` takes. Errors are
/// the exact CLI strings, so an agent sees precisely what a human would.
pub fn define_view(
    vbase: &Path,
    name: &str,
    base: &str,
    rules_text: &str,
) -> Result<Vec<OutLine>, String> {
    let r = check_derived_definition(vbase, name, base, rules_text)?;
    let dir = vbase.join("meta").join("views");
    let self_def_path = dir.join(format!("{name}.layout"));
    let mut out = Vec::new();
    if self_def_path.is_file() {
        out.push(OutLine::Info(format!(
            "view '{name}' already defined — replacing it (live meta-server mounts hot-reload it within their cache TTL)"
        )));
    }
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    std::fs::write(&self_def_path, format!("base {base}\n{rules_text}"))
        .map_err(|e| e.to_string())?;
    out.push(OutLine::Info(format!(
        "defined derived view '{name}' over base '{base}': {} hide, {} only, {} reloc rule(s)",
        r.hide.len(),
        r.only.len(),
        r.reloc.len()
    )));
    out.push(OutLine::Note(format!(
        "note: mounts of '{name}' are read-only (virtual shape) — write to base view '{base}'"
    )));
    if !vbase.join("meta").join(format!("{base}.db")).is_file() {
        out.push(OutLine::Note(format!(
            "note: no local <db-path>/meta/{base}.db yet — fine under PG or lazy creation, otherwise run `grovefs --home ... init --view {base}` first"
        )));
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
    vbase: &Path,
    name: &str,
    base: &str,
    rules_text: &str,
) -> Result<Value, String> {
    let r = check_derived_definition(vbase, name, base, rules_text)?;
    let self_def_path = vbase
        .join("meta")
        .join("views")
        .join(format!("{name}.layout"));
    let replaces_existing = self_def_path.is_file();
    let base_db_exists = vbase.join("meta").join(format!("{base}.db")).is_file();
    Ok(json!({
        "accepted": true,
        "dry_run": true,
        "kind": "derived",
        "name": name,
        "base": base,
        "hide": r.hide.len(),
        "only": r.only.len(),
        "reloc": r.reloc.len(),
        "would_replace_existing_definition": replaces_existing,
        "base_view_db_present": base_db_exists,
        "mount": "read-only (derived)",
        "note": if base_db_exists {
            String::new()
        } else {
            format!("no local <db-path>/meta/{base}.db yet — fine under PG or lazy creation, otherwise run `grovefs --home ... init --view {base}` first")
        },
    }))
}

/// Create/replace a PURE ALIAS (name governance), optionally retiring a
/// previous alias atomically (write-before-delete: the real view stays
/// governed at every instant). Validation mirrors the CLI exactly.
pub fn alias_view(
    vbase: &Path,
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
    if vbase.join("meta").join(format!("{name}.db")).is_file() {
        return Err(format!(
            "'{name}' already names a physical view db — pick another name"
        ));
    }
    let dir = vbase.join("meta").join("views");
    // The alias target must be a PHYSICAL view (the storage identity the
    // alias maps onto), and must not itself be governed: chaining aliases or
    // aliasing a derived view would create resolution ambiguity the server
    // does not model (M_OPEN resolution is single-hop by design).
    let mut alias_to: HashMap<String, String> = HashMap::new();
    let mut derived_names: Vec<String> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for ent in rd.flatten() {
            let p = ent.path();
            if p.extension().and_then(|e| e.to_str()) != Some("layout") {
                continue;
            }
            let stem = match p.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            if let Ok(d) = LayoutRules::parse_def_file(&p) {
                match d {
                    ViewDef::Alias(r) => {
                        alias_to.insert(stem, r);
                    }
                    ViewDef::Derived(..) => derived_names.push(stem),
                }
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
    if let Some(existing) = alias_to.iter().find(|(_, r)| **r == real) {
        if existing.0 != name {
            return Err(format!(
                "real view '{real}' is already governed by alias '{}' — one public name per real view; to rotate it gaplessly use `grovefs view alias --name {name} --real {real} --retire {}`",
                existing.0, existing.0
            ));
        }
    }
    let mut out = Vec::new();
    let slot = dir.join(format!("{name}.layout"));
    // Overwriting an existing definition slot is a governance replacement:
    // surface it as a note so both entrances report it identically.
    if slot.is_file() {
        out.push(OutLine::Note(format!(
            "note: view '{name}' already defined — replacing it with the alias (live mounts re-resolve at their next M_OPEN)"
        )));
    }
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    // Write the new alias BEFORE removing the retired one: the real view is
    // governed by at least one name at every instant, so a leaked old name
    // never opens a window where the real name becomes mountable by itself.
    std::fs::write(dir.join(format!("{name}.layout")), format!("alias {real}\n")).map_err(|e| e.to_string())?;
    if let Some(old) = &retire {
        let _ = std::fs::remove_file(dir.join(format!("{old}.layout")));
        out.push(OutLine::Info(format!(
            "rotated public name for real view '{real}': '{old}' -> '{name}' (gapless — '{real}' stayed governed throughout, '{old}' no longer resolves)"
        )));
        out.push(OutLine::Note(format!(
            "note: ACL grants are keyed to '{real}', so they carried over untouched — legitimate users just remount under '{name}'"
        )));
    } else {
        out.push(OutLine::Info(format!(
            "defined pure alias '{name}' -> real view '{real}'"
        )));
    }
    out.push(OutLine::Note(format!(
        "note: '{real}' is no longer a mount entry point while this definition exists (mounts by its real name are refused, locally and via meta-server); alias mounts show the FULL tree and are WRITABLE"
    )));
    if !vbase.join("meta").join(format!("{real}.db")).is_file() {
        out.push(OutLine::Note(format!(
            "note: no local <db-path>/meta/{real}.db yet — fine under PG or lazy creation, otherwise run `grovefs --home ... init --view {real}` first"
        )));
    }
    Ok(out)
}

/// One human-readable line per definition, byte-identical to `grovefs view list`.
fn entry_line(e: &ViewEntry) -> String {
    match e.kind {
        "alias" => format!(
            "alias '{}' -> real view '{}' (full tree, writable; '{}' not mountable by its own name)",
            e.name, e.real_view, e.real_view
        ),
        _ => format!(
            "view '{}' -> base '{}': {} hide, {} only, {} reloc rule(s) (read-only)",
            e.name, e.real_view, e.hide, e.only, e.reloc
        ),
    }
}

/// Enumerate the store: every physical view db plus every governed definition
/// (derived or alias), resolved from the real files. A malformed definition is
/// an error, exactly as on the CLI (never silently hidden from the agent).
pub fn list_views(vbase: &Path) -> Result<StoreViews, String> {
    let dir = vbase.join("meta").join("views");
    let mut defined: Vec<ViewEntry> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for ent in rd.flatten() {
            let p = ent.path();
            if p.extension().and_then(|e| e.to_str()) == Some("layout") {
                if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                    names.push(stem.to_string());
                }
            }
        }
    }
    names.sort();
    for n in names {
        let d = LayoutRules::parse_def_file(&dir.join(format!("{n}.layout")))
            .map_err(|e| format!("bad view definition for '{n}': {e}"))?;
        defined.push(match d {
            ViewDef::Alias(real) => ViewEntry {
                name: n,
                kind: "alias",
                real_view: real,
                hide: 0,
                only: 0,
                reloc: 0,
            },
            ViewDef::Derived(b, r) => ViewEntry {
                name: n,
                kind: "derived",
                real_view: b,
                hide: r.hide.len(),
                only: r.only.len(),
                reloc: r.reloc.len(),
            },
        });
    }
    let mut physical: Vec<String> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(vbase.join("meta")) {
        for ent in rd.flatten() {
            let p = ent.path();
            if p.extension().and_then(|e| e.to_str()) == Some("db") {
                if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                    // `acl.db` (the view whitelist) and `api_keys.db` (the MCP
                    // key/audit store) are control-plane stores, not view trees
                    // — never surface them as mountable names.
                    if stem != "acl" && stem != "api_keys" {
                        physical.push(stem.to_string());
                    }
                }
            }
        }
    }
    physical.sort();
    physical.dedup();
    Ok(StoreViews { physical, defined })
}

/// CLI-facing rendering of [`list_views`] (byte-identical to the old inline
/// `ViewAction::List` output, including the empty-state line).
pub fn list_views_text(vbase: &Path) -> Result<Vec<OutLine>, String> {
    let s = list_views(vbase)?;
    let dir = vbase.join("meta").join("views");
    let mut out = Vec::new();
    if s.defined.is_empty() {
        out.push(OutLine::Info(format!(
            "no view definitions under {} (every view name maps to its own tree)",
            dir.display()
        )));
    }
    for e in &s.defined {
        out.push(OutLine::Info(entry_line(e)));
    }
    Ok(out)
}

/// Remove a view DEFINITION (derived or alias — the file slot is shared).
/// Underlying data untouched; already-mounted clients keep their resolved
/// lens until they remount (or the server cache evicts it).
pub fn remove_view(vbase: &Path, name: &str) -> Result<Vec<OutLine>, String> {
    if !valid_view_name(name) {
        return Err(format!("invalid view name '{name}'"));
    }
    let p = vbase.join("meta").join("views").join(format!("{name}.layout"));
    if !p.is_file() {
        return Err(format!(
            "no view definition '{name}' under {}",
            vbase.join("meta").join("views").display()
        ));
    }
    std::fs::remove_file(&p).map_err(|e| e.to_string())?;
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
/// its byte size (0 for directories in this read-only render).
#[derive(Debug)]
pub struct TreeEntry {
    pub path: String,
    pub kind: &'static str,
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

/// Open a view's LOCAL SQLite metadata READ-ONLY as a trait object. A missing
/// db is reported honestly: this control-plane tool walks a local store, not a
/// remote/PG-backed tree (there is no metadata root to read here).
fn open_meta_ro(vbase: &Path, view: &str) -> Result<Arc<dyn MetaProvider>, String> {
    let p = vbase.join("meta").join(format!("{view}.db"));
    if !p.is_file() {
        return Err(format!(
            "no local metadata tree for view '{view}' ({}) — list_view_tree introspects a LOCAL SQLite store only; a remote/PG-backed tree is not walked here",
            p.display()
        ));
    }
    MetaEngine::open_ex(&p, true)
        .map(|e| -> Arc<dyn MetaProvider> { Arc::new(e) })
        .map_err(|e| format!("cannot open view '{view}' metadata read-only: {e}"))
}

/// Depth-first render of `provider`'s display tree under `ino`, appending
/// absolute display paths to `out`. `readdir` already sorts by name, so the
/// listing matches what a mount shows; synthesized scaffolding dirs carry a
/// virtual ino that `LayoutMeta::readdir` resolves on the next recursion.
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
        out.push(TreeEntry { path: child.clone(), kind, size: n.size });
        if n.is_dir() && depth < max_depth {
            walk_tree(provider, n.id, &child, depth + 1, max_depth, max, out, truncated);
            if *truncated {
                return;
            }
        }
    }
}

/// Render the listing a mount of the target would show — metadata only, no
/// data IO, no mount. See [`TreeTarget`] for the two ways to ask.
pub fn view_tree(
    vbase: &Path,
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
                let slot = vbase.join("meta").join("views").join(format!("{view}.layout"));
                if slot.is_file() {
                    match LayoutRules::parse_def_file(&slot)
                        .map_err(|e| format!("bad view definition for '{view}': {e}"))? {
                        ViewDef::Derived(base, rules) => {
                            let eng = open_meta_ro(vbase, &base)?;
                            let lens: Arc<dyn MetaProvider> =
                                Arc::new(LayoutMeta::new(eng, rules));
                            (lens, "derived", base)
                        }
                        ViewDef::Alias(real) => {
                            (open_meta_ro(vbase, &real)?, "alias", real)
                        }
                    }
                } else {
                    (open_meta_ro(vbase, view)?, "physical", view.to_string())
                }
            }
            TreeTarget::Simulate { base, rules } => {
                if !valid_view_name(base) {
                    return Err(format!("invalid base view name '{base}'"));
                }
                let r = LayoutRules::parse(rules)
                    .map_err(|e| format!("bad layout rules: {e}"))?;
                let eng = open_meta_ro(vbase, base)?;
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
    walk_tree(provider.as_ref(), start_ino, &root_path, 0, max_depth, cap, &mut entries, &mut truncated);
    Ok(ShapeTree { kind, rendered_from, root_path, entries, truncated })
}

/// Full detail of ONE view slot for read-modify-write editing: what `list_views`
/// summarizes, plus the verbatim definition text (derived rules or the alias
/// target) and the mount semantics — never hiding a malformed definition.
pub fn describe_view(vbase: &Path, name: &str) -> Result<Value, String> {
    if !valid_view_name(name) {
        return Err(format!("invalid view name '{name}'"));
    }
    let slot = vbase.join("meta").join("views").join(format!("{name}.layout"));
    if slot.is_file() {
        let text = std::fs::read_to_string(&slot)
            .map_err(|e| format!("cannot read definition '{}': {e}", slot.display()))?;
        return Ok(match LayoutRules::parse_def_file(&slot)
            .map_err(|e| format!("bad view definition for '{name}': {e}"))?
        {
            ViewDef::Alias(real) => json!({
                "name": name, "kind": "alias", "real_view": real,
                "mount": "read-write (full tree under the public name)",
                "real_name_mountable": false,
                "definition": text,
            }),
            ViewDef::Derived(base, r) => json!({
                "name": name, "kind": "derived", "base": base,
                "mount": "read-only (virtual shape; writes go to the base view)",
                "hide": r.hide.len(), "only": r.only.len(), "reloc": r.reloc.len(),
                "definition": text,
            }),
        });
    }
    if vbase.join("meta").join(format!("{name}.db")).is_file() {
        return Ok(json!({
            "name": name, "kind": "physical",
            "mount": "read-write (this is a storage identity, not a governed definition)",
        }));
    }
    Err(format!(
        "no view '{name}' here (not a definition under meta/views/ nor a physical meta/{name}.db) — `list_views` names what exists"
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

/// The two resource templates advertised (fixed shape, `{name}` filled per view).
pub fn resource_templates() -> Value {
    json!([
        { "uriTemplate": "grovefs://view/{name}/definition",
          "name": "view-definition",
          "description": "Verbatim governance definition of one view slot (derived rules or the alias target); a physical view reports its mount semantics instead.",
          "mimeType": "application/json" },
        { "uriTemplate": "grovefs://view/{name}/tree",
          "name": "view-tree",
          "description": "The directory listing a mount of this view actually shows, rendered through its lens (metadata only — no mount, no data bytes).",
          "mimeType": "application/json" },
    ])
}

/// Enumerate every view slot in `vbase` as MCP resources (physical → tree;
/// derived/alias → definition + tree).
pub fn list_resources(vbase: &Path) -> Result<Value, String> {
    let s = list_views(vbase)?;
    let mut resources: Vec<Value> = Vec::new();
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

/// Read one resource by URI, delegating to `describe_view` / `view_tree` so the
/// bytes match the corresponding tool result exactly.
pub fn read_resource(vbase: &Path, uri: &str) -> Result<Value, String> {
    let rest = uri
        .strip_prefix(VIEW_URI_PREFIX)
        .ok_or_else(|| format!("unsupported resource uri '{uri}' (expected {VIEW_URI_PREFIX}<name>/(definition|tree))"))?;
    let (name, facet) = rest
        .rsplit_once('/')
        .ok_or_else(|| format!("malformed resource uri '{uri}' (expected .../<name>/(definition|tree))"))?;
    if name.is_empty() {
        return Err(format!("resource uri '{uri}' names no view"));
    }
    let text = match facet {
        "definition" => serde_json::to_string_pretty(&describe_view(vbase, name)?).unwrap_or_default(),
        "tree" => {
            let t = view_tree(vbase, TreeTarget::Existing(name), None, 0, 0)?;
            let dir_count = t.entries.iter().filter(|e| e.kind == "dir").count();
            let entries: Vec<Value> = t
                .entries
                .iter()
                .map(|e| json!({ "path": e.path, "kind": e.kind, "size": e.size }))
                .collect();
            serde_json::to_string_pretty(&json!({
                "kind": t.kind,
                "rendered_from": t.rendered_from,
                "root_path": t.root_path,
                "truncated": t.truncated,
                "dir_count": dir_count,
                "file_count": t.entries.len() - dir_count,
                "entries": entries,
            }))
            .unwrap_or_default()
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
"Gaplessly rotate the public alias for real view '{real}': replace '{old}' with '{new}' in ONE command so the storage name is never left mountable.\n1. alias_view {{\"name\":\"{new}\",\"real\":\"{real}\",\"retire\":\"{old}\"}} — write-before-delete: '{new}' resolves, '{old}' stops resolving, '{real}' stays governed (not directly mountable) throughout, and since ACL grants are keyed to '{real}' they carry over untouched.\n2. list_views — confirm '{new}' -> '{real}' is present and '{old}' is gone.\n3. Any client still on '{old}' remounts under '{new}' (rotation only affects NEW mounts; established connections drain on close)."
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
/// existing view names in the LOCAL store matching the typed prefix. Discovery
/// only — it never reaches a remote/PG backend tree.
pub fn complete(vbase: &Path, argument: &Value) -> Result<Value, String> {
    let name = argument.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let value = argument.get("value").and_then(|v| v.as_str()).unwrap_or("");
    let wants_view = matches!(name, "name" | "base" | "view" | "real");
    if !wants_view {
        return Ok(json!({ "completion": { "values": [], "total": 0, "hasMore": false } }));
    }
    let s = list_views(vbase)?;
    let mut names: Vec<String> = s.physical.clone();
    names.extend(s.defined.iter().map(|e| e.name.clone()));
    let lower = value.to_lowercase();
    let matched: Vec<String> = names
        .into_iter()
        .filter(|n| n.to_lowercase().starts_with(&lower))
        .collect();
    let total = matched.len();
    let values: Vec<String> = matched.into_iter().take(100).collect();
    let has_more = total > values.len();
    Ok(json!({ "completion": { "values": values, "total": total, "hasMore": has_more } }))
}

// ---------------------------------------------------------------------------
// The MCP server proper (stdio, newline-delimited JSON-RPC 2.0)
// ---------------------------------------------------------------------------

/// JSON-schema `inputSchema` block shared by the tools' `name`-style args.
fn str_prop(desc: &str) -> Value {
    json!({ "type": "string", "description": desc })
}

/// The five exposed tools, in stable order, with self-describing schemas —
/// an agent reads these descriptions as its whole operating manual.
pub fn tool_defs() -> Value {
    json!([
        {
            "name": "list_views",
            "description": "List every view in the store: physical view trees (storage identities, mountable read-write) and governed definitions — derived views (read-only shaped lenses over a base view) and pure aliases (writable public names for a real view; while an alias exists its real name is not a mount entry point). Answers as JSON: {physical:[...], defined:[{name,kind,real_view,hide,only,reloc}]}.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "db_path": str_prop("Metadata root holding meta/*.db and meta/views/*.layout; defaults to the server's --db-path/--home")
                }
            }
        },
        {
            "name": "define_derived_view",
            "description": "Create or replace a DERIVED view: a named, centrally-governed presentation lens over the physical base view, built from `rules` text (one verb per line, # comments allowed): `hide <src>` (prune a subtree), `only <src>` (serve just that subtree), `map <src> <dst>` (re-home entries to a virtual display path), `link <src> <dst>` (add a second name for the same entries). Mounts of the derived name are READ-ONLY — writes go to the base view and show up through the lens within the client cache TTL. Against a live meta-server, remounts are not needed: the server hot-reloads the definition and mounts converge within ≤2s. A `base` line inside rules is rejected (the base is the `base` argument).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": str_prop("Derived view name (one safe path element, must differ from base)"),
                    "base": str_prop("Physical view whose tree this lens displays"),
                    "rules": str_prop("Layout rules text, e.g. \"# only docs\\nonly /docs\""),
                    "db_path": str_prop("Metadata root; defaults to the server's --db-path/--home")
                },
                "required": ["name", "base", "rules"]
            }
        },
        {
            "name": "alias_view",
            "description": "Create or replace a PURE ALIAS: a public writable name for a physical view (name governance, not shape). The alias mounts the FULL tree read-write, and while it exists the real name is refused as a mount entry point — locally and via meta-server. ACL grants stay keyed to the real view. Pass `retire` to rotate a previous alias of the SAME real view gaplessly (write-before-delete: the real name never becomes mountable). Chaining aliases and aliasing a derived view are rejected.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": str_prop("Public alias name (must differ from the real view name)"),
                    "real": str_prop("Physical view the alias maps onto"),
                    "retire": str_prop("Previous alias name of the same real view to retire atomically (optional)"),
                    "db_path": str_prop("Metadata root; defaults to the server's --db-path/--home")
                },
                "required": ["name", "real"]
            }
        },
        {
            "name": "remove_view",
            "description": "Remove a view DEFINITION by name (derived or alias — they share a slot). Underlying data is untouched; already-mounted clients keep their resolved lens until remount. Removing an alias makes the real name mountable again for NEW mounts.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": str_prop("Definition name to remove"),
                    "db_path": str_prop("Metadata root; defaults to the server's --db-path/--home")
                },
                "required": ["name"]
            }
        },
        {
            "name": "preview_rules",
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
            "description": "Pre-publish DRY RUN of a whole derived-view definition — not just the verb syntax (`preview_rules`) but every governance guard `define_derived_view` enforces: invalid names, name==base, shadowing a physical <name>.db, replacing a pure alias, bad rules text. Returns the EXACT refusal string a real publish would give (so nothing lands by surprise), or on success a JSON verdict {accepted:true, dry_run:true, hide/only/reloc counts, would_replace_existing_definition, base_view_db_present}. Writes NOTHING. Call this right before define_derived_view to confirm a name+base+rules trio is acceptable.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": str_prop("Candidate derived view name to validate"),
                    "base": str_prop("Physical base view the candidate lens sits over"),
                    "rules": str_prop("Candidate layout rules text (inline, same as define_derived_view)"),
                    "db_path": str_prop("Metadata root; defaults to the server's --db-path/--home")
                },
                "required": ["name", "base", "rules"]
            }
        },
        {
            "name": "list_view_tree",
            "description": "DISCOVERY + SHAPE PREVIEW: render the directory listing a mount would actually show, read from the view's LOCAL metadata (no mount, no data IO). Give `view` (physical, derived — rendered through its real lens including synthesized dirs, or alias — the full real tree) to inspect an existing listing, OR `base` + inline `rules` to preview an UNPUBLISHED lens before committing it. Returns JSON {kind, rendered_from, root_path, dir_count, file_count, truncated, entries:[{path,kind,size}]} where each `path` is an absolute DISPLAY path — exactly the string to reuse in a hide/only/map rule. Use this to learn which paths exist instead of guessing. Remote/PG-backed trees are not walked (needs a local meta/<view>.db).",
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
            }
        },
        {
            "name": "get_view",
            "description": "Describe ONE view in full for read-modify-write editing: for a derived view the verbatim `definition` text plus base and hide/only/reloc counts and the read-only mount note; for an alias the real view and that its real name is not mountable; for a physical view that it is a storage identity. `list_views` summarizes every slot; use this to read a single lens's exact rules before changing them. Errors on an unknown name rather than hiding a malformed definition.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": str_prop("View name to describe (definition slot or physical view db)"),
                    "db_path": str_prop("Metadata root; defaults to the server's --db-path/--home")
                },
                "required": ["name"]
            }
        }
    ])
}

/// Dispatch one `tools/call`; returns the JSON-RPC `result` object.
/// Governance failures come back INSIDE the result (`isError: true` + the CLI
/// error text), per MCP convention — the JSON-RPC layer stays for protocol
/// errors only, so the agent's client surfaces the message instead of dying.
pub fn call_tool(name: &str, args: &Value, default_db: &Path) -> Value {
    let err = |msg: String| json!({ "content": [{ "type": "text", "text": msg }], "isError": true });
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
            .ok_or_else(|| err(format!("missing string argument '{key}'")))
    };
    let db = args
        .get("db_path")
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .unwrap_or_else(|| default_db.to_path_buf());
    match name {
        "list_views" => match list_views(&db) {
            Ok(store) => {
                let defined: Vec<Value> = store
                    .defined
                    .iter()
                    .map(|e| {
                        json!({
                            "name": e.name, "kind": e.kind, "real_view": e.real_view,
                            "hide": e.hide, "only": e.only, "reloc": e.reloc,
                            "mount": if e.kind == "alias" { "read-write (alias)" } else { "read-only (derived)" },
                            "line": entry_line(e),
                        })
                    })
                    .collect();
                let text = serde_json::to_string_pretty(&json!({
                    "db_path": db.display().to_string(),
                    "physical": store.physical,
                    "defined": defined,
                }))
                .unwrap_or_default();
                json!({ "content": [{ "type": "text", "text": text }] })
            }
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
            match define_view(&db, &vname, &vbase, &rules) {
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
            match alias_view(&db, &vname, &real, retire) {
                Ok(lines) => ok_lines(lines),
                Err(e) => err(e),
            }
        }
        "remove_view" => {
            let vname = match s("name") {
                Ok(v) => v,
                Err(e) => return e,
            };
            match remove_view(&db, &vname) {
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
            match validate_definition(&db, &vname, &vbase, &rules) {
                Ok(v) => json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&v).unwrap_or_default() }] }),
                Err(e) => err(e),
            }
        }
        "get_view" => {
            let vname = match s("name") {
                Ok(v) => v,
                Err(e) => return e,
            };
            match describe_view(&db, &vname) {
                Ok(v) => json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&v).unwrap_or_default() }] }),
                Err(e) => err(e),
            }
        }
        "list_view_tree" => {
            let view = args.get("view").and_then(|v| v.as_str());
            let base = args.get("base").and_then(|v| v.as_str());
            let rules = args.get("rules").and_then(|v| v.as_str());
            let path = args.get("path").and_then(|v| v.as_str());
            let depth = args.get("depth").and_then(|v| v.as_u64()).map(|v| v as usize).unwrap_or(0);
            let max = args.get("max").and_then(|v| v.as_u64()).map(|v| v as usize).unwrap_or(0);
            let target = match (view, base, rules) {
                (Some(v), _, _) => TreeTarget::Existing(v),
                (None, Some(b), Some(r)) => TreeTarget::Simulate { base: b, rules: r },
                (None, Some(b), None) => {
                    return err(format!(
                        "'base' also needs inline 'rules' to simulate a lens (got base '{b}' alone)"
                    ))
                }
                (None, None, _) => {
                    return err(
                        "provide 'view' (an existing view) or 'base' + 'rules' (a simulated lens)"
                            .into(),
                    )
                }
            };
            match view_tree(&db, target, path, depth, max) {
                Ok(t) => {
                    let dir_count = t.entries.iter().filter(|e| e.kind == "dir").count();
                    let file_count = t.entries.len() - dir_count;
                    let entries: Vec<Value> = t
                        .entries
                        .iter()
                        .map(|e| json!({ "path": e.path, "kind": e.kind, "size": e.size }))
                        .collect();
                    let text = serde_json::to_string_pretty(&json!({
                        "kind": t.kind,
                        "rendered_from": t.rendered_from,
                        "root_path": t.root_path,
                        "truncated": t.truncated,
                        "dir_count": dir_count,
                        "file_count": file_count,
                        "entries": entries,
                    }))
                    .unwrap_or_default();
                    json!({ "content": [{ "type": "text", "text": text }] })
                }
                Err(e) => err(e),
            }
        }
        other => err(format!("unknown tool '{other}'")),
    }
}

fn result_frame(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_frame(id: Value, code: i64, message: String) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// Answer one request frame. `None` = a notification (no response is written).
pub fn handle_request(req: &Value, default_db: &Path, client_version: &mut Option<String>) -> Option<Value> {
    // Notifications carry no id — they must never be answered.
    let id = req.get("id").filter(|v| !v.is_null()).cloned();
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Object(Map::new()));
    let result: Value = match method {
        "initialize" => {
            *client_version = params
                .get("protocolVersion")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let advertised = client_version
                .clone()
                .unwrap_or_else(|| MCP_PROTOCOL_VERSION.to_string());
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
                "instructions": "Customize data listings by governing SERVER-SIDE views over one store. If the server was started with `--require-api-key`, pass your credential as `apiKey` (a `grovefs apikey create` key, format '<id>:<secret>') in the initialize params — the state-touching methods (tools/list, tools/call, resources/list, resources/read, completion/complete) are refused until it authenticates, and every call is logged against your key. Discover: list_views (all slots) and get_view (one slot's exact rules). Explore/preview shape: list_view_tree renders a view's real directory listing — or a base+rules lens SIMULATED before you commit it — so you use exact display paths instead of guessing. Reshape: define_derived_view (hide/only/map/link rules, read-only lenses hot-reloaded into live meta-server mounts), alias_view (public writable names), remove_view. Validate before publishing: preview_rules (dry-run a rules text's verb syntax) and validate_definition (dry-run a whole name+base+rules against every governance guard, writing nothing). Beyond tools, the same governance is exposed as MCP RESOURCES (grovefs://view/<name>/definition and /tree, read via resources/read), PROMPTS (reshape-view and rotate-alias playbooks via prompts/get), and COMPLETION (completion/complete fills a view-name argument from the store); set a threshold with logging/setLevel to receive notifications/message frames per call. Rule paths are absolute view-tree paths; a base line inside rules is rejected. Introspection is metadata-only over a LOCAL store: it never mounts and reads no data bytes.",
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({ "tools": tool_defs() }),
        "tools/call" => {
            let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| Value::Object(Map::new()));
            call_tool(name, &args, default_db)
        }
        // Resources: the same governance exposed as attachable context, so a
        // client can read a view's definition or rendered tree without a tool
        // call. Errors are protocol-level (a bad URI is not a governance refusal).
        "resources/templates/list" => json!({ "resourceTemplates": resource_templates() }),
        "resources/list" => match list_resources(default_db) {
            Ok(v) => v,
            Err(e) => return id.map(|id| error_frame(id, -32603, e)),
        },
        "resources/read" => {
            let uri = match params.get("uri").and_then(|u| u.as_str()) {
                Some(u) => u,
                None => {
                    return id
                        .map(|id| error_frame(id, -32602, "resources/read requires a 'uri'".into()))
                }
            };
            match read_resource(default_db, uri) {
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
        // Completion: fill a view-name argument from the store's own names.
        "completion/complete" => {
            let a = params
                .get("argument")
                .cloned()
                .unwrap_or(Value::Object(Map::new()));
            match complete(default_db, &a) {
                Ok(v) => v,
                Err(e) => return id.map(|id| error_frame(id, -32602, e)),
            }
        }
        // Logging: the ack is stateless; the threshold itself is held per session
        // (see `Session::handle`), which decides which notifications/message to emit.
        "logging/setLevel" => json!({}),
        // Client notifications (no id) are accepted silently — never answered.
        m if m.starts_with("notifications/") || m == "initialized" => {
            return None;
        }
        // Anything else: method-not-found only when an id was present (a real
        // request); a bare unknown notification stays silent.
        _ => {
            return id.map(|id| error_frame(id, -32601, format!("method not found: {method}")))
        }
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
                (false, clip(v["result"]["content"][0]["text"].as_str().unwrap_or("")))
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
        "tools/call" => params.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string(),
        "resources/read" => params.get("uri").and_then(|u| u.as_str()).unwrap_or("").to_string(),
        _ => String::new(),
    }
}

/// Per-connection server state: the shared governance `default_db`, an optional
/// API-key store (audit log + authentication), whether the gated methods
/// (`tools/list`, `tools/call`) require an authenticated key, and — once a key
/// authenticates — which identity this session is acting as (so every logged
/// call is attributed). `initialize`/`ping` stay open so a client can always
/// complete the handshake and present its key.
pub struct Session {
    default_db: PathBuf,
    store: Option<ApiKeyStore>,
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
    /// Build a session over `default_db`. The API-key store is opened
    /// best-effort (`<db>/meta/api_keys.db`): if it cannot be opened the server
    /// still runs but cannot authenticate or log. When `require_key` is set and
    /// `GROVEFS_MCP_API_KEY` is exported, the whole server is pre-authenticated
    /// (for MCP clients that cannot customize the `initialize` handshake).
    pub fn new(default_db: PathBuf, require_key: bool) -> Self {
        let store = ApiKeyStore::open(&default_db).ok();
        let mut authed = None;
        if require_key {
            if let Some(s) = &store {
                if let Ok(env_key) = std::env::var("GROVEFS_MCP_API_KEY") {
                    authed = s.authenticate(&env_key).ok();
                }
            }
        }
        Self {
            default_db,
            store,
            require_key,
            authed,
            client_version: None,
            log_min: None,
            pending: Vec::new(),
        }
    }

    /// Try to authenticate a key presented in `initialize`, logging the attempt
    /// either way (a failed key is a security-relevant event worth an audit
    /// row). Success stamps the session identity used to attribute later calls.
    fn authenticate(&mut self, key: &str) {
        if let Some(s) = &self.store {
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
            if let Some(s) = &self.store {
                let subject = audit_subject(method, &params);
                let _ = s.log_call(None, method, &subject, false, "unauthorized: no valid api key");
            }
            let msg = "authentication required: present a valid apiKey in the initialize params (or export GROVEFS_MCP_API_KEY for the server)";
            return id.map(|id| error_frame(id, -32001, msg.to_string()));
        }

        let resp = handle_request(req, &self.default_db, &mut self.client_version);

        // Audit the state-touching calls (authenticated or not, success or
        // failure), attributed to the session key when one authenticated. Tools
        // and resource reads both reveal or change per-view state, so both land
        // in the audit log; pure discovery/list templates do not.
        if matches!(method, "tools/call" | "resources/read") {
            if let Some(s) = &self.store {
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
                let (lvl, rank, msg) = if ok {
                    ("info", 20, format!("{method} ok"))
                } else {
                    ("warning", 40, format!("{method} failed: {reason}"))
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
/// is appended to `<db>/meta/api_keys.db`'s audit log either way.
pub fn serve(default_db: PathBuf, require_key: bool) -> std::io::Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut session = Session::new(default_db, require_key);
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

    fn ask(req: &Value, default_db: &Path) -> Option<Value> {
        let mut cv = None;
        handle_request(req, default_db, &mut cv)
    }

    #[test]
    fn initialize_echoes_client_version() {
        let r = ask(
            &json!({"jsonrpc":"2.0","id":1,"method":"initialize",
                    "params":{"protocolVersion":"2024-11-05","capabilities":{}}}),
            Path::new("/"),
        )
        .unwrap();
        assert_eq!(r["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(r["result"]["serverInfo"]["name"], "grovefs");
        assert!(r["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn notifications_are_never_answered() {
        assert!(
            ask(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}), Path::new("/")).is_none()
        );
        // A request (with id) for an unknown method gets method-not-found.
        let r = ask(&json!({"jsonrpc":"2.0","id":2,"method":"foo/bar"}), Path::new("/")).unwrap();
        assert_eq!(r["error"]["code"], -32601);
    }

    #[test]
    fn tools_list_advertises_the_eight_governance_tools() {
        let r = ask(&json!({"jsonrpc":"2.0","id":3,"method":"tools/list"}), Path::new("/")).unwrap();
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
                "remove_view",
                "preview_rules",
                "validate_definition",
                "list_view_tree",
                "get_view"
            ]
        );
        for t in r["result"]["tools"].as_array().unwrap() {
            assert!(t["inputSchema"]["type"] == "object" && t["description"].is_string());
        }
    }

    #[test]
    fn full_governance_round_trip_over_tool_calls() {
        let home = db();
        // Seed one physical view db so define/alias see a realistic store.
        std::fs::create_dir_all(home.join("meta")).unwrap();
        std::fs::write(home.join("meta/team.db"), b"").unwrap();
        let call = |id, name: &str, args: Value| {
            ask(
                &json!({"jsonrpc":"2.0","id":id,"method":"tools/call",
                        "params":{"name":name,"arguments":args}}),
                &home,
            )
            .unwrap()
        };
        let text = |v: &Value| -> String {
            v["result"]["content"][0]["text"].as_str().unwrap().to_string()
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
        assert!(text(&d).contains("defined derived view 'docsonly' over base 'team': 0 hide, 1 only, 0 reloc rule(s)"));
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
        let a = call(
            13,
            "alias_view",
            json!({"name": "public", "real": "team"}),
        );
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
        assert!(text(&rot).contains("rotated public name for real view 'team': 'public' -> 'public2'"));
        assert!(!home.join("meta/views/public.layout").exists());
        // remove both definitions; data (team.db) untouched.
        for (i, n) in [(16, "docsonly"), (17, "public2")] {
            let r = call(i, "remove_view", json!({"name": n}));
            assert!(text(&r).contains("removed view definition"));
        }
        assert!(home.join("meta/team.db").is_file());
        let gone = call(18, "remove_view", json!({"name": "docsonly"}));
        assert_eq!(gone["result"]["isError"], true);
        assert!(text(&gone).starts_with("no view definition 'docsonly' under"));
    }

    #[test]
    fn missing_argument_is_a_tool_error_not_a_crash() {
        let home = db();
        let r = ask(
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                    "params":{"name":"define_derived_view","arguments":{"name":"x"}}}),
            &home,
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
        std::fs::create_dir_all(home.join("meta")).unwrap();
        // Seed a REAL tree with the normal engine, then drop the writer so the
        // read-only walk below opens a clean handle: /docs/a.md and
        // /photos/private/x.jpg.
        {
            let m = MetaEngine::open(&home.join("meta/team.db")).unwrap();
            let docs = m.create_node(ROOT_INO, "docs", true, None, 0o0755, 0, 0).unwrap();
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
        let call = |id: i64, name: &str, args: Value| {
            ask(
                &json!({"jsonrpc":"2.0","id":id,"method":"tools/call",
                        "params":{"name":name,"arguments":args}}),
                &home,
            )
            .unwrap()
        };
        let text = |v: &Value| -> String {
            v["result"]["content"][0]["text"].as_str().unwrap().to_string()
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
        assert!(pg["definition"].as_str().unwrap().contains("hide /photos/private"));
        // (5) a real listing under an alias resolves to the FULL real tree.
        call(25, "alias_view", json!({"name": "pub", "real": "team"}));
        let a = call(26, "list_view_tree", json!({"view": "pub", "depth": 10}));
        let pa: Value = serde_json::from_str(&text(&a)).unwrap();
        assert_eq!(pa["kind"], "alias");
        assert!(paths(&a).contains(&"/photos/private/x.jpg".to_string()));
    }

    #[test]
    fn list_view_tree_missing_local_db_is_a_tool_error() {
        let home = db();
        std::fs::create_dir_all(home.join("meta")).unwrap();
        // No meta/ghost.db exists → an honest control-plane refusal, not a crash.
        let r = ask(
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                    "params":{"name":"list_view_tree","arguments":{"view":"ghost"}}}),
            &home,
        )
        .unwrap();
        assert_eq!(r["result"]["isError"], true);
        assert!(r["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("no local metadata tree for view 'ghost'"));
    }

    #[test]
    fn api_key_gate_refuses_then_admits_and_logs() {
        let home = db();
        std::fs::create_dir_all(home.join("meta")).unwrap();
        std::fs::write(home.join("meta/team.db"), b"").unwrap();
        // Mint a key out-of-band, exactly as `grovefs apikey create` would.
        let store = ApiKeyStore::open(&home).unwrap();
        let (_id, key) = store.create("agent-a").unwrap();

        let mut sess = Session::new(home.clone(), true);
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
        let define = calls.iter().find(|c| c.tool == "define_derived_view").unwrap();
        assert!(define.ok);
        assert_eq!(define.key_label.as_deref(), Some("agent-a"));
        // the refused tools/list and the bad-key initialize are BOTH auditable.
        assert!(calls.iter().any(|c| !c.ok && c.detail.contains("unauthorized")));
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
        let mut sess = Session::new(home.clone(), false);
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
        std::fs::create_dir_all(home.join("meta/views")).unwrap();
        // A physical view backed by a real engine tree, plus a derived definition.
        {
            let m = MetaEngine::open(&home.join("meta/team.db")).unwrap();
            let docs = m.create_node(ROOT_INO, "docs", true, None, 0o0755, 0, 0).unwrap();
            m.create_node(docs, "a.md", false, Some("k-a"), 0o0644, 0, 0).unwrap();
        }
        std::fs::write(
            home.join("meta/views/release.layout"),
            b"base team\nhide /photos/private\n",
        )
        .unwrap();

        let list = ask(&json!({"jsonrpc":"2.0","id":1,"method":"resources/list"}), &home).unwrap();
        let uris: Vec<&str> = list["result"]["resources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["uri"].as_str().unwrap())
            .collect();
        assert!(uris.contains(&"grovefs://view/team/tree"));
        assert!(uris.contains(&"grovefs://view/release/definition"));
        assert!(uris.contains(&"grovefs://view/release/tree"));

        let tmpl =
            ask(&json!({"jsonrpc":"2.0","id":2,"method":"resources/templates/list"}), &home)
                .unwrap();
        assert_eq!(tmpl["result"]["resourceTemplates"].as_array().unwrap().len(), 2);

        // The definition resource mirrors get_view (carries the verbatim rule).
        let def = ask(
            &json!({"jsonrpc":"2.0","id":3,"method":"resources/read",
                    "params":{"uri":"grovefs://view/release/definition"}}),
            &home,
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
            &home,
        )
        .unwrap();
        let ttext = tree["result"]["contents"][0]["text"].as_str().unwrap();
        assert!(ttext.contains("/docs") && ttext.contains("a.md"));

        // A bad facet is a protocol error (not an in-result refusal, not a crash).
        let bad = ask(
            &json!({"jsonrpc":"2.0","id":5,"method":"resources/read",
                    "params":{"uri":"grovefs://view/team/bogus"}}),
            &home,
        )
        .unwrap();
        assert_eq!(bad["error"]["code"], -32002);

        // Under --require-api-key, resources/read is gated like the tools.
        let mut sess = Session::new(home.clone(), true);
        let refused = sess
            .handle(&json!({"jsonrpc":"2.0","id":6,"method":"resources/read",
                            "params":{"uri":"grovefs://view/release/definition"}}))
            .unwrap();
        assert_eq!(refused["error"]["code"], -32001);
    }

    #[test]
    fn prompts_are_instantiated_and_completion_lists_view_names() {
        let home = db();
        std::fs::create_dir_all(home.join("meta/views")).unwrap();
        std::fs::write(home.join("meta/team.db"), b"").unwrap();
        std::fs::write(
            home.join("meta/views/release.layout"),
            b"base team\nhide /x\n",
        )
        .unwrap();

        let pl = ask(&json!({"jsonrpc":"2.0","id":1,"method":"prompts/list"}), &home).unwrap();
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
            &home,
        )
        .unwrap();
        let msg = pg["result"]["messages"][0]["content"]["text"].as_str().unwrap();
        assert!(msg.contains("'team'") && msg.contains("drop privates") && msg.contains("validate_definition"));

        // A missing required argument is a protocol error.
        let perr = ask(
            &json!({"jsonrpc":"2.0","id":3,"method":"prompts/get",
                    "params":{"name":"rotate-alias","arguments":{"real":"team"}}}),
            &home,
        )
        .unwrap();
        assert_eq!(perr["error"]["code"], -32602);

        // Completion returns the store's own view names by prefix, case-insensitively.
        let c = ask(
            &json!({"jsonrpc":"2.0","id":4,"method":"completion/complete",
                    "params":{"ref":{"type":"ref/resource","value":"grovefs://view/{name}/tree"},
                              "argument":{"name":"name","value":"RE"}}}),
            &home,
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
            &home,
        )
        .unwrap();
        assert_eq!(c2["result"]["completion"]["values"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn logging_setlevel_emits_notifications_above_threshold() {
        let home = db();
        std::fs::create_dir_all(home.join("meta")).unwrap();
        std::fs::write(home.join("meta/team.db"), b"").unwrap();
        let mut sess = Session::new(home.clone(), false);
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
}

//! grovefs — a view-view FUSE filesystem.
//!
//! One physical store (`<base>/blocks`, shared), one *view* database per view
//! (`<base>/meta/<view>.db`). Mounting as a given view surfaces only that
//! view tree. Data IO is fully decoupled from NFS: it goes through the
//! pluggable `StorageBackend` trait (local disk today, gRPC to a cluster later).
//!
//! Subcommands:
//!   init   — create a view DB + seed some folders/files from a manifest
//!   mount  — mount a view at a mountpoint (foreground; `--layout` lens, or a
//!            server-side derived view name resolved from `meta/views/`)
//!   umount — unmount
//!   view   — manage server-side view definitions (define/alias/list/rm):
//!            derived lenses (shape governance, mounted read-only) and pure
//!            aliases (name governance: the alias is the writable public
//!            face of a physical view, whose real name stops being mountable)
//!   mcp    — serve the view-governance tools over MCP (stdio) so an AI agent
//!            can customize data listings through the same validated API
//!   import — mirror an EXISTING local directory tree into a view, zero-copy
//!            (hard link into blocks/) where the filesystem allows it
//!   acl    — per-view uid whitelist
//!   data-server / meta-server — centralized storage/metadata services

use clap::{Parser, Subcommand};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;
use grovefs::acl::AclStore;
use grovefs::backend;
use grovefs::fs_impl::GroveFS;
use grovefs::meta;
use grovefs::meta::MetaEngine;
use grovefs::meta::MetaProvider;
use grovefs::remote;

#[derive(Parser)]
#[command(name = "grove", version, about = "User-view FUSE filesystem, no NFS")]
struct Cli {
    /// Base directory holding `blocks/` (data) and `meta/` (view DBs) for LOCAL
    /// mode (`init`, a mount without `--meta`/`--data`). Commands that talk to
    /// one specific store name their own path instead (`meta-server --db-path`,
    /// `acl --db-path`, `data-server --root`), with `--home` kept as their
    /// fallback. A fully remote mount (`--data` + `--meta`) needs neither and
    /// writes nothing here. Defaults to $GROVEFS_HOME or ~/.grovefs.
    #[arg(long, global = true)]
    home: Option<PathBuf>,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Initialize a view (creates meta DB, optional seed manifest)
    Init {
        #[arg(long)]
        view: String,
        /// Optional manifest file: lines of "mkdir <path>" or "touch <path>"
        /// to pre-build a custom folder hierarchy.
        #[arg(long)]
        manifest: Option<PathBuf>,
    },
    /// Mount a view at a mountpoint (runs in foreground)
    Mount {
        #[arg(long)]
        view: String,
        #[arg(short, long)]
        mountpoint: PathBuf,
        /// Remote data service address (host:port). If set, object IO is shipped
        /// over the network to a `data-server` instead of the local disk (no NFS).
        /// A comma-separated list (addr1,addr2,...) enables deterministic
        /// cross-server striping; a single address behaves exactly as before.
        #[arg(long)]
        data: Option<String>,
        /// Remote metadata service address (host:port). If set, the view tree is
        /// served by a central `meta-server` instead of a local `<home>/meta` DB,
        /// so multiple clients mounting the same view share ONE authoritative view
        /// (consistent across machines) rather than diverging local copies.
        /// May be a comma-separated failover list (`primary,standby`): the mount
        /// stays on the first reachable node and falls over to a backup when the
        /// primary dies (pairs with HA `meta-server --role/--peer`).
        #[arg(long)]
        meta: Option<String>,
        /// Mount the view read-only: every mutating operation (create,
        /// write, mkdir, unlink, rename, symlink/link, chmod, truncate) returns
        /// EROFS. Enforced by the kernel `MS_RDONLY` flag plus a filesystem-side
        /// guard.
        #[arg(long)]
        ro: bool,
        /// Strict cache coherence for multi-client mounts: disable the kernel
        /// attribute/dentry cache and read file data straight through
        /// (FOPEN_DIRECT_IO), so one client's writes are visible to another
        /// immediately instead of after the cache TTL. Trade: no client-side
        /// caching, so more metadata/data round-trips. Pairs with `--meta`.
        #[arg(long)]
        strict: bool,
        /// Access token presented to a `meta-server` started with `--token`.
        /// Must match the server's token or the mount fails at connect time with
        /// a permission error. Ignored for local metadata (no `--meta`).
        #[arg(long)]
        token: Option<String>,
        /// Extra fuser options, e.g. allow_root, allow_other
        #[arg(long, num_args = 0..)]
        o: Vec<String>,
        /// FUSE worker threads dispatching requests concurrently (fuser 0.18
        /// session `n_threads`). 0 = single-threaded (legacy behavior, useful as
        /// an A/B knob for the bench). Each thread gets its own /dev/fuse fd via
        /// `FUSE_DEV_IOC_CLONE` (Linux 4.5+), so kernel replies are not serialized
        /// behind one channel lock.
        #[arg(long, default_value_t = 4)]
        threads: usize,
        /// Report a specific uid on the `M_OPEN` handshake (remote meta only),
        /// instead of the process's real euid, so a shared `meta-server` with a
        /// view whitelist can admit this mount under a chosen slot. Also drives
        /// the local mount's read-gate check. Advisory: the client self-reports
        /// its uid, and the token is the real gate against a malicious peer.
        #[arg(long)]
        as_uid: Option<u32>,
        /// Presentation overlay: a rules file that reshapes how THIS mount
        /// displays the view without touching the stored tree. Verbs: `hide
        /// <src>`, `only <src>`, `map <src> <dst>`, `link <src> <dst>` (see
        /// `grovefs::layout`). Client-side and per-mount: two clients mounting
        /// the same view with different files see different cuts of the same
        /// authoritative data. Writes through the lens land on the real
        /// entries; creating directly inside a synthesized (lens-only) dir is
        /// refused with EPERM.
        #[arg(long)]
        layout: Option<PathBuf>,
        /// Disable FUSE READDIRPLUS negotiation (kernel >= 4.6). With it off,
        /// directory listings fall back to plain READDIR + per-entry LOOKUP —
        /// useful as an A/B benchmark knob and for compatibility debugging.
        /// READDIRPLUS is only ever offered on mounts whose metadata provider
        /// pages inside its own store: local `--home` SQLite, and remote mounts
        /// against a SQLite/PG meta-server (which serve the paged M_READDIRP
        /// RPC). Derived/lens mounts answer a whole listing per buffer, so this
        /// flag changes nothing for them.
        #[arg(long)]
        no_readdirplus: bool,
        /// Raise the kernel attribute/dentry cache TTL to this many seconds
        /// (default 2). For read-heavy or immutable trees (e.g. streaming the
        /// same large files, or an astronomy "data never lands" read-once
        /// workload) a larger TTL cuts repeated LOOKUP/GETATTR round-trips.
        /// Cross-client freshness is still kept by the default notify-invalidation
        /// push, and on kernels >= 3.15 AUTO_INVAL_DATA drops stale page data when
        /// size/mtime change; `--strict` overrides this to 0 (full bypass).
        #[arg(long, default_value_t = 2)]
        tune_cache: u64,
        /// Opt-in per-stage latency telemetry. When set, the mount records how
        /// long each phase of a read/write/metadata op takes (key resolve, data
        /// backend, remote RTT, striping fan-out, SQLite/PG query, SSI retry …)
        /// into HDR histograms and prints an aggregate p50/p90/p99/max report on
        /// unmount. Off by default and cheap when off (one relaxed load per site,
        /// no clock/lock/allocation on the hot path). Diagnostic only — not a
        /// throughput claim. Also enabled by env `GROVEFS_STATS=1`.
        #[arg(long)]
        stats: bool,
        /// With `--stats`: also append the report to this file periodically and
        /// on exit (a stable, diff-friendly sink for A/B runs and CI capture).
        #[arg(long)]
        stats_file: Option<PathBuf>,
        /// With `--stats`: rewrite `--stats-file` (or stderr if no file) every
        /// N seconds while mounted, so a long-running mount can be sampled
        /// offline without unmounting. 0/unset disables periodic dumping.
        #[arg(long, default_value_t = 0)]
        stats_interval: u64,
    },
    /// Run the remote data service (central cluster node) backed by local disk
    DataServer {
        /// Listen address, e.g. 0.0.0.0:50052
        #[arg(short, long, default_value = "127.0.0.1:50052")]
        listen: String,
        /// Directory holding the physical objects on this node
        #[arg(short, long)]
        root: PathBuf,
        /// Shared secret clients must present (`mount --token`). When set, the
        /// server rejects any `OP_OPEN` whose token differs, so an unauthorized
        /// host on the LAN can no longer read or write arbitrary objects by
        /// key. Empty (default) keeps the original LAN-trusted, no-auth behavior.
        #[arg(long)]
        token: Option<String>,
    },
    /// Run the central metadata service: the authoritative per-view DB that
    /// many `mount --meta` clients share (no NFS; pairs with `data-server`).
    MetaServer {
        /// Listen address, e.g. 0.0.0.0:50053
        #[arg(short, long, default_value = "127.0.0.1:50053")]
        listen: String,
        /// Authoritative metadata root for this server: under SQLite every
        /// view DB is created under `<db-path>/meta/<view>.db` (lazily, on
        /// first open) and the ACL lives at `<db-path>/meta/acl.db`; under
        /// PostgreSQL (`--store postgres://…`) both the view trees and the
        /// ACL whitelist live in PG, making `<db-path>` a config-only
        /// directory (derived-view definitions, logs). Takes precedence over
        /// the global `--home`, which is kept for backward compatibility.
        #[arg(long)]
        db_path: Option<PathBuf>,
        /// Shared secret clients must present (`mount --token`). When set, the
        /// server refuses any `M_OPEN` whose token differs, so an unauthorized
        /// host on the LAN can no longer open arbitrary view trees. Empty
        /// (default) keeps the original LAN-trusted, no-auth behavior.
        #[arg(long)]
        token: Option<String>,
        /// HA role: `primary` (default) serves reads+writes and ships snapshots to
        /// `--peer`; `standby` serves reads only from the replicated copy, rejects
        /// writes, and promotes itself if the primary's heartbeats stop.
        #[arg(long, default_value = "primary")]
        role: String,
        /// The other HA node's `host:port`. A primary pushes every ~500ms and
        /// heartbeats its epoch here; a standby promotes after missing heartbeats.
        /// Requires a *separate* metadata root (`--db-path`, or the legacy
        /// global `--home`) from the peer (each node keeps its own
        /// copy of the view dbs; replication ships whole snapshots between them).
        #[arg(long)]
        peer: Option<String>,
        /// Opt-in synchronous replication for mutating meta ops (L2). Only
        /// meaningful with `--role primary` and a `--peer`. When set, every
        /// committed write op blocks until the standby has durably installed
        /// the pushed snapshot and ACKed on the control channel, dropping RPO
        /// from ~500ms to 0 for that op. Cost: ~one control-channel RTT +
        /// `VACUUM INTO` fsync per write, serialized across concurrent writers.
        /// On ACK timeout (2s) the op still succeeds but degrades to async with
        /// an `eprintln!` warn — so RPO is never silently misreported.
        #[arg(long)]
        sync_repl: bool,
        /// Fencing witness: one or more `data-server` `host:port` arbiters (a
        /// comma-separated list) both HA nodes can reach, used to fence the
        /// double-write window by MAJORITY. A primary holds a time-bounded LEASE
        /// on its epoch at each arbiter (renewed every tick); a grant is only
        /// authoritative once a strict majority (`N/2+1`) of them hold it, and an
        /// arbiter refuses overlapping leases, so a majority lease is itself proof
        /// of exclusive authority — a write is admitted from a cheap LOCAL lease
        /// check, not an arbiter round-trip on every op. Because two majorities
        /// always intersect, a same-value promotion race is won by exactly one
        /// node. Losing a MINORITY of arbiters neither freezes writes (a quorum is
        /// still reachable) nor lets a stale primary resurrect (a quorum still
        /// fences it); `N=1` behaves exactly as a single witness used to. The gate
        /// also covers the DATA plane: the client stamps its live epoch onto every
        /// write-intent content frame (create/write/truncate/remove/fsync), and
        /// the witness data-server refuses one whose epoch lags its token (EROFS),
        /// so stale bytes can no longer slip in by bypassing the meta layer. That
        /// content gate is only effective on a data-server the quorum raises the
        /// token of, i.e. one in the `--witness` list — deploy the witness
        /// co-located with the storage clients write to; a content store outside
        /// the set keeps a zero token and is guarded by the meta gate alone.
        /// Because the data-server holds the fence check and the backend write
        /// atomically against a promotion (a claim drains in-flight writes), a
        /// stale write can never land concurrently with the new primary's — content
        /// torn-writes are closed on the arbiter, not just bounded. Promotion is
        /// likewise non-overlapping: a would-be new primary may only seize a
        /// majority lease once the incumbent's has expired, so a stale primary
        /// provably stands down before another serves. A meta<->meta partition thus
        /// converges to one primary instead of an unbounded double-primary. Cost:
        /// one arbiter fan-out per lease renewal (amortized over all writes in the
        /// window), only when `--witness` is set. Used FAIL-SAFE: if a configured
        /// witness cannot form a lease quorum for 3s (one lease) a primary
        /// self-demotes and a standby refuses to promote (writes freeze rather than
        /// risk split-brain). Each data-server persists the token in
        /// `<root>/.fence_token`, so losing a minority of those stores cannot
        /// resurrect a deposed primary. Empty (default) keeps peer-only
        /// best-effort fencing, which a strict two-node split-brain can defeat —
        /// deploy a witness (an ODD number, 3 is the robust default) for safety.
        #[arg(long)]
        witness: Option<String>,
        /// Metadata backend URI. Default (unset or `"sqlite"`) is the embedded
        /// SQLite path: one db file per view under `<db-path>/meta/`, HA snapshot
        /// shipping available. Set to `postgres://user:pw@host:port/db`
        /// (`postgresql://…` also accepted) to place the authoritative view
        /// tree in PostgreSQL: one `grovefs_v_<hex(view)>` schema per view,
        /// transactions for rename/unlink/lock. Note that this mode is
        /// **incompatible** with `--role standby` / `--peer` / `--witness`:
        /// GroveFS-native HA snapshot shipping is `VACUUM INTO`-based and only
        /// makes sense on the embedded SQLite backend; under PG, run
        /// PostgreSQL's own streaming/logical replication for HA instead.
        /// The per-view uid whitelist also migrates to a shared PG table
        /// (`grovefs_acl.view_acl`), so one `pg_dump` covers everything.
        #[arg(long)]
        store: Option<String>,
    },
    /// Unmount a mountpoint
    Umount {
        #[arg(short, long)]
        mountpoint: PathBuf,
    },
    /// Manage the per-view uid whitelist. Under SQLite the store lives at
    /// `<db-path>/meta/acl.db`; under PostgreSQL (`--store postgres://…`)
    /// it lives in the shared table `grovefs_acl.view_acl`.
    /// A view with no rows is unrestricted (any uid, once past the token gate,
    /// may mount); adding any row for a view switches it to whitelist mode.
    Acl {
        /// The metadata root holding `meta/acl.db` — deliberately the SAME
        /// option name (and the same directory) as `meta-server --db-path`,
        /// so that both commands take one shared path, spelled identically.
        /// Falls back to the global `--home` when omitted.
        #[arg(long, global = true)]
        db_path: Option<PathBuf>,
        /// Connect to a PostgreSQL-backed ACL store instead of local SQLite.
        /// Must match the `meta-server --store` URI.
        #[arg(long)]
        store: Option<String>,
        #[command(subcommand)]
        action: AclAction,
    },
    /// Manage SERVER-SIDE view definitions — the two governance shapes:
    /// DERIVED views (named presentation lenses over ONE physical view tree,
    /// mounted read-only) and pure ALIASES (public-name ↔ real-name mapping,
    /// writable, real name not mountable). Definitions live in the meta root
    /// (`<db-path>/meta/views/<name>.layout`), clients simply mount
    /// `--view <name>` and need no local rules file. A `meta-server` resolves
    /// them at `M_OPEN`; a local mount resolves them from `--home`. Rules are
    /// display customization, NOT a security boundary — hard isolation stays
    /// view + ACL + token (mounting the base name directly shows the full
    /// tree to whoever is allowed past its gates). Derived (virtual) views
    /// mount READ-ONLY: the shape is centrally governed, so every write goes
    /// to the original (base) view — a mount of a derived name refuses all
    /// mutations with EROFS (kernel read-only flag negotiated at `M_OPEN`,
    /// plus an authoritative per-op gate on the meta-server).
    View {
        /// The metadata root holding `meta/views/*.layout` — same option name
        /// and directory as `meta-server --db-path` / `grove acl --db-path`;
        /// falls back to the global `--home` when omitted.
        #[arg(long)]
        db_path: Option<PathBuf>,
        #[command(subcommand)]
        action: ViewAction,
    },
    /// Serve view governance over MCP (Model Context Protocol) on stdio, for
    /// AI agents. An MCP client (agent runtime) spawns this as a subprocess
    /// and the agent drives the same validated operations as `grove view`:
    /// list_views / define_derived_view / alias_view / remove_view /
    /// preview_rules — rules travel as inline text (hide/only/map/link), so
    /// an agent customizes a data listing's shape without touching files.
    /// Implements the initialize / ping / tools/list / tools/call core of the
    /// 2025-06-18 protocol revision with newline-delimited JSON-RPC 2.0
    /// framing. Control plane only: it writes `<db-path>/meta/views/*.layout`
    /// definitions (live meta-server mounts hot-reload them) but never mounts
    /// or touches data. `--db-path` sets the default metadata root (falls
    /// back to `--home`); a per-call `db_path` argument overrides it.
    Mcp {
        /// Metadata root holding `meta/views/` — same option name and
        /// directory as `grove view` / `meta-server --db-path`.
        #[arg(long)]
        db_path: Option<PathBuf>,
        /// Require an API key before the tool surface is usable. With it, an
        /// MCP client must present a `grove apikey create` key as `apiKey` in
        /// the `initialize` params (or the operator exports
        /// `GROVEFS_MCP_API_KEY` to pre-authenticate the whole server); until a
        /// key authenticates, `tools/list` and `tools/call` are refused with a
        /// JSON-RPC error. `initialize`/`ping` stay open so a client can always
        /// handshake. Off by default: a plain `grove mcp` trusts whoever can
        /// spawn it (the same OS boundary as a local mount). Either way, every
        /// tool call is appended to `<db-path>/meta/api_keys.db`'s audit log.
        #[arg(long)]
        require_api_key: bool,
    },
    /// Manage the MCP server's API keys and read its call-audit log. Keys
    /// authenticate callers of `grove mcp --require-api-key`; they are stored
    /// as SHA-256 hashes (only the minting `create` shows the plaintext once),
    /// so a leaked `api_keys.db` hands out nothing usable. Revoking keeps the
    /// row (so past calls stay attributed) but fails authentication. The audit
    /// log records every tool call (timestamp, key, tool, success, detail).
    Apikey {
        /// The metadata root holding `meta/api_keys.db` — same option name and
        /// directory as `grove mcp --db-path` / `meta-server --db-path`; falls
        /// back to the global `--home`.
        #[arg(long)]
        db_path: Option<PathBuf>,
        #[command(subcommand)]
        action: ApikeyAction,
    },
    /// Import an EXISTING local directory tree into a view WITHOUT moving or
    /// duplicating bytes: every regular file is registered as an object —
    /// hard-linked into `blocks/` when source and store share a filesystem
    /// (zero extra bytes; edits through either path stay visible in the
    /// other), streamed as a copy otherwise — and the directory structure is
    /// mirrored into the view tree under `--prefix`. Run it where the store
    /// lives (local `--home`, or the meta/data servers' root — same
    /// operational envelope as `grove acl`/`grove view`). Re-running is an
    /// INCREMENTAL import: names already in the view are skipped. Symlinks
    /// and special files are skipped (counted, never followed). Timestamps,
    /// mode and owner are preserved from the source. The target must be a
    /// PHYSICAL view — importing into a derived (virtual) name is refused,
    /// since virtual views mount read-only. Live `meta-server`
    /// mounts converge within their attr/dentry TTL (≤2s, no remount); a PG
    /// backend instead goes through a mounted path (rsync) since the
    /// authoritative tree lives in Postgres, not in a local db file.
    Import {
        /// Metadata root holding `meta/<view>.db` — same option name and
        /// directory as `meta-server --db-path`; falls back to `--home`.
        #[arg(long)]
        db_path: Option<PathBuf>,
        /// Data root holding `blocks/` — same option name as
        /// `data-server --root` style paths; defaults to `--db-path` (the
        /// colocated topology used by every deployment example).
        #[arg(long)]
        data_root: Option<PathBuf>,
        /// Target view (its meta DB is created if absent, like `init`).
        #[arg(long)]
        view: String,
        /// Existing directory to mirror in.
        #[arg(long)]
        source: PathBuf,
        /// In-view destination directory; defaults to the source dir's name
        /// at the view root.
        #[arg(long)]
        prefix: Option<String>,
        /// Only report what would be imported; touch nothing.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum ViewAction {
    /// Create/replace derived view `--name` on physical `--base`, rules read
    /// from `--rules FILE` (same verbs as `mount --layout`: hide/only/map/link).
    /// Against a `meta-server`, live mounts of that name pick the new shape up
    /// automatically (the server re-checks the definition's mtime per op; the
    /// client converges within its attr/dentry TTL, ≤2s — no remount). A local
    /// (`--home`) mount resolves the definition once at mount time. Mounts of
    /// the derived name are READ-ONLY (writes belong to the base view).
    Define {
        #[arg(long)]
        name: String,
        #[arg(long)]
        base: String,
        /// Layout rules file (one verb per line; a `base` line inside it is
        /// rejected here — the base is given by `--base`).
        #[arg(long)]
        rules: PathBuf,
    },
    /// List every view definition: derived views (name, base tree, rule
    /// counts, read-only) and pure aliases (name → real view, writable).
    List,
    /// Remove a view DEFINITION `--name` (derived or alias — the file slot is
    /// shared). Underlying data untouched;
    /// already-mounted clients keep their resolved lens until they remount
    /// (or the server's cache is evicted by an HA/snapshot event). Removing an
    /// alias also makes the real name mountable again for NEW mounts.
    Rm {
        #[arg(long)]
        name: String,
    },
    /// Create/replace a PURE ALIAS `--name` for physical `--real`: name
    /// governance (vs `define`'s shape governance). No rules, so the alias
    /// shows the FULL tree and stays WRITABLE — it simply is the real view
    /// under a public name. A `meta-server` then refuses mounts of the real
    /// name itself (and a local `--home` mount honors the same contract):
    /// while the alias exists the real name is not a mount entry point, so
    /// public names can rotate freely without exposing the storage identity.
    /// ACL grants are checked against the REAL name (resolution precedes the
    /// read gate), so `grove acl allow --view <real> ...` authorizes both
    /// doors. Chained aliases, aliases over derived views, and aliasing an
    /// already-governed real name are rejected at this command.
    ///
    /// To ROTATE a leaked public name in one gapless step, add `--retire
    /// <old-alias>`: the new alias is written before the old one is removed,
    /// so the real name stays governed throughout (never briefly mountable by
    /// its own name), and the ACL grants keyed to the real view carry over
    /// untouched — the leaker's old name stops resolving, legitimate users
    /// just remount under the new name.
    Alias {
        #[arg(long)]
        name: String,
        #[arg(long)]
        real: String,
        /// Atomically retire a previous alias of the same real view (name
        /// rotation without a governance gap).
        #[arg(long)]
        retire: Option<String>,
    },
}

#[derive(Subcommand)]
enum AclAction {
    /// Grant `--uid` access to `--view`. At least one of `--read`/`--write`
    /// is normally set; both-false reserves the row without granting.
    Allow {
        #[arg(long)]
        view: String,
        #[arg(long)]
        uid: u32,
        #[arg(long)]
        read: bool,
        #[arg(long)]
        write: bool,
    },
    /// Revoke `--uid`'s grant on `--view`. Removing the last row reopens the
    /// view to everyone.
    Revoke {
        #[arg(long)]
        view: String,
        #[arg(long)]
        uid: u32,
    },
    /// List grants on `--view` (empty output = unrestricted).
    List {
        #[arg(long)]
        view: String,
    },
}

#[derive(Subcommand)]
enum ApikeyAction {
    /// Mint a new key labeled `--label` and print the full plaintext
    /// `<id>:<secret>` ONCE (it is not recoverable afterward — only its
    /// SHA-256 is stored). Give this to the agent/client; it presents it as
    /// `apiKey` in the MCP `initialize` params.
    Create {
        #[arg(long)]
        label: String,
    },
    /// List every key (id, label, created/last-used/revoked) — never any
    /// secret material.
    List,
    /// Revoke the key with id `--id` (short id prefix accepted). Revoked keys
    /// fail authentication but stay listed so past audit rows keep attribution.
    Revoke {
        #[arg(long)]
        id: String,
    },
    /// Show the most recent `--limit` (default 20) MCP call-audit rows.
    Calls {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
}

fn base_dir(home: &Option<PathBuf>) -> PathBuf {
    if let Some(h) = home {
        return h.clone();
    }
    if let Ok(h) = std::env::var("GROVEFS_HOME") {
        return PathBuf::from(h);
    }
    dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".grovefs")
}

fn meta_path(base: &Path, view: &str) -> PathBuf {
    base.join("meta").join(format!("{view}.db"))
}

/// Scan `<base>/meta/views/*.layout` once for PURE ALIAS definitions
/// (alias-name → real-view-name). Mirrors the meta-server's own resolver so a
/// local `--home` mount honors the same name-governance contract: mounting a
/// governed real name by its real name is refused locally, exactly as
/// remotely. Malformed entries are ignored here; the branches that actually
/// need a definition re-parse it and surface the error.
fn load_view_aliases(base: &Path) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if let Ok(rd) = std::fs::read_dir(base.join("meta").join("views")) {
        for ent in rd.flatten() {
            let p = ent.path();
            if p.extension().and_then(|s| s.to_str()) != Some("layout") {
                continue;
            }
            let name = match p.file_stem().and_then(|s| s.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            if let Ok(grovefs::layout::ViewDef::Alias(real)) =
                grovefs::layout::LayoutRules::parse_def_file(&p)
            {
                out.insert(name, real);
            }
        }
    }
    out
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let cli = Cli::parse();
    let base = base_dir(&cli.home);
    // NOTE: deliberately no unconditional `create_dir_all(base/meta)` here.
    // Only two paths actually touch `<home>/meta`: `init` and a *local* mount
    // (no `--meta`). Those create it in their own branches below. A fully
    // remote mount (`--data` + `--meta`) writes nothing under `--home` — no
    // authoritative data, no cache, no ACL (the read/write gates are enforced
    // server-side at M_OPEN / per-op) — so `--home` is not needed at all there
    // and leaves no footprint. The servers self-provision too: `meta-server`
    // creates `<base>/meta` in `serve()`, and `AclStore`/`MetaStore`/the
    // on-push view path all `create_dir_all` before opening their files.

    match cli.command {
        Cmd::Init { view, manifest } => {
            let mp = meta_path(&base, &view);
            if let Some(dir) = mp.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let meta = MetaEngine::open(&mp)?;
            println!("initialized view '{view}' at {}", mp.display());
            if let Some(mf) = manifest {
                let backend = backend::open_local(&base)?;
                seed_manifest(&meta, backend.as_ref(), &mf)?;
                println!("applied seed manifest {}", mf.display());
            }
        }
        Cmd::Mount {
            view,
            mountpoint,
            data,
            meta,
            ro,
            strict,
            token,
            o,
            threads,
            as_uid,
            layout,
            no_readdirplus,
            tune_cache,
            stats: stats_on,
            stats_file,
            stats_interval,
        } => {
            std::fs::create_dir_all(&mountpoint)?;
            // ---- opt-in per-stage latency telemetry ----
            // Enabled by `--stats` OR env `GROVEFS_STATS=1|true`. Off by default,
            // where every instrument site is a single relaxed load that skips all
            // work (no clock, lock, or allocation on the hot path). The returned
            // `ExitDump` guard prints + flushes the aggregate report when the mount
            // arm returns — including when `session.run()?` bails — so even a
            // failed mount leaves a triage artifact.
            let stats_env = std::env::var("GROVEFS_STATS")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false);
            let _stats_dump = if stats_on || stats_env {
                let interval = if stats_interval > 0 {
                    Some(std::time::Duration::from_secs(stats_interval))
                } else {
                    None
                };
                grovefs::stats::enable(stats_file.clone(), interval);
                if let Some(p) = &stats_file {
                    println!("telemetry: --stats on, periodic report -> {}", p.display());
                } else {
                    println!("telemetry: --stats on, report printed at unmount");
                }
                Some(grovefs::stats::ExitDump)
            } else {
                None
            };
            // Effective uid reported to the server / used for local ACL check:
            // `--as-uid` overrides the process euid (advisory).
            let uid = as_uid.unwrap_or_else(|| unsafe { libc::geteuid() });
            // A DERIVED (virtual) view — server-defined shape or local
            // `meta/views/<view>.layout` — mounts read-only by contract: the
            // shape is centrally governed, writes belong to the original
            // (base) view. Set by whichever metadata source resolves below;
            // folded into the FUSE readonly flag AND the kernel RO mount.
            let mut view_ro = false;
            // Metadata source: a central `meta-server` when --meta is given (a
            // single authoritative view shared across clients), else this machine's
            // local `<home>/meta/<view>.db`. Both satisfy `MetaProvider`, so the
            // FUSE layer below is identical either way.
            let meta_provider: Arc<dyn MetaProvider> = match &meta {
                Some(addr) => {
                    println!("using remote metadata service at {addr}");
                    let rm = remote::RemoteMeta::new_with_token_uid(
                        addr,
                        &view,
                        token.as_deref().unwrap_or(""),
                        uid,
                    )?;
                    // The M_OPEN ack flagged derived names (absent = old server
                    // or physical view → stays writable, behavior-compatible).
                    view_ro = rm.is_readonly_view();
                    Arc::new(rm)
                }
                None => {
                    // Local mount: enforce the view whitelist's read-gate before
                    // opening the sqlite file. Write-gating is a meta-server-only
                    // feature (writes here hit the file directly, behind OS fs
                    // perms); document this honestly.
                    std::fs::create_dir_all(base.join("meta"))?;
                    // PURE ALIAS resolution mirrors the meta-server's M_OPEN
                    // gate (see `load_view_aliases`): a governed real name is
                    // not a local mount entry point either, and the public
                    // alias name resolves to the real tree — full shape,
                    // writable, grants written against the REAL name hold.
                    // Remote mounts deliberately do NOT swap here: the client
                    // sends the alias name and the SERVER resolves (and
                    // refuses) it.
                    let mut effective = view.clone();
                    let mut local_def: Option<grovefs::layout::ViewDef> = None;
                    let local_aliases = load_view_aliases(&base);
                    if let Some(real) = local_aliases.get(&view) {
                        println!(
                            "alias view '{view}' resolves to real view '{real}' (full tree, writable)"
                        );
                        effective = real.clone();
                    } else if local_aliases.values().any(|r| r == &view) {
                        // Mounting the real name while it is governed: the
                        // alias is the entry point, not this one (same shape
                        // as the server's M_OPEN refusal).
                        return Err(format!(
                            "view '{view}' is governed by an alias — mount the alias name instead (`grove view list` shows it)"
                        )
                        .into());
                    }
                    if layout.is_none() {
                        if let Ok(rd) =
                            std::fs::read_dir(base.join("meta").join("views"))
                        {
                            for ent in rd.flatten() {
                                let p = ent.path();
                                if p.file_stem().and_then(|s| s.to_str())
                                    == Some(view.as_str())
                                    && p.extension().and_then(|s| s.to_str())
                                        == Some("layout")
                                {
                                    let d =
                                        grovefs::layout::LayoutRules::parse_def_file(
                                            &p,
                                        )
                                        .map_err(|e| {
                                            format!("bad view definition '{view}': {e}")
                                        })?;
                                    local_def = Some(d);
                                    break;
                                }
                            }
                        }
                    }
                    let acl = AclStore::open(&base)?;
                    if !acl.check_read(&effective, uid) {
                        return Err(format!(
                            "local view '{effective}' is whitelisted; uid {uid} has no read grant (see `grove acl list --view {effective}`)"
                        )
                        .into());
                    }
                    // A local mount resolves the SAME derived-view definition a
                    // meta-server would (`<home>/meta/views/<view>.layout`): the
                    // lens wraps the base tree's engine, so one server-side
                    // definition serves every mount of this home. An explicit
                    // `--layout FILE` takes precedence (no lens stacking).
                    let def = base.join("meta").join("views").join(format!("{effective}.layout"));
                    let derived = match &local_def {
                        Some(grovefs::layout::ViewDef::Derived(b, r))
                            if layout.is_none() =>
                        {
                            Some((b.clone(), r.clone()))
                        }
                        _ if def.is_file() && layout.is_none() => {
                            let (b, r) =
                                grovefs::layout::LayoutRules::parse_view_file(&def)
                                    .map_err(|e| format!("bad derived view '{effective}': {e}"))?;
                            Some((b, r))
                        }
                        _ => None,
                    };
                    if let Some((b, rules)) = derived {
                        println!(
                            "derived view '{view}' over base '{b}': {} hide, {} only, {} reloc rule(s)",
                            rules.hide.len(),
                            rules.only.len(),
                            rules.reloc.len()
                        );
                        // Same read-only contract the meta-server enforces for
                        // derived names; a local mount honors it without a wire.
                        view_ro = true;
                        println!(
                            "note: derived (virtual) views mount read-only — write to base view '{b}'"
                        );
                        let engine = MetaEngine::open(&meta_path(&base, &b))?;
                        Arc::new(grovefs::layout::LayoutMeta::new(Arc::new(engine), rules))
                    } else {
                        let mp = meta_path(&base, &effective);
                        Arc::new(MetaEngine::open(&mp)?)
                    }
                }
            };
            // Presentation overlay: wrap the provider in a per-mount lens so
            // different clients render the SAME stored tree with different
            // cuts/reorganization. Purely client-side: the server and every
            // other mount are unaffected; data IO is untouched (entries keep
            // their real inodes, so read/write/lock/GC pass straight through).
            let meta_provider: Arc<dyn MetaProvider> = match &layout {
                Some(f) => {
                    let rules = grovefs::layout::LayoutRules::parse_file(f)
                        .map_err(|e| format!("bad layout file: {e}"))?;
                    println!(
                        "layout overlay {}: {} hide, {} only, {} reloc rule(s)",
                        f.display(),
                        rules.hide.len(),
                        rules.only.len(),
                        rules.reloc.len()
                    );
                    Arc::new(grovefs::layout::LayoutMeta::new(meta_provider, rules))
                }
                None => meta_provider,
            };
            // Fencing wiring: the meta client owns the live epoch handle (kept
            // current on every M_OPEN handshake/failover); the data client shares
            // the SAME handle to stamp writes, and uses the refresher to re-pull
            // the epoch from the authority when the data-server fences a stale one.
            // For a local meta this is a detached 0, so the data gate stays inert.
            let fence_epoch = meta_provider.fence_handle();
            let meta_for_fence = meta_provider.clone();
            // Keep a display-capable handle for the invalidation listener:
            // for `--layout` + `--meta`, the lens translates source-coordinate
            // events into display coordinates at the kernel notifier.
            let provider_for_events: Option<Arc<dyn MetaProvider>> =
                if meta.is_some() { Some(meta_provider.clone()) } else { None };
            let fence_refresh: Arc<dyn Fn() -> u64 + Send + Sync> =
                Arc::new(move || meta_for_fence.fetch_fence_epoch());
            // Pick the data path: remote over our TCP transport (no NFS) when
            // --data is given, otherwise the local disk backend. The FUSE and
            // metadata layers are identical either way thanks to the trait.
            let backend: Arc<dyn backend::StorageBackend> = match &data {
                Some(addr) => {
                    // A comma-separated list of data servers activates deterministic
                    // cross-server striping (P5); a single address keeps the plain
                    // remote path byte-for-byte unchanged.
                    let addrs: Vec<&str> =
                        addr.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
                    if addrs.len() > 1 {
                        println!(
                            "using sharded remote data service across {} servers: {}",
                            addrs.len(),
                            addrs.join(",")
                        );
                        let mut members: Vec<Arc<dyn backend::StorageBackend>> =
                            Vec::with_capacity(addrs.len());
                        for a in &addrs {
                            members.push(Arc::new(
                                remote::RemoteBackend::new_with_token(
                                    a,
                                    token.as_deref().unwrap_or(""),
                                )?
                                .with_fence(fence_epoch.clone(), fence_refresh.clone()),
                            ));
                        }
                        Arc::new(backend::ShardedBackend::with_default_stripe(members))
                    } else {
                        let one = addrs.first().copied().unwrap_or(addr.as_str());
                        println!("using remote data service at {one}");
                        Arc::new(
                            remote::RemoteBackend::new_with_token(
                                one,
                                token.as_deref().unwrap_or(""),
                            )?
                            .with_fence(fence_epoch.clone(), fence_refresh.clone()),
                        )
                    }
                }
                None => Arc::from(backend::open_local(&base)?),
            };
            let fs = GroveFS::new(
                meta_provider,
                backend,
                ro || view_ro || o.iter().any(|s| s == "ro"),
                strict || o.iter().any(|s| s == "strict"),
                !no_readdirplus,
                tune_cache,
            );

            // Build the fuser 0.18 session Config. The old `--o` string surface is
            // preserved; only `allow_root`/`allow_other` changed shape — they are no
            // longer MountOption variants but the `acl` field (session-wide request
            // filtering), with All == allow_other and RootAndOwner == allow_root.
            let mut cfg = fuser::Config::default();
            cfg.mount_options.push(fuser::MountOption::FSName(format!("grovefs:{view}")));
            for raw in &o {
                match raw.as_str() {
                    "allow_root" => cfg.acl = fuser::SessionACL::RootAndOwner,
                    "allow_other" => cfg.acl = fuser::SessionACL::All,
                    "default_permissions" => cfg
                        .mount_options
                        .push(fuser::MountOption::DefaultPermissions),
                    "auto_unmount" => cfg.mount_options.push(fuser::MountOption::AutoUnmount),
                    "ro" => cfg.mount_options.push(fuser::MountOption::RO),
                    other => cfg.mount_options.push(fuser::MountOption::CUSTOM(other.to_string())),
                }
            }
            // `--ro` (or `-o ro`) mounts the view read-only at the kernel level;
            // a derived (virtual) view is forced RO regardless of the flag.
            let want_ro = ro || view_ro || o.iter().any(|s| s == "ro");
            if want_ro && !o.iter().any(|s| s == "ro") {
                cfg.mount_options.push(fuser::MountOption::RO);
            }
            // Concurrent dispatch: n_threads worker event loops, each with its own
            // cloned /dev/fuse fd. `threads = 0` maps to single-threaded legacy mode.
            cfg.n_threads = if threads > 1 { Some(threads) } else { None };
            cfg.clone_fd = threads > 1;
            // Only print `base` when the mount actually reads/writes under it:
            // a fully remote mount (`--data` + `--meta`) touches no local store
            // at all, so naming a base there would mislead (mirrors the
            // removed unconditional create_dir_all side effect).
            let locality = if data.is_some() && meta.is_some() {
                String::from("fully remote: no local store")
            } else {
                format!("base {}", base.display())
            };
            println!(
                "mounting view '{view}' at {} ({}{})",
                mountpoint.display(),
                locality,
                if want_ro { ", read-only" } else { "" }
            );
            // Graceful shutdown: the FUSE session returns whenever the mount goes
            // away; a helper thread watches for SIGINT/SIGTERM and asks
            // `fusermount -u` to unmount, ending the loop cleanly. An external
            // `fusermount -u` (no signal) likewise makes the session return.
            //
            // A remote-metadata mount additionally wants real-time cross-client
            // coherence without `--strict`: pushing kernel-cache invalidations
            // needs a `Notifier`, which we can grab from the session (`notifier()`
            // takes `&self` and returns an owned handle, so it does not keep the
            // session borrowed). We then run the session on the main thread in the
            // *same* blocking `run()` path as a local mount; a background listener
            // thread translates peer mutations into inval_entry/inval_inode.
            //
            // NOTE: do NOT use `spawn()` + `BackgroundSession::umount_and_join()`
            // here — `umount_and_join(self)` drops the mount handle first (an
            // active unmount) and then joins, so it tears the mount down
            // immediately instead of serving. `run()` is the blocking serve loop
            // (and in 0.18 it fans out to `cfg.n_threads` workers internally).
            let session = fuser::Session::new(fs, &mountpoint, &cfg)?;
            if let Some(addr) = &meta {
                let notifier = session.notifier();
                let tok = token.clone().unwrap_or_default();
                let _listener = remote::spawn_invalidation_listener(addr, &view, &tok, uid, notifier, provider_for_events);
            }
            spawn_term_unmounter(mountpoint.clone());
            session.run()?;
            println!("unmounted view '{view}' at {}", mountpoint.display());
        }
        Cmd::DataServer { listen, root, token } => {
            std::fs::create_dir_all(&root)?;
            let backend = Arc::from(backend::open_local(&root)?);
            let srv = match token.as_deref() {
                Some(t) if !t.is_empty() => remote::DataServer::with_token(&listen, backend, t),
                _ => remote::DataServer::new(&listen, backend),
            };
            // Durable fencing token: a witness restart must not zero the epoch
            // and let a deposed primary sneak back in as a writer.
            let srv = srv.with_fence_store(root.join(".fence_token"));
            srv.serve()?;
        }
        Cmd::MetaServer {
            listen,
            db_path,
            token,
            role,
            peer,
            sync_repl,
            witness,
            store,
        } => {
            let role = if role.eq_ignore_ascii_case("standby") {
                grovefs::remote::meta::Role::Standby
            } else {
                grovefs::remote::meta::Role::Primary
            };
            // `--db-path` is the explicit, self-describing form of the metadata
            // root; the global `--home` still works (back-compat) but loses if
            // both are given.
            let meta_base = db_path.unwrap_or_else(|| base.clone());
            let mut srv = match token.as_deref() {
                Some(t) if !t.is_empty() =>
                    grovefs::remote::MetaServer::with_token(&listen, &meta_base, t),
                _ => grovefs::remote::MetaServer::new(&listen, &meta_base),
            };
            srv = srv.with_ha(role, peer);
            if sync_repl {
                srv = srv.with_sync_repl(true);
            }
            srv = srv.with_witness(witness);
            srv = srv.with_store(store);
            srv.serve()?;
        }
        Cmd::Umount { mountpoint } => {
            run_fusermount(&["-u", &mountpoint.to_string_lossy()])?;
            println!("unmounted {}", mountpoint.display());
        }
        Cmd::Acl { db_path, store, action } => {
            // `--db-path` (same name/same dir as `meta-server --db-path`) wins;
            // the global `--home` stays as the backward-compatible fallback.
            let acl_base = db_path.unwrap_or_else(|| base.clone());
            let acl = match store.as_deref() {
                Some(uri) if uri.starts_with("postgres://") || uri.starts_with("postgresql://") => {
                    AclStore::open_pg(uri).map_err(|e| Box::<dyn std::error::Error>::from(e))?
                }
                _ => AclStore::open(&acl_base)?,
            };
            match action {
                AclAction::Allow { view, uid, read, write } => {
                    acl.allow(&view, uid, read, write)?;
                    println!("acl: granted view='{view}' uid={uid} read={read} write={write}");
                }
                AclAction::Revoke { view, uid } => {
                    acl.revoke(&view, uid)?;
                    println!("acl: revoked view='{view}' uid={uid}");
                }
                AclAction::List { view } => {
                    let rows = acl.list(&view);
                    if rows.is_empty() {
                        println!(
                            "view '{view}': UNRESTRICTED (no ACL rows — anyone who passes the token gate may mount)"
                        );
                    } else {
                        println!("view '{view}' is whitelisted:");
                        for r in rows {
                            println!(
                                "  uid {} read={} write={}",
                                r.uid, r.can_read, r.can_write
                            );
                        }
                    }
                }
            }
        }
        Cmd::View { db_path, action } => {
            // Derived-view definitions live with the authoritative meta root,
            // exactly like acl.db: control-plane state, read by the meta-server
            // at M_OPEN (or by a local mount from --home) — never replicated
            // with the view trees themselves (copy the `views/` dir to a HA
            // standby like any other config).
            let vbase = db_path.unwrap_or_else(|| base.clone());
            match action {
                ViewAction::Define {
                    name,
                    base: src_base,
                    rules,
                } => {
                    // The view-governance ops live in `grovefs::mcp` — the CLI
                    // and the MCP server are two entrances onto ONE validated
                    // implementation. `--rules` stays a file here; MCP agents
                    // hand the same text inline.
                    let text = std::fs::read_to_string(&rules)
                        .map_err(|e| format!("cannot read rules file {}: {e}", rules.display()))?;
                    for l in grovefs::mcp::define_view(&vbase, &name, &src_base, &text)? {
                        match l {
                            grovefs::mcp::OutLine::Info(s) => println!("{s}"),
                            grovefs::mcp::OutLine::Note(s) => eprintln!("{s}"),
                        }
                    }
                }
                ViewAction::List => {
                    for l in grovefs::mcp::list_views_text(&vbase)? {
                        match l {
                            grovefs::mcp::OutLine::Info(s) => println!("{s}"),
                            grovefs::mcp::OutLine::Note(s) => eprintln!("{s}"),
                        }
                    }
                }
                ViewAction::Alias { name, real, retire } => {
                    // Same shared implementation as the MCP `alias_view` tool
                    // (validation, gapless --retire rotation, notes).
                    for l in grovefs::mcp::alias_view(&vbase, &name, &real, retire.as_deref())? {
                        match l {
                            grovefs::mcp::OutLine::Info(s) => println!("{s}"),
                            grovefs::mcp::OutLine::Note(s) => eprintln!("{s}"),
                        }
                    }
                }
                ViewAction::Rm { name } => {
                    for l in grovefs::mcp::remove_view(&vbase, &name)? {
                        match l {
                            grovefs::mcp::OutLine::Info(s) => println!("{s}"),
                            grovefs::mcp::OutLine::Note(s) => eprintln!("{s}"),
                        }
                    }
                }
            }
        }
        Cmd::Mcp {
            db_path,
            require_api_key,
        } => {
            // The agent entrance onto the same governance API: an MCP client
            // spawns this over stdio; EOF on stdin shuts it down. Control
            // plane only — no mount, no data IO. `--require-api-key` gates
            // tools/list + tools/call behind a `grove apikey` credential.
            grovefs::mcp::serve(
                db_path.unwrap_or_else(|| base.clone()),
                require_api_key,
            )?;
        }
        Cmd::Apikey { db_path, action } => {
            // Keys + audit log live beside acl.db in the authoritative meta
            // root — control-plane state, shared by `grove mcp` at this path.
            let ak_base = db_path.unwrap_or_else(|| base.clone());
            let keys = grovefs::apikeys::ApiKeyStore::open(&ak_base)?;
            match action {
                ApikeyAction::Create { label } => {
                    let (id, full) = keys.create(&label)?;
                    println!("apikey created — shown ONCE (only its SHA-256 is stored; save it now):");
                    println!("  id:    {id}");
                    println!("  label: {label}");
                    println!("  key:   {full}");
                    println!();
                    println!("Present it as the `apiKey` param in the MCP `initialize` handshake, or");
                    println!("export GROVEFS_MCP_API_KEY='{full}' when launching");
                    println!("`grove mcp --db-path {} --require-api-key`.", ak_base.display());
                    println!("Revoke later with: grove apikey --db-path {} revoke --id {id}", ak_base.display());
                }
                ApikeyAction::List => {
                    let rows = keys.list()?;
                    if rows.is_empty() {
                        println!(
                            "no api keys yet (under {}); mint one with `grove apikey create --label L`",
                            ak_base.display()
                        );
                    } else {
                        println!(
                            "{:<3} {:<34} {:<16} {:<25} {:<25} {}",
                            "#", "id", "label", "created", "last_used", "revoked"
                        );
                        for (i, r) in rows.iter().enumerate() {
                            println!(
                                "{:<3} {:<34} {:<16} {:<25} {:<25} {}",
                                i + 1,
                                r.id,
                                r.label,
                                r.created_at,
                                r.last_used_at.as_deref().unwrap_or("-"),
                                r.revoked_at.as_deref().unwrap_or("-"),
                            );
                        }
                    }
                }
                ApikeyAction::Revoke { id } => {
                    // Accept an exact id, else a UNIQUE prefix (ids are long hex).
                    let all = keys.list()?;
                    let resolved: Vec<_> = if let Some(e) = all.iter().find(|k| k.id == id) {
                        vec![e]
                    } else {
                        all.iter().filter(|k| k.id.starts_with(id.as_str())).collect()
                    };
                    match resolved.len() {
                        0 => {
                            return Err(format!(
                                "no api key with id '{id}' (see `grove apikey list`)"
                            )
                            .into())
                        }
                        1 => {
                            let k = resolved[0];
                            if k.revoked_at.is_some() {
                                println!("apikey: id='{}' (label '{}') is already revoked", k.id, k.label);
                            } else if keys.revoke(&k.id)? {
                                println!("apikey: revoked id='{}' (label '{}'); it can no longer authenticate, past calls keep attribution", k.id, k.label);
                            } else {
                                println!("apikey: id='{}' became revoked concurrently", k.id);
                            }
                        }
                        n => {
                            return Err(format!(
                                "ambiguous id prefix '{id}' matches {n} keys — use a longer prefix or the full id"
                            )
                            .into())
                        }
                    }
                }
                ApikeyAction::Calls { limit } => {
                    let rows = keys.recent_calls(limit)?;
                    if rows.is_empty() {
                        println!("no logged mcp calls yet (a `grove mcp` run appends one row per tools/call here)");
                    } else {
                        println!("last {} mcp call(s), newest first:", rows.len());
                        println!(
                            "{:<25} {:<34} {:<12} {:<22} {:<4} {}",
                            "ts", "key_id", "method", "tool", "ok", "detail"
                        );
                        for r in &rows {
                            println!(
                                "{:<25} {:<34} {:<12} {:<22} {:<4} {}",
                                r.ts,
                                r.key_id.as_deref().unwrap_or("-"),
                                r.method,
                                if r.tool.is_empty() { "-" } else { r.tool.as_str() },
                                if r.ok { "yes" } else { "no" },
                                r.detail,
                            );
                        }
                    }
                }
            }
        }
        Cmd::Import {
            db_path,
            data_root,
            view,
            source,
            prefix,
            dry_run,
        } => {
            let md = std::fs::metadata(&source)
                .map_err(|e| format!("import source {}: {e}", source.display()))?;
            if !md.is_dir() {
                return Err("import --source must be a directory".into());
            }
            let mbase = db_path.unwrap_or_else(|| base.clone());
            // Import writes the authoritative tree directly — that is only
            // sound for a PHYSICAL view. A derived name is served through its
            // base's db; writing a separate `<view>.db` under the virtual name
            // would create a shadow tree nothing ever reads.
            if mbase
                .join("meta")
                .join("views")
                .join(format!("{view}.layout"))
                .is_file()
            {
                // Resolve the slot so BOTH governance shapes are covered: a
                // derived name would write a shadow `<view>.db` nothing reads,
                // and an alias name is not a storage identity at all — the
                // server resolves it to the real view, whose db import would
                // bypass here lives under a different name.
                let slot = mbase.join("meta").join("views").join(format!("{view}.layout"));
                let target = grovefs::layout::LayoutRules::parse_def_file(&slot)
                    .map(|d| match d {
                        grovefs::layout::ViewDef::Alias(real) => format!(
                            "'{view}' is a pure alias of real view '{real}' — import into '{real}' directly (an alias is not a storage identity)"
                        ),
                        grovefs::layout::ViewDef::Derived(..) => format!(
                            "import target '{view}' is a derived (virtual) view — import into its physical base view instead (derived views mount read-only)"
                        ),
                    })
                    .unwrap_or_else(|_| {
                        format!(
                            "import target '{view}' has a view definition — import into its physical base view instead"
                        )
                    });
                return Err(target.into());
            }
            let droot = data_root.unwrap_or_else(|| mbase.clone());
            let prefix = prefix.unwrap_or_else(|| {
                source
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "imported".to_string())
            });
            let prefix = format!("/{}", prefix.trim_matches('/'));
            if dry_run {
                // Literally touch nothing: not even the view DB is opened.
                let mut st = ImportStats::default();
                count_walk(&source, &mut st)?;
                println!(
                    "dry run: view '{view}' under {} would mirror {} dir(s) and {} file(s) \
                     (symlinks/special files and names already in the view are skipped \
                     on the real run)",
                    prefix, st.dirs, st.files
                );
                return Ok(());
            }
            let mp = meta_path(&mbase, &view);
            if let Some(dir) = mp.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let meta = MetaEngine::open(&mp)?;
            let backend = backend::open_local(&droot)?;
            let mut dirs: std::collections::HashMap<String, i64> =
                std::collections::HashMap::new();
            dirs.insert("/".to_string(), meta::ROOT_INO);
            ensure_dir_soft(&meta, &prefix, &mut dirs)?;
            let mut st = ImportStats::default();
            import_walk(&meta, backend.as_ref(), &source, &prefix, &mut dirs, &mut st)?;
            println!(
                "imported into view '{view}' under {}: {} dir(s), {} object(s) \
                 ({} hardlinked zero-copy, {} copied), {} skipped, {} failed",
                prefix,
                st.dirs,
                st.linked + st.copied,
                st.linked,
                st.copied,
                st.skipped,
                st.failed
            );
            if st.failed > 0 {
                return Err(format!("{} entry(ies) failed to import", st.failed).into());
            }
        }
    }
    Ok(())
}

/// Tallies for `grove import`.
#[derive(Default)]
struct ImportStats {
    dirs: usize,
    files: usize,
    linked: usize,
    copied: usize,
    skipped: usize,
    failed: usize,
}

/// ensure_dir variant that REUSES pre-existing nodes instead of failing —
/// what makes a re-run of `grove import` an incremental no-op over the
/// already-mirrored parts of the tree.
fn ensure_dir_soft(
    meta: &MetaEngine,
    path: &str,
    dirs: &mut std::collections::HashMap<String, i64>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut cur_ino = meta::ROOT_INO;
    let mut cur_path = String::from("/");
    for comp in path.trim_matches('/').split('/').filter(|c| !c.is_empty()) {
        let next_path = if cur_path == "/" {
            format!("/{comp}")
        } else {
            format!("{cur_path}/{comp}")
        };
        if let Some(&ino) = dirs.get(&next_path) {
            cur_ino = ino;
            cur_path = next_path;
            continue;
        }
        let ino = match meta.lookup(cur_ino, comp) {
            Some(node) => node.id,
            None => meta.create_node(cur_ino, comp, true, None, 0o0755,
                unsafe { libc::getuid() }, unsafe { libc::getgid() })?,
        };
        dirs.insert(next_path.clone(), ino);
        cur_ino = ino;
        cur_path = next_path;
    }
    Ok(())
}

/// Pure source-side tally for `import --dry_run`: counts what WOULD be
/// mirrored, without opening the store at all — "touch nothing", literally.
fn count_walk(src: &Path, st: &mut ImportStats) -> Result<(), Box<dyn std::error::Error>> {
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let ty = e.file_type()?;
        if ty.is_dir() {
            st.dirs += 1;
            count_walk(&e.path(), st)?;
        } else if ty.is_file() {
            st.files += 1;
        } else {
            st.skipped += 1;
        }
    }
    Ok(())
}

/// Recursively mirror the existing tree at `src` into view path `in_view`
/// (the data half of `Cmd::Import`). Dirs become nodes; regular files become
/// objects via `StorageBackend::import_at` (hard link first, copy fallback),
/// keeping their timestamps/mode/owner. Names that already exist are skipped;
/// symlinks/special files are skipped too (and never followed, which also
/// rules out symlink loops). Per-entry failures are counted and reported, not
/// fatal — one unreadable file must not abort a bulk import.
fn import_walk(
    meta: &MetaEngine,
    backend: &dyn backend::StorageBackend,
    src: &Path,
    in_view: &str,
    dirs: &mut std::collections::HashMap<String, i64>,
    st: &mut ImportStats,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::MetadataExt;
    let mut entries: Vec<_> = std::fs::read_dir(src)?
        .filter_map(|e| match e {
            Ok(e) => Some(e),
            Err(err) => {
                eprintln!("import: cannot read an entry under {}: {err}", src.display());
                st.failed += 1;
                None
            }
        })
        .collect();
    entries.sort_by_key(|e| e.file_name()); // stable order across re-runs
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.is_empty() || name == "." || name == ".." {
            continue;
        }
        let child = if in_view == "/" {
            format!("/{name}")
        } else {
            format!("{in_view}/{name}")
        };
        let ty = e.file_type()?;
        if ty.is_dir() {
            st.dirs += 1;
            ensure_dir_soft(meta, &child, dirs)?;
            import_walk(meta, backend, &e.path(), &child, dirs, st)?;
        } else if ty.is_file() {
            let parent_ino = *dirs
                .get(in_view)
                .ok_or_else(|| format!("import: missing dir node for {in_view}"))?;
            if meta.lookup(parent_ino, &name).is_some() {
                st.skipped += 1; // incremental re-run: keep whatever is there
                continue;
            }
            let fm = match std::fs::metadata(&e.path()) {
                Ok(fm) => fm,
                Err(err) => {
                    eprintln!("import: {}: {err}", e.path().display());
                    st.failed += 1;
                    continue;
                }
            };
            let key = backend.new_key(&uuid::Uuid::new_v4().to_string());
            let imported = match backend.import_at(&key, &e.path()) {
                Ok(v) => v,
                Err(err) => {
                    eprintln!("import: {}: {err}", e.path().display());
                    st.failed += 1;
                    continue;
                }
            };
            match meta.create_node(
                parent_ino,
                &name,
                false,
                Some(&key),
                (fm.mode() & 0o7777) as u32,
                fm.uid(),
                fm.gid(),
            ) {
                Ok(ino) => {
                    // Keep the file's own clock, not "import happened now".
                    meta.set_times(ino, Some(fm.atime() as i64), Some(fm.mtime() as i64));
                    match imported {
                        backend::Imported::Hardlinked => st.linked += 1,
                        backend::Imported::Copied => st.copied += 1,
                    }
                }
                Err(meta::MetaError::Exists) => {
                    // Lost a race against a concurrent writer: drop the object
                    // we just materialized so nothing dangles, count as skipped.
                    let _ = backend.remove(&key);
                    st.skipped += 1;
                }
                Err(err) => {
                    let _ = backend.remove(&key);
                    eprintln!("import: {child}: {err}");
                    st.failed += 1;
                }
            }
        } else {
            // symlink / fifo / socket / device: not representable as a plain
            // object today — counted, never followed.
            st.skipped += 1;
        }
    }
    Ok(())
}

/// Seed a custom folder hierarchy from a simple manifest. Demonstrates that the
/// view tree is entirely view-defined and decoupled from physical layout.
fn seed_manifest(
    meta: &MetaEngine,
    backend: &dyn backend::StorageBackend,
    manifest: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let text = std::fs::read_to_string(manifest)?;
    let mut dirs: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    dirs.insert("/".to_string(), meta::ROOT_INO);
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut it = line.split_whitespace();
        match it.next() {
            Some("mkdir") => {
                let path = it.next().ok_or("mkdir needs a path")?;
                ensure_dir(meta, path, &mut dirs)?;
            }
            Some("touch") => {
                let path = it.next().ok_or("touch needs a path")?;
                let (parent, name) = split_parent(path);
                let parent_ino = *dirs
                    .get(&parent)
                    .ok_or_else(|| format!("unknown parent {parent}"))?;
                let key = uuid::Uuid::new_v4().to_string();
                backend.create(&key)?;
                meta.create_node(parent_ino, &name, false, Some(&key), 0o0644,
                    unsafe { libc::getuid() }, unsafe { libc::getgid() })?;
            }
            _ => return Err(format!("bad manifest line: {line}").into()),
        }
    }
    Ok(())
}

fn ensure_dir(
    meta: &MetaEngine,
    path: &str,
    dirs: &mut std::collections::HashMap<String, i64>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut cur_ino = meta::ROOT_INO;
    let mut cur_path = String::from("/");
    for comp in path.trim_matches('/').split('/').filter(|c| !c.is_empty()) {
        let next_path = if cur_path == "/" {
            format!("/{comp}")
        } else {
            format!("{cur_path}/{comp}")
        };
        if let Some(&ino) = dirs.get(&next_path) {
            cur_ino = ino;
            cur_path = next_path;
            continue;
        }
        let ino = meta.create_node(cur_ino, comp, true, None, 0o0755,
            unsafe { libc::getuid() }, unsafe { libc::getgid() })?;
        dirs.insert(next_path.clone(), ino);
        cur_ino = ino;
        cur_path = next_path;
    }
    Ok(())
}

fn split_parent(path: &str) -> (String, String) {
    let p = path.trim_matches('/');
    match p.rfind('/') {
        Some(idx) => {
            let parent = format!("/{}", &p[..idx]);
            let name = p[idx + 1..].to_string();
            (parent, name)
        }
        None => ("/".to_string(), p.to_string()),
    }
}

fn run_fusermount(args: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
    let bin = if Path::new("/usr/bin/fusermount3").exists() {
        "fusermount3"
    } else {
        "fusermount"
    };
    let status = std::process::Command::new(bin).args(args).status()?;
    if !status.success() {
        return Err(format!("{bin} exited with {status}").into());
    }
    Ok(())
}

// ---- Graceful termination (signal handling) --------------------------------
//
// We can't safely call into fuser from a signal handler, so we use the classic
// self-pipe trick: the handler sets a flag and writes one byte to a pipe. The
// main thread blocks reading that pipe; the byte (or the EINTR the signal
// causes) wakes it, and it then drops the session to unmount cleanly. Covers
// Ctrl+C (SIGINT) and `kill` (SIGTERM). SIGKILL still relies on auto_unmount.
static TERM_RECEIVED: AtomicBool = AtomicBool::new(false);
static SELF_PIPE_R: AtomicI32 = AtomicI32::new(-1);
static SELF_PIPE_W: AtomicI32 = AtomicI32::new(-1);

extern "C" fn term_handler(_sig: libc::c_int) {
    TERM_RECEIVED.store(true, Ordering::SeqCst);
    // write(2) is async-signal-safe; a full pipe just means we're already woken.
    let w = SELF_PIPE_W.load(Ordering::Relaxed);
    if w >= 0 {
        let b = 1u8;
        unsafe {
            libc::write(w, &b as *const u8 as *const libc::c_void, 1);
        }
    }
}

fn install_termination_handlers() {
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return; // best-effort: without it we behave like before (kill leaves mount)
    }
    SELF_PIPE_R.store(fds[0], Ordering::Relaxed);
    SELF_PIPE_W.store(fds[1], Ordering::Relaxed);
    let h: extern "C" fn(libc::c_int) = term_handler;
    unsafe {
        libc::signal(libc::SIGINT, h as libc::sighandler_t);
        libc::signal(libc::SIGTERM, h as libc::sighandler_t);
    }
}

fn wait_for_termination() {
    let r = SELF_PIPE_R.load(Ordering::Relaxed);
    if r < 0 {
        // Signal setup failed; park forever so the default-kernel action on a
        // later SIGINT/SIGTERM (already unmasked) is what stops us.
        std::thread::sleep(std::time::Duration::from_secs(u64::MAX / 2));
        return;
    }
    let mut buf = [0u8; 16];
    loop {
        let n = unsafe { libc::read(r, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n > 0 {
            break;
        }
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) && TERM_RECEIVED.load(Ordering::SeqCst) {
                break;
            }
            if e.raw_os_error() != Some(libc::EINTR) {
                break; // pipe closed/errored: stop waiting
            }
        }
    }
    println!("received termination signal, unmounting cleanly...");
}

/// Spawn a watcher thread that unmounts `mountpoint` when the process gets
/// SIGINT/SIGTERM. Best-effort: if signal setup fails the thread simply idles.
fn spawn_term_unmounter(mountpoint: PathBuf) {
    std::thread::spawn(move || {
        install_termination_handlers();
        wait_for_termination();
        let _ = run_fusermount(&["-u", &mountpoint.to_string_lossy()]);
    });
}

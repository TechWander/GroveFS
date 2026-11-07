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
//!   gc     — offline orphan-object audit / reclaim for a LOCAL store
//!   data-server / meta-server — centralized storage/metadata services

use clap::{Parser, Subcommand};
use grovefs::backend;
use grovefs::fs_impl::GroveFS;
use grovefs::meta;
use grovefs::meta::MetaEngine;
use grovefs::meta::MetaProvider;
use grovefs::remote;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;

#[derive(Parser)]
#[command(
    name = "grovefs",
    version,
    about = "User-view FUSE filesystem",
    long_about = r#"User-view FUSE filesystem.

One physical object store (`<root>/blocks`, content-addressed by UUID), one
independent view tree per user (`<home>/meta/<view>.db`). Mounting a view
surfaces only that view; data IO is fully decoupled from the directory tree.

Runs standalone (a local `--home`), as a central cluster (a `data-server`
paired with a `meta-server` that many clients mount with `--data`/`--meta`),
or as an agent-facing governance service over MCP (`grovefs mcp`).
"#,
    after_long_help = r#"COMMON USAGE

  Local single machine:
    grovefs --home ~/grove init  --view alice
    grovefs --home ~/grove mount --view alice -m ~/mnt
    grovefs umount -m ~/mnt

  Central cluster (one shared view across machines):
    grovefs data-server --root /data    --listen 0.0.0.0:50100 --token SECRET
    grovefs meta-server --db-path /meta --listen 0.0.0.0:50053 --token SECRET
    grovefs mount --view alice -m /mnt --data host:50100 --meta host:50053 --token SECRET

  Onboard an existing tree (zero-copy when it shares the filesystem):
    grovefs import --db-path /meta --view alice --source /srv/legacy

  Per-caller mount credentials instead of one static cluster token (revocable,
  audited; a revoke closes a LIVE mount within 1s and it unmounts itself):
    grovefs apikey      --db-path /meta create --label analyst --for mount
    grovefs meta-server --db-path /meta --listen 0.0.0.0:50053 --require-api-key
    grovefs data-server --root /data    --listen 0.0.0.0:50100 --require-api-key --keys-from /meta
    grovefs mount --view alice -m /mnt --data host:50100 --meta host:50053 --api-key ID:SECRET

  Server-side governance (no local rules file needed on clients):
    grovefs view --db-path /meta define --name lens   --base alice --rules lens.rules
    grovefs view --db-path /meta alias  --name public --real alice

  Audit/reclaim orphan bytes in a LOCAL store (offline; scan is the default):
    grovefs gc --db-path ~/grovefs              # report orphans / ledger-held keys
    grovefs gc --db-path ~/grovefs --apply      # delete >24h orphans (store offline)

  AI-agent surface over MCP (stdio), guarded by an API key:
    grovefs apikey --db-path /meta create --label claude-desktop --for mcp   # prints <id>:<secret> once
    grovefs mcp    --db-path /meta --require-api-key
    grovefs mcp --help   # list the tools an agent can call

Run `grovefs <command> --help` for a command's full options.

  Long commands shrink: a KEY=VALUE file is auto-loaded into the environment
  ($GROVEFS_CONFIG, else $HOME/.config/grovefs/grovefs.conf),
  and the repeat-heavy flags fall back to env — GROVEFS_META / GROVEFS_DATA /
  GROVEFS_TOKEN for `mount` and the servers, GROVEFS_HOME, GROVEFS_DB_PATH,
  GROVEFS_STATS, GROVEFS_API_KEY, GROVEFS_MCP_API_KEY, GROVEFS_BW_READ /
  GROVEFS_BW_WRITE. Precedence is always flag > live env > config file;
  keep the file 600 if it holds secrets.
"#
)]
struct Cli {
    /// Base directory for LOCAL mode (single machine).
    ///
    /// Holds `blocks/` (data objects) and `meta/<view>.db` (one DB per view).
    /// Used by `init` and by a mount that has no `--meta`/`--data`.
    /// Defaults to $GROVEFS_HOME or ~/.grovefs.
    ///
    /// Remote mode does NOT touch it: clients name the servers directly with
    /// `mount --meta/--data`, so nothing is ever written under this path.
    /// On a storage node each service names its own store path instead
    /// (`meta-server --db-path`, `data-server --root`).
    #[arg(long, global = true, verbatim_doc_comment)]
    home: Option<PathBuf>,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Initialize a view (creates meta DB, optional seed manifest)
    Init {
        /// View to create (its metadata DB is initialized if absent).
        #[arg(long)]
        view: String,
        /// Optional manifest file: lines of "mkdir <path>" or "touch <path>"
        /// to pre-build a custom folder hierarchy.
        #[arg(long)]
        manifest: Option<PathBuf>,
    },
    /// Mount a view at a mountpoint (detaches into the background by default).
    ///
    /// The mount process keeps serving in the background (own session, output to
    /// `--log-file`) and this command returns once the mountpoint is live.
    /// Pass `--foreground` to keep it attached to the terminal (systemd
    /// `Type=exec`, debugging, or scripts that redirect stdout themselves).
    /// Tear down with `grovefs umount -m <mountpoint>`.
    #[command(after_long_help = r#"USAGE EXAMPLES

  Local single machine (foreground so you watch it attach):
    grovefs --home ~/grove mount --view alice -m ~/mnt --foreground

  Share ONE view across machines via the cluster (remote meta + data):
    grovefs mount --view alice -m /mnt --meta HOST:50053 --data HOST:50100 --token SECRET

  Fail over across meta-server replicas (stays on the first reachable node):
    grovefs mount --view alice -m /mnt --meta PRIMARY:50053,STANDBY:50053 --data HOST:50100

  Stripe one big file across several data-servers (comma list, order-stable):
    grovefs mount --view alice -m /mnt --data HOST1:50100,HOST2:50100,HOST3:50100

  Strict coherence (no client cache; a peer's writes show immediately):
    grovefs mount --view alice -m /mnt --meta HOST:50053 --data HOST:50100 --strict

  Read-only mount (every mutation returns EROFS):
    grovefs mount --view alice -m /mnt --ro

  Mount a server-side derived lens or public alias by its name:
    grovefs mount --view lens -m /mnt --meta HOST:50053 --data HOST:50100

  This mount only: a local presentation overlay from a rules file:
    grovefs --home ~/grove mount --view alice -m ~/mnt --layout my.rules

  Detaches by DEFAULT: returns once the mountpoint is live and serves in the
  background (output to --log-file); tear down with `grovefs umount -m ~/mnt`.
"#)]
    Mount {
        /// View to mount — a physical name, or a server-side derived/alias name.
        #[arg(long)]
        view: String,
        /// Existing directory to mount the view onto.
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
        /// API key (`<id>:<secret>`, minted by `grovefs apikey create --for
        /// mount`) presented to a `meta-server`/`data-server` started with
        /// `--require-api-key` instead of (or alongside) a static `--token`.
        /// Unlike `--token`, this is a per-caller credential: the operator can
        /// list it, audit the connects behind it (`grovefs apikey calls`) and
        /// revoke it — a revoke lands on THIS live mount at its first server
        /// round-trip after the 1s re-check window (a read answered purely from
        /// the kernel cache needs no round-trip, so it is not interrupted), after
        /// which the mount prints the server's reason and unmounts itself rather
        /// than serving a broken view. Falls back to env
        /// `GROVEFS_API_KEY` so a secret need not sit in a command line. Refused
        /// at connect time if the key is unknown/revoked, or scoped to `mcp`.
        #[arg(long)]
        api_key: Option<String>,
        /// Extra fuser options, e.g. allow_root, allow_other
        #[arg(long, num_args = 0..)]
        o: Vec<String>,
        /// FUSE worker threads dispatching requests concurrently (fuser 0.18
        /// session `n_threads`). 0 = single-threaded mode, useful as an A/B
        /// knob for the bench. Each thread gets its own /dev/fuse fd via
        /// `FUSE_DEV_IOC_CLONE` (Linux 4.5+), so kernel replies are not serialized
        /// behind one channel lock.
        #[arg(long, default_value_t = 4)]
        threads: usize,
        /// Report a specific uid on the `M_OPEN` handshake (and on the
        /// invalidation subscription) instead of the process's real euid, so a
        /// shared `meta-server` records lock arbitration and audit rows under a
        /// chosen owner. Remote mounts only: a local `--home` mount has no
        /// server to report to, and its access control is the OS's. Advisory:
        /// the client self-reports its uid, and the token / API key is the real
        /// gate against a malicious peer.
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
        /// Cap this mount's CONTENT read throughput, e.g. `50M`, `1G` or a
        /// plain byte count (1024-based). 0/unset = unlimited (the default).
        /// A shared GCRA slot schedule in the FUSE read path hands bytes back
        /// at exactly this rate (bytes are paced from the first op; a single
        /// op larger than one second of budget may start up to 1 s early).
        /// Content writes have their own knob (`--bw-write`); metadata ops
        /// are never throttled. Falls back to env `GROVEFS_BW_READ`. Honest
        /// boundary: a per-mount, CLIENT-side shaper — a raw data-plane
        /// client that bypasses FUSE, or N mounts of the same view, each get
        /// their own budget.
        #[arg(long, value_parser = grovefs::bwlimit::parse_rate)]
        bw_read: Option<u64>,
        /// Cap this mount's CONTENT write throughput (same syntax; env
        /// fallback `GROVEFS_BW_WRITE`). Inert on read-only mounts: their
        /// writes are refused with EROFS before any budget is spent.
        #[arg(long, value_parser = grovefs::bwlimit::parse_rate)]
        bw_write: Option<u64>,
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
        /// Stay attached to this terminal instead of detaching to the
        /// background (the traditional behavior; required for systemd
        /// `Type=exec` units). Informational output goes to stdout/stderr.
        #[arg(long)]
        foreground: bool,
        /// When detached (the default): file the background mount's
        /// stdout/stderr append into. Defaults to
        /// `$TMPDIR/grovefs-mount-<mountpoint>.log`; pass `/dev/null` for a
        /// fully silent mount. Ignored with `--foreground` unless the
        /// caller's stdio is a terminal.
        #[arg(long)]
        log_file: Option<PathBuf>,
    },
    /// Run the remote data service (central cluster node) backed by local disk
    DataServer {
        /// Listen address, e.g. 0.0.0.0:50052
        #[arg(short, long, default_value = "127.0.0.1:50052")]
        listen: String,
        /// Directory holding the physical objects on this node. A
        /// comma-separated list (dir1,dir2,...) spreads objects across several
        /// storage roots (disks) automatically: each whole object is placed on
        /// one root by a hash of its key (chunk families pin to their object's
        /// root), so no client or metadata change is needed and `df` aggregates
        /// the disks. The root list must stay FIXED for a given store — adding
        /// or removing a disk re-maps every key. A single path behaves as before.
        #[arg(short, long)]
        root: PathBuf,
        /// Shared secret clients must present (`mount --token`). When set, the
        /// server rejects any `OP_OPEN` whose token differs, so an unauthorized
        /// host on the LAN can no longer read or write arbitrary objects by
        /// key. Empty (default) keeps the original LAN-trusted, no-auth behavior.
        #[arg(long)]
        token: Option<String>,
        /// Gate `OP_OPEN` on a revocable per-caller credential instead of (or in
        /// addition to) a static `--token`: every client must present a live key
        /// minted by `grovefs apikey create --for mount` (`mount --api-key`).
        /// Connects and revokes are logged, and the key is re-verified on live
        /// connections (once per second), so a revoke closes an established
        /// mount rather than only refusing the next one. The store is
        /// `<keys-from>/meta/api_keys.db`; `--keys-from` defaults to this node's
        /// first `--root`, so a colocated deployment needs no extra flag and a
        /// split one points at a synced/colocated copy of that file.
        #[arg(long)]
        require_api_key: bool,
        /// Metadata root holding `meta/api_keys.db` for `--require-api-key` (the
        /// same layout `grovefs apikey --db-path` manages). Defaults to the
        /// first `--root`.
        #[arg(long)]
        keys_from: Option<PathBuf>,
        /// Key/audit backend for `--require-api-key`. Unset / `"sqlite"` reads
        /// the local `<keys-from>/meta/api_keys.db`; a `postgres://…`
        /// (`postgresql://…`) URI — or the `GROVEFS_STORE` env — reads the shared
        /// `grovefs_control.api_keys` table, so a data-server on a PG deployment
        /// gates mounts against the SAME registry its meta-server does with no
        /// file to colocate (and `--keys-from` is then unused).
        #[arg(long)]
        store: Option<String>,
    },
    /// Run the central metadata service many `mount --meta` clients share.
    ///
    /// Hosts the authoritative per-view DB that those clients share (no NFS);
    /// pairs with `data-server`.
    MetaServer {
        /// Listen address, e.g. 0.0.0.0:50053
        #[arg(short, long, default_value = "127.0.0.1:50053")]
        listen: String,
        /// Authoritative metadata root for this server: under SQLite every
        /// view DB lives under `<db-path>/meta/<view>.db` (created explicitly
        /// by `view create` / `init` / `view import`, never by a mount) and
        /// the API-key/audit store lives at `<db-path>/meta/api_keys.db`;
        /// under PostgreSQL (`--store postgres://…`) the view trees live in
        /// PG, making `<db-path>` a config-only directory (derived-view
        /// definitions, logs). Takes precedence over the global `--home` when
        /// both are given.
        #[arg(long)]
        db_path: Option<PathBuf>,
        /// Shared secret clients must present (`mount --token`). When set, the
        /// server refuses any `M_OPEN` whose token differs, so an unauthorized
        /// host on the LAN can no longer open arbitrary view trees. Empty
        /// (default) keeps the original LAN-trusted, no-auth behavior.
        #[arg(long)]
        token: Option<String>,
        /// Gate `M_OPEN` on a revocable per-caller credential instead of (or in
        /// addition to) a static `--token`: every client must present a live key
        /// minted by `grovefs apikey create --for mount` (`mount --api-key`). The
        /// store is `<db-path>/meta/api_keys.db` — the same file `grovefs mcp
        /// --db-path` authenticates against and `grovefs apikey --db-path`
        /// manages, so one credential registry serves both entrances while key
        /// SCOPES keep them apart. Connects and revokes are audited, and live
        /// connections are re-verified once per second, so a revoke closes an
        /// established mount (which then unmounts itself with the server's
        /// reason printed) rather than only refusing the next mount.
        #[arg(long)]
        require_api_key: bool,
        /// HA role: `primary` (default) serves reads+writes and ships snapshots to
        /// `--peer`; `standby` serves reads only from the replicated copy, rejects
        /// writes, and promotes itself if the primary's heartbeats stop.
        #[arg(long, default_value = "primary")]
        role: String,
        /// The other HA node's `host:port`. A primary pushes every ~500ms and
        /// heartbeats its epoch here; a standby promotes after missing heartbeats.
        /// Requires a *separate* metadata root (`--db-path`, or the global
        /// `--home`) from the peer (each node keeps its own
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
        #[arg(long)]
        store: Option<String>,
    },
    /// Unmount a mountpoint
    Umount {
        /// The mountpoint to unmount (terminates its background mount).
        #[arg(short, long)]
        mountpoint: PathBuf,
    },
    /// Manage server-side views: base trees, derived lenses, pure aliases and forks.
    ///
    /// A BASE view is a physical tree (`meta/<name>.db`): mountable read-write,
    /// the only kind whose deletes do real garbage collection. A DERIVED view is a
    /// named presentation lens over ONE physical tree, mounted read-only; a pure
    /// ALIAS maps a public name to a real name (writable, real name not
    /// mountable); a FORK is an independent metadata tree seeded from a base —
    /// structure writable, content read-only (EROFS), deletes touch only the
    /// fork. Definitions live in the meta root
    /// (`<db-path>/meta/views/<name>.layout`), clients simply mount
    /// `--view <name>` and need no local rules file. A `meta-server` resolves
    /// them at `M_OPEN`; a local mount resolves them from `--home`. Rules are
    /// display customization, NOT a security boundary — hard isolation stays
    /// view + API-key/token (mounting the base name directly shows the full
    /// tree to whoever is allowed past its gates). Derived (virtual) views
    /// mount READ-ONLY: the shape is centrally governed, so every write goes
    /// to the original (base) view — a mount of a derived name refuses all
    /// mutations with EROFS (kernel read-only flag negotiated at `M_OPEN`,
    /// plus an authoritative per-op gate on the meta-server). A fork mounts
    /// read-write for the tree and refuses only content writes (write/truncate)
    /// with EROFS — organize names freely, upload bytes to the base view.
    View {
        /// The metadata root holding `meta/views/*.layout` — same option name
        /// and directory as `meta-server --db-path` / `grovefs apikey --db-path`;
        /// falls back to the global `--home` when omitted.
        #[arg(long)]
        db_path: Option<PathBuf>,
        /// Control-plane backend. Unset / `"sqlite"` governs the local
        /// `<db-path>/meta/views/*.layout` files; a `postgres://…`
        /// (`postgresql://…` too) URI — or the `GROVEFS_STORE` env — governs the
        /// shared `grovefs_control.view_defs` table instead, so definitions and
        /// the meta-server's own resolver read/write ONE system of record. Must
        /// match the `meta-server --store` this root is served by.
        #[arg(long)]
        store: Option<String>,
        #[command(subcommand)]
        action: ViewAction,
    },
    /// Serve view governance over MCP (stdio) so an AI agent can drive it.
    ///
    /// An MCP client (agent runtime) spawns this as a subprocess and the agent
    /// drives the same validated operations as `grovefs view`: list_views /
    /// define_derived_view / alias_view / remove_view / preview_rules — rules
    /// travel as inline text (hide/only/map/link), so an agent customizes a
    /// data listing's shape without touching files. Implements the initialize /
    /// ping / tools/list / tools/call core of the 2025-06-18 protocol revision
    /// with newline-delimited JSON-RPC 2.0 framing. Control plane only: it
    /// writes `<db-path>/meta/views/*.layout` definitions (live meta-server
    /// mounts hot-reload them) but never mounts or touches data. `--db-path`
    /// sets the default metadata root (falls back to `--home`); a per-call
    /// `db_path` argument overrides it.
    #[command(
        after_long_help = r#"MCP TOOL SURFACE — what `tools/list` returns (10 tools).
Every tool takes an optional `db_path` (defaults to this server's --db-path/--home),
carries MCP annotations (readOnlyHint/destructiveHint/idempotentHint) for client
approval prompts, and the four JSON-returning ones also declare an outputSchema
and return structuredContent alongside the human-readable text.

  DISCOVERY (read-only, never mutates; metadata only, no data bytes):
    list_views            every slot: physical trees + derived lenses + forks + aliases (JSON summary)
    get_view              one view in full — the verbatim rules, for read-modify-write editing
    list_view_tree        the exact listing a mount shows; also previews an unpublished base+rules lens

  GOVERNANCE (writes <db>/meta/views/*.layout definitions; never mounts or touches data):
    define_derived_view   create/replace a READ-ONLY derived lens (name, base, rules: hide/only/map/link)
    fork_view             create an INDEPENDENT tree (name, base): structure writable, content read-only
    graft_view            copy a curated, renamable subset into a fork/base view (name, map plan)
    alias_view            create/replace a WRITABLE public alias (name, real; `retire` rotates gaplessly)
    remove_view           drop a definition by name (a fork also drops its own tree; data bytes stay while held)

  DRY RUN (validate a candidate before publishing; writes nothing):
    preview_rules         parse-check rules text alone (line-level error + accepted counts)
    validate_definition   full governance pre-check — returns the exact refusal a real publish would give

  BEYOND TOOLS (same store, no extra configuration):
    resources             grovefs://guide, grovefs://views,
                          grovefs://view/<name>/definition|tree{?path,depth,max}
    prompts               reshape-view, rotate-alias playbooks
    completion            fills view-name arguments from the store
    logging/setLevel      streams notifications/message (naming the tool) per call

NOTES
  Protocol: 2025-06-18 over newline-delimited JSON-RPC 2.0 on stdio; the
    2025-03-26 and 2024-11-05 revisions are accepted too (an unknown revision
    is answered with 2025-06-18 rather than echoed).
  Control plane only: it reshapes how a view LIST is presented; it never mounts
    a filesystem or reads/writes file bytes.
  Auth: start with --require-api-key, mint an MCP-scoped key via
    `grovefs apikey create --label <name> --for mcp`, present it as `apiKey` in
    the initialize params (or export GROVEFS_MCP_API_KEY).
"#
    )]
    Mcp {
        /// Metadata root holding `meta/views/` — same option name and
        /// directory as `grovefs view` / `meta-server --db-path`.
        #[arg(long)]
        db_path: Option<PathBuf>,
        /// Require an API key before the tool surface is usable. With it, an
        /// MCP client must present a `grovefs apikey create` key as `apiKey` in
        /// the `initialize` params (or the operator exports
        /// `GROVEFS_MCP_API_KEY` to pre-authenticate the whole server); until a
        /// key authenticates, `tools/list` and `tools/call` are refused with a
        /// JSON-RPC error. `initialize`/`ping` stay open so a client can always
        /// handshake. Off by default: a plain `grovefs mcp` trusts whoever can
        /// spawn it (the same OS boundary as a local mount). Either way, every
        /// tool call is appended to `<db-path>/meta/api_keys.db`'s audit log.
        #[arg(long)]
        require_api_key: bool,
        /// Control-plane backend. Unset / `"sqlite"` governs the local
        /// `<db-path>/meta/views/*.layout` files and `api_keys.db`; a
        /// `postgres://…` (`postgresql://…`) URI — or the `GROVEFS_STORE` env —
        /// binds the server to the shared `grovefs_control` schema (definitions,
        /// keys and the call audit all in PG). A PG-bound server ignores a
        /// per-call `db_path` override: one URI, one system of record.
        #[arg(long)]
        store: Option<String>,
    },
    /// Manage the MCP server's API keys and read its call-audit log.
    ///
    /// Keys authenticate callers of `grovefs mcp --require-api-key`; they are
    /// stored as SHA-256 hashes (only the minting `create` shows the plaintext
    /// once), so a leaked `api_keys.db` hands out nothing usable. Revoking
    /// keeps the row (so past calls stay attributed) but fails authentication.
    /// The audit log records every tool call (timestamp, key, tool, success,
    /// detail).
    Apikey {
        /// The metadata root holding `meta/api_keys.db` — same option name and
        /// directory as `grovefs mcp --db-path` / `meta-server --db-path`; falls
        /// back to the global `--home`.
        #[arg(long)]
        db_path: Option<PathBuf>,
        /// Key/audit backend. Unset / `"sqlite"` uses the local
        /// `<db-path>/meta/api_keys.db`; a `postgres://…` (`postgresql://…`)
        /// URI — or the `GROVEFS_STORE` env — uses the shared
        /// `grovefs_control.api_keys`/`mcp_calls` tables, so a key minted here
        /// is authoritative for every meta-/data-server and MCP pointing at that
        /// URI with no per-node file to colocate.
        #[arg(long)]
        store: Option<String>,
        #[command(subcommand)]
        action: ApikeyAction,
    },
    /// Inspect and repair the cross-view blob reference ledger (the shared
    /// `meta/refs.db` that decides when a physical object may actually be
    /// deleted). A blob is freed only when its last name in EVERY view holding
    /// it is removed; this ledger is what makes a delete in one view safe for
    /// another view sharing the same object (e.g. `import` of one source into
    /// two views). Normally maintenance-free — it is updated atomically with
    /// every create/delete — but a view db removed OUT-OF-BAND (`rm meta/x.db`,
    /// a fresh `--home`) never released its holds, so the ledger over-counts and
    /// keeps bytes too long. `refs rebuild` rewrites it from the trees that
    /// actually exist. SQLite-backed stores only. See `grovefs view create`.
    Refs {
        /// The metadata root holding `meta/refs.db` and the per-view trees
        /// `meta/*.db` — same option name and directory as `meta-server
        /// --db-path`; falls back to the global `--home`.
        #[arg(long)]
        db_path: Option<PathBuf>,
        #[command(subcommand)]
        action: RefsAction,
    },
    /// Audit and reclaim ORPHAN objects in a LOCAL store (offline GC).
    ///
    /// An orphan is a page under `<data-root>/blocks/` that NO surviving view
    /// tree names and the cross-view ledger does not book — the residue of an
    /// out-of-band `rm meta/<view>.db` (no unlink ever ran) or a crash between
    /// object creation and the node insert. SCAN IS THE DEFAULT: it prints the
    /// counts, the biggest orphans and any ledger-held keys that must first be
    /// released with `grovefs refs rebuild`.
    ///
    /// `--apply` deletes only objects older than `--older-than` (a hard-linked
    /// import inherits the SOURCE file's mtime, so young-object filtering alone
    /// is not enough), never a key the ledger still holds, and refuses to run
    /// while this store is mounted or served by a local grovefs process. A
    /// view tree that cannot be read aborts the whole scan rather than guessing
    /// — a skipped tree would make every blob it names look orphaned.
    Gc {
        /// Metadata root holding `meta/*.db` — same option name and directory
        /// as `meta-server --db-path`; falls back to the global `--home`.
        #[arg(long)]
        db_path: Option<PathBuf>,
        /// Data root(s) holding `blocks/` — comma-separated for several disks,
        /// same convention as `data-server --root`; defaults to `--db-path`.
        #[arg(long)]
        data_root: Option<PathBuf>,
        /// Minimum object age to reclaim (`30m`, `24h`, `7d`; `0` disables the
        /// grace window). Younger objects are never deleted: an in-flight
        /// create/import may not have committed its node row yet.
        #[arg(long, default_value = "24h", value_name = "DURATION")]
        older_than: String,
        /// Delete the reclaimable candidates (default is a dry-run report).
        #[arg(long)]
        apply: bool,
        /// Emit one JSON object instead of the text report.
        #[arg(long)]
        json: bool,
        /// Bypass the "this data root shares no object with any tree" guard —
        /// only for a store whose every surviving reference lives elsewhere.
        #[arg(long)]
        force: bool,
    },
    /// Import an existing local directory tree into a view, zero-copy where possible.
    ///
    /// Every regular file is registered as an object — hard-linked into
    /// `blocks/` when source and store share a filesystem (zero extra bytes;
    /// edits through either path stay visible in the other), streamed as a copy
    /// otherwise — and the directory structure is mirrored into the view tree
    /// under `--prefix`. Run it where the store lives (local `--home`, or the
    /// meta/data servers' root — same operational envelope as `grovefs
    /// view`). Re-running is an INCREMENTAL import: names already
    /// in the view are skipped. Symlinks and special files are skipped (counted,
    /// never followed). Timestamps, mode and owner are preserved from the
    /// source. The target must be a PHYSICAL view — importing into a derived
    /// (virtual) name is refused, since virtual views mount read-only. Live
    /// `meta-server` mounts converge within their attr/dentry TTL (≤2s, no
    /// remount); a PG backend instead goes through a mounted path (rsync) since
    /// the authoritative tree lives in Postgres, not in a local db file.
    Import {
        /// Metadata root holding `meta/<view>.db` — same option name and
        /// directory as `meta-server --db-path`; falls back to `--home`.
        #[arg(long)]
        db_path: Option<PathBuf>,
        /// Data root holding `blocks/` — same option name and values as
        /// `data-server --root` (a comma-separated list spreads imported
        /// objects across the node's disks by the same key-hash placement);
        /// defaults to `--db-path` (the colocated topology used by every
        /// deployment example).
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

    /// Report disk usage (subtree byte totals) without going through FUSE.
    /// Connects directly to the metadata backend (PG or SQLite) for the tree
    /// structure, then batch-stats content sizes via the data-server (or local
    /// blocks/). Output is `du`-compatible: `<size>\t<path>`, sorted by depth.
    Du {
        /// View to measure.
        #[arg(long)]
        view: String,
        /// Meta-server endpoint (host:port) — uses M_TREE_DUMP RPC to get
        /// the tree without direct PG/SQLite access.
        #[arg(long)]
        meta: Option<String>,
        /// Access token for meta-server (if --meta requires auth).
        #[arg(long)]
        meta_token: Option<String>,
        /// API key for meta-server (if --meta requires auth).
        #[arg(long)]
        meta_api_key: Option<String>,
        /// PostgreSQL store URI (same as meta-server --store).
        #[arg(long)]
        store: Option<String>,
        /// Metadata root for SQLite mode (meta/<view>.db lives under here).
        #[arg(long)]
        db_path: Option<PathBuf>,
        /// Data-server endpoint(s) for batch stat: `host:port`, or a
        /// comma-separated list in the same order the mount used. Needed when
        /// the tree comes through `--meta`/`--store`, which imply no local
        /// storage root to stat. A striped store (`s<N>` data_refs) can only
        /// be measured with exactly the `N` addresses the writer used: a list
        /// of any other length is refused in either direction, not zeroed.
        #[arg(long)]
        data: Option<String>,
        /// Local data root holding blocks/; defaults to the store root named
        /// by `--db-path`/`--home` (same convention as `grovefs import
        /// --data-root`). Without either a local root or `--data`, sizes would
        /// be guessed at zero, so the command refuses.
        #[arg(long)]
        data_root: Option<PathBuf>,
        /// API key for data-server authentication.
        #[arg(long)]
        api_key: Option<String>,
        /// Access token for data-server (legacy --token mode).
        #[arg(long)]
        token: Option<String>,
        /// Subpath within the view to start from (default: root /).
        #[arg(long, default_value = "/")]
        path: String,
        /// Show per-directory breakdown up to N levels deep (0 = only the
        /// grand total of --path, 1 = plus immediate children, etc.).
        #[arg(long, default_value = "999999")]
        depth: usize,
        /// Print raw byte counts (no human K/M/G suffix).
        #[arg(long, short = 'b')]
        bytes: bool,
        /// Print only the grand total (equivalent to --depth 1 --path /).
        #[arg(long)]
        total: bool,
    },
}

#[derive(Subcommand)]
enum ViewAction {
    /// Create a NEW BASE (physical) view named `--view`, seeding its empty tree
    /// root (optionally from `--manifest`). This is the deliberate, explicit
    /// act of standing up a real storage tree — the one kind of view whose
    /// deletes do REAL garbage collection: removing a file's last name in a base
    /// view, once no derived lens or other view still references the bytes
    /// (cross-view ledger), frees the underlying object. Derived views
    /// (`define`), forks (`fork`) and aliases (`alias`) only reshape, copy or
    /// rename a base view; they never own physical deletion. Equivalent to
    /// `grovefs init --view NAME`,
    /// offered here so base-view creation reads as an explicit first-class step.
    Create {
        /// Name of the new base (physical) view; its tree lives at
        /// `<db-path>/meta/<view>.db`.
        #[arg(long)]
        view: String,
        /// Optional seed manifest (`name<TAB>content` lines) to populate the
        /// freshly created base view with, exactly like `init --manifest`.
        #[arg(long)]
        manifest: Option<PathBuf>,
    },
    /// Create a FORK `--name` of the physical base view `--base`: an
    /// INDEPENDENT metadata tree (`<db-path>/meta/<name>.db`) seeded with a
    /// point-in-time copy of the base's names. Content bytes are SHARED (booked
    /// in the cross-view ledger, no copies), so a fork costs metadata only.
    ///
    /// A fork is the writable middle ground between a base view and a lens:
    /// structure (mkdir/rename/unlink/symlink/link) is allowed, but CONTENT
    /// writes (write/truncate) are refused with EROFS — uploading bytes belongs
    /// to a base view. Deleting a name removes only the fork's own metadata;
    /// the base's tree is untouched and a shared object is freed only when no
    /// view holds a name for it anymore. Removing the fork (`view rm`) deletes
    /// its tree and rebuilds the ledger; content bytes the base still names stay.
    ///
    /// The base must be a physical view (a name with its own `meta/<base>.db`
    /// and no definition) backed by local SQLite; PostgreSQL-backed views cannot
    /// be forked yet.
    Fork {
        #[arg(long)]
        name: String,
        #[arg(long)]
        base: String,
        /// Start EMPTY (root row only) instead of copying the base's entries,
        /// to be filled by `view graft`. Same guards and the same mount
        /// semantics; only the initial row set differs.
        #[arg(long)]
        empty: bool,
    },
    /// Graft entries of view `--src` into fork `--name`, at arbitrary
    /// destinations and under new names. One plan line per entry:
    /// `<src>` (keep its basename, placed in `--dest`) or `<src>TAB<dst>`
    /// (rename and/or another directory). `--map -` reads the plan from
    /// stdin, so an external program can generate it on the fly. A directory
    /// source brings its whole subtree; the placement is ONE transaction (all
    /// or nothing) and copies no bytes — rows enter the fork, objects stay
    /// shared through the cross-view ledger. An existing name refuses the
    /// plan unless `--skip-existing` leaves it alone. Only a view that OWNS
    /// its tree (a fork) can receive.
    Graft {
        /// Fork view receiving the entries.
        #[arg(long)]
        name: String,
        /// Plan file; `-` reads the plan from stdin.
        #[arg(long)]
        map: PathBuf,
        /// View whose entries are taken; defaults to the fork's base. A BASE
        /// view must be named explicitly (nothing infers which tree to lift).
        #[arg(long)]
        src: Option<String>,
        /// In-fork destination directory for lines without a TAB; defaults
        /// to the fork root. Missing directories are created.
        #[arg(long)]
        dest: Option<String>,
        /// Leave names that already exist instead of refusing the whole plan.
        #[arg(long)]
        skip_existing: bool,
    },
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
    /// List every view: bare base trees (`meta/<name>.db` with no definition),
    /// derived views (name, base, rule counts, read-only), forks (independent
    /// trees, structure writable / content read-only) and pure aliases
    /// (name → real view, writable).
    List,
    /// Remove a view. WITHOUT `--base`: remove the view DEFINITION `--name`
    /// (derived, fork or alias — the file slot is shared). For derived/alias
    /// the underlying data is untouched; already-mounted clients keep their
    /// resolved lens until they remount (or the server's cache is evicted by an
    /// HA/snapshot event). Removing an alias also makes the real name mountable
    /// again for NEW mounts. For a FORK this also deletes its independent
    /// metadata tree and rebuilds the cross-view ledger — the base's names and
    /// any still-held bytes stay.
    ///
    /// WITH `--base`: remove an entire physical BASE view `--name` from a local
    /// SQLite store — its metadata tree dies, the cross-view ledger is rebuilt
    /// from the surviving trees, and every content blob no other view holds is
    /// freed through the store's data root (`--data-root`, default `--home`;
    /// point it at the root/data-server that actually stores the objects).
    /// Guards: a governed name needs the plain `view rm` first, an actively
    /// mounted `grovefs:<name>` is refused, a `--store postgres://…` deployment
    /// is refused (its tree is a PG schema — drop the schema and let PG/data
    /// servers own the bytes). Other machines' live mounts of the same
    /// meta-server are not visible here — stop them first.
    Rm {
        #[arg(long)]
        name: String,
        /// Also remove the PHYSICAL base tree and reclaim unreferenced bytes
        /// (see the long help; default removes only a definition slot).
        #[arg(long)]
        base: bool,
        /// Data root holding `blocks/` for `--base` byte reclaim (same option
        /// name and fallback as `grovefs import --data-root`).
        #[arg(long)]
        data_root: Option<PathBuf>,
    },
    /// Create/replace a PURE ALIAS `--name` for physical `--real`: name
    /// governance (vs `define`'s shape governance). No rules, so the alias
    /// shows the FULL tree and stays WRITABLE — it simply is the real view
    /// under a public name. A `meta-server` then refuses mounts of the real
    /// name itself (and a local `--home` mount honors the same contract):
    /// while the alias exists the real name is not a mount entry point, so
    /// public names can rotate freely without exposing the storage identity.
    /// Chained aliases, aliases over derived views, and aliasing an
    /// already-governed real name are rejected at this command.
    ///
    /// To ROTATE a leaked public name in one gapless step, add `--retire
    /// <old-alias>`: the new alias is written before the old one is removed,
    /// so the real name stays governed throughout (never briefly mountable by
    /// its own name) — the leaker's old name stops resolving, legitimate users
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
    /// Export ONE view's METADATA TREE to a portable line-delimited JSON file
    /// (`--out`): a header plus one node row per line (ids, names, inode
    /// numbers, timestamps, `data_ref` keys). Content bytes are NOT included —
    /// blobs are referenced by `data_ref`, whose on-disk bucket path is a pure
    /// function of the key, so copying the store's `blocks/` tree alongside
    /// the export reproduces the view on another machine. No host path, device
    /// id or hostname is stored. Virtual names (alias/derived) have no tree of
    /// their own and are refused — export the physical base they resolve to.
    Export {
        #[arg(long)]
        view: String,
        #[arg(long)]
        out: PathBuf,
        /// Overwrite `--out` if it already exists.
        #[arg(long)]
        force: bool,
    },
    /// Import a metadata tree written by `view export` into a NEW local base
    /// tree (`meta/<view>.db`), preserving ids/inode numbers (hard links stay
    /// hard links) and rebuilding the cross-view ledger afterwards. The target
    /// must not be governed by a definition; an existing tree needs
    /// `--replace`, and every mount/meta-server using this store must be
    /// stopped first. Content bytes are not in the file: copy the referenced
    /// `blocks/` tree to the target store. Add `--as-fork <base>` to record
    /// `fork <base>` after a successful import (the imported tree then keeps
    /// fork semantics: structure writable, content EROFS).
    Import {
        #[arg(long)]
        view: String,
        #[arg(long = "in")]
        input: PathBuf,
        /// Replace an existing `meta/<view>.db` (offline migration only).
        #[arg(long)]
        replace: bool,
        /// Record the imported tree as a fork of this physical base view
        /// (the base tree must already exist in the target store).
        #[arg(long = "as-fork")]
        as_fork: Option<String>,
    },
}

#[derive(Subcommand)]
enum ApikeyAction {
    /// Mint a new key labeled `--label` and print the full plaintext
    /// `<id>:<secret>` ONCE (it is not recoverable afterward — only its
    /// SHA-256 is stored). Give this to the agent/client; it presents it as
    /// `apiKey` in the MCP `initialize` params, or as `mount --api-key` for a
    /// `meta-server`/`data-server` started with `--require-api-key`.
    Create {
        #[arg(long)]
        label: String,
        /// Which entrance the key may open: `mount` (a `--require-api-key`
        /// meta/data-server), `mcp` (`grovefs mcp --require-api-key`) or `any`
        /// (both — the default, and what every key minted before scopes existed
        /// carries). Narrow it so a mount credential cannot be replayed against
        /// the governance API, and an agent key cannot mount data.
        #[arg(long, default_value = "any")]
        r#for: String,
        /// Bind the key to ONE physical view: it can only open `--view NAME`
        /// (any other view's M_OPEN is refused). Omit for a system-level key
        /// that may connect to any view. This is the credential-level mechanism
        /// for the common "this client only touches that tree" case.
        #[arg(long)]
        view: Option<String>,
        /// Make the key read-only: the server refuses every write-intent op
        /// (create/write/truncate/remove) from it, so an accidental `mount`
        /// without `-o ro` still cannot change data. Deleting the last name in
        /// a read-only view never frees the underlying blob either (only a
        /// read-write key on the BASE view does real GC — see `view create`).
        #[arg(long)]
        ro: bool,
    },
    /// List every key (id, label, scope, view binding, ro/rw,
    /// created/last-used/revoked) — never any secret material. Add `--view NAME`
    /// to show only the keys that can open that view (a pure alias also matches
    /// keys bound to its real view).
    List {
        /// Only keys that can open this view: an exact `--view` binding, or
        /// (when NAME is a pure alias) keys bound to its real view. Unbound
        /// system keys admit every view and are summarized separately.
        #[arg(long)]
        view: Option<String>,
    },
    /// Revoke the key with id `--id` (short id prefix accepted). Revoked keys
    /// fail authentication but stay listed so past audit rows keep attribution.
    Revoke {
        #[arg(long)]
        id: String,
    },
    /// Flip the read-only flag of an ACTIVE key IN PLACE (short id prefix
    /// accepted, like `revoke`). Default sets it read-only; `--rw` restores
    /// read-write. This moves only the server-side write verdict — it never
    /// touches liveness or authentication — and a live mount picks the new
    /// verdict up on its next re-check tick (≤1s), no reconnect, no remount
    /// (the same key's reads keep flowing while it is `ro`). A revoked id is
    /// refused: a revoke is permanent and set-ro never half-undoes it.
    SetRo {
        #[arg(long)]
        id: String,
        /// Flip to read-only (the default when neither `--ro` nor `--rw` is given).
        #[arg(long)]
        ro: bool,
        /// Flip to read-write (mutually exclusive with `--ro`).
        #[arg(long, conflicts_with = "ro")]
        rw: bool,
    },
    /// Show the most recent `--limit` (default 20) MCP call-audit rows.
    Calls {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
}

#[derive(Subcommand)]
enum RefsAction {
    /// Recompute `meta/refs.db` from the surviving view trees: for every
    /// `data_ref`, count the DISTINCT views still holding a name for it and
    /// rewrite the holder rows. Clears holds stranded by an out-of-band view-db
    /// removal (the only over-count source) so their blobs become freeable again.
    /// A missing/empty ledger is (re)built; healthy stores need no rebuild.
    Rebuild,
}

/// Auto-load a `KEY=VALUE` config file into the environment, WITHOUT clobbering
/// variables that are already set — the precedence chain for every knob below
/// is always flag > live env > config file > built-in default. Search order:
/// `$GROVEFS_CONFIG` (explicit path) > `$HOME/.config/grovefs/grovefs.conf`;
/// the first existing file wins. Only
/// `GROVEFS_`-prefixed keys are honored (the rest are ignored with a warning);
/// recognized: GROVEFS_HOME, GROVEFS_META / GROVEFS_DATA / GROVEFS_TOKEN
/// (mount defaults), GROVEFS_STATS, GROVEFS_API_KEY, GROVEFS_MCP_API_KEY, …
/// The file is read once before clap parses, so the existing per-knob env
/// fallbacks consume the values without any extra plumbing; the mount
/// re-exec keeps them (the child inherits the environment). Secrets do belong
/// in here only if the file itself is protected (`chmod 600`) — exactly the
/// contract `GROVEFS_API_KEY` already carries as a plain environment variable.
fn load_config_file() {
    let path = std::env::var("GROVEFS_CONFIG")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME").ok().map(|h| {
                PathBuf::from(&h)
                    .join(".config")
                    .join("grovefs")
                    .join("grovefs.conf")
            })
        });
    let Some(path) = path else { return };
    let Ok(text) = std::fs::read_to_string(&path) else {
        // An explicit $GROVEFS_CONFIG that cannot be read is a misconfiguration
        // worth a warning; a default location that does not exist is normal.
        if std::env::var("GROVEFS_CONFIG").is_ok() {
            eprintln!(
                "warning: GROVEFS_CONFIG={} not readable; continuing with env/defaults",
                path.display()
            );
        }
        return;
    };
    for (lineno, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Optional `export ` prefix so a grovefs.conf doubles as a shell file.
        let line = line.strip_prefix("export ").unwrap_or(line).trim();
        let Some((key, value)) = line.split_once('=') else {
            eprintln!(
                "warning: {}:{}: not a KEY=VALUE line; ignored",
                path.display(),
                lineno + 1
            );
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        if !key.starts_with("GROVEFS_") {
            eprintln!(
                "warning: {}:{}: {} is not a GROVEFS_* knob; ignored",
                path.display(),
                lineno + 1,
                key
            );
            continue;
        }
        // Quote-aware so values may contain spaces (paths, rule text).
        let value = match value {
            v if (v.starts_with('"') && v.ends_with('"') && v.len() >= 2)
                || (v.starts_with('\'') && v.ends_with('\'') && v.len() >= 2) =>
            {
                v[1..v.len() - 1].to_string()
            }
            v => v.to_string(),
        };
        // Live environment always wins; the config file only fills defaults.
        if std::env::var_os(key).is_none() {
            std::env::set_var(key, value);
        }
    }
}

/// `--bw-read`/`--bw-write` env fallback: an explicit flag wins; an
/// unparsable env value is a hard error (silently ignoring a cap the
/// operator believes is on would be worse than refusing to mount).
fn resolve_bw(flag: Option<u64>, env_key: &str) -> Result<u64, Box<dyn std::error::Error>> {
    if let Some(v) = flag {
        return Ok(v);
    }
    match std::env::var(env_key) {
        Ok(s) if !s.trim().is_empty() => {
            grovefs::bwlimit::parse_rate(&s).map_err(|e| format!("{env_key}: {e}").into())
        }
        _ => Ok(0),
    }
}

fn base_dir(home: &Option<PathBuf>) -> PathBuf {
    if let Some(h) = home {
        return h.clone();
    }
    if let Ok(h) = std::env::var("GROVEFS_HOME") {
        return PathBuf::from(h);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".grovefs")
}

fn meta_path(base: &Path, view: &str) -> PathBuf {
    base.join("meta").join(format!("{view}.db"))
}

/// Open an EXISTING local view tree. A mount never creates one: an unknown
/// name is refused with the same "create it first" guidance the meta-server's
/// `M_OPEN` gives, so a typo cannot silently become an empty tree.
fn open_existing_view(base: &Path, view: &str) -> Result<MetaEngine, Box<dyn std::error::Error>> {
    let mp = meta_path(base, view);
    if !mp.is_file() {
        return Err(format!(
            "view '{view}' has no tree at {} — views are created explicitly; run `grovefs view --db-path {} create --view {view}` (or `grovefs --home {} init --view {view}`) first",
            mp.display(),
            base.display(),
            base.display()
        )
        .into());
    }
    Ok(MetaEngine::open(&mp)?)
}

/// Resolve the metadata backend URI for a CONTROL-plane command. The explicit
/// `--store` flag wins; otherwise the `GROVEFS_STORE` env fills the default
/// (same flag > env chain as `--token`/`--home`). `None` / a non-`postgres[ql]://`
/// value means the local SQLite/file control plane; a `postgres[ql]://` URI
/// routes api_keys and view definitions to the shared `grovefs_control` schema.
/// This is what makes the control plane FOLLOW the metadata backend — one
/// system of record, never a local file a PG deployment would silently ignore.
fn resolve_store(explicit: Option<String>) -> Option<String> {
    explicit.or_else(|| std::env::var("GROVEFS_STORE").ok())
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
    load_config_file();
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
    // authoritative data, no cache — so `--home` is not needed at all there
    // and leaves no footprint. The servers self-provision too: `meta-server`
    // creates `<base>/meta` in `serve()`, and `MetaStore`/the
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
            api_key,
            o,
            threads,
            as_uid,
            layout,
            no_readdirplus,
            tune_cache,
            bw_read,
            bw_write,
            stats: stats_on,
            stats_file,
            stats_interval,
            foreground,
            log_file,
        } => {
            // Cluster coordinates need not be retyped on every mount: each of
            // the repeat-heavy flags falls back to its env knob when unset —
            // the live env wins over the config file, an explicit flag wins
            // over both (see `load_config_file`). The detached re-exec keeps
            // these: the child inherits the resolved env below.
            let data = data.or_else(|| std::env::var("GROVEFS_DATA").ok());
            let meta = meta.or_else(|| std::env::var("GROVEFS_META").ok());
            let token = token.or_else(|| std::env::var("GROVEFS_TOKEN").ok());
            // Content rate caps: explicit flag > live env/config-file knob;
            // an unparsable env value refuses the mount (see `resolve_bw`).
            let bw_read = resolve_bw(bw_read, "GROVEFS_BW_READ")?;
            let bw_write = resolve_bw(bw_write, "GROVEFS_BW_WRITE")?;
            // Default `mount` detaches: re-exec this exact command with
            // `--foreground` in a new session (stdio → log file when attached
            // to a terminal, inherited otherwise so redirecting scripts keep
            // their sink), wait for the mountpoint to appear in /proc/mounts,
            // print a one-line summary, return. Everything below runs in the
            // child (or in the parent itself with `--foreground`).
            if !foreground {
                daemonize_and_await(&view, &mountpoint, log_file.as_deref())?;
                return Ok(());
            }
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
            // Effective uid reported to the server (lock arbitration, audit):
            // `--as-uid` overrides the process euid (advisory).
            let uid = as_uid.unwrap_or_else(|| unsafe { libc::geteuid() });
            // Per-caller credential for a `--require-api-key` server. The env
            // fallback keeps a secret out of argv (visible in `ps` and shell
            // history); an explicit flag always wins.
            let api_key = api_key
                .or_else(|| std::env::var("GROVEFS_API_KEY").ok())
                .unwrap_or_default();
            if !api_key.is_empty() && meta.is_none() && data.is_none() {
                // Say so rather than letting a caller believe a local mount just
                // authenticated: there is no server here to present a credential
                // to — isolation is the OS's (`--home`/`blocks/` permissions).
                eprintln!(
                    "note: --api-key is only presented to a remote --meta/--data server; a local mount is gated by OS permissions, so it is ignored here"
                );
            }
            // A DERIVED (virtual) view — server-defined shape or local
            // `meta/views/<view>.layout` — mounts read-only by contract: the
            // shape is centrally governed, writes belong to the original
            // (base) view. Set by whichever metadata source resolves below;
            // folded into the FUSE readonly flag AND the kernel RO mount.
            let mut view_ro = false;
            // A FORK mounts with STRUCTURE writable but CONTENT read-only: the
            // FUSE layer refuses write/truncate with EROFS while
            // mkdir/rename/unlink/symlink pass through. Set by whichever
            // metadata source resolves below (remote ack byte [2], local
            // `fork <base>` definition).
            let mut content_ro = false;
            // Metadata source: a central `meta-server` when --meta is given (a
            // single authoritative view shared across clients), else this machine's
            // local `<home>/meta/<view>.db`. Both satisfy `MetaProvider`, so the
            // FUSE layer below is identical either way.
            let meta_provider: Arc<dyn MetaProvider> = match &meta {
                Some(addr) => {
                    println!("using remote metadata service at {addr}");
                    let rm = remote::RemoteMeta::new_with_creds(
                        addr,
                        &view,
                        token.as_deref().unwrap_or(""),
                        uid,
                        &api_key,
                    )?;
                    // The M_OPEN ack flags derived names (byte [0]); a
                    // physical view stays writable.
                    view_ro = rm.is_readonly_view();
                    // Ack byte [2] flags a FORK: structure writable, content RO.
                    content_ro = rm.is_content_ro_view();
                    Arc::new(rm)
                }
                None => {
                    // Local mount: access control is the OS's (`--home`/
                    // `blocks/` file permissions) — there is no server-side
                    // credential gate to apply against a local sqlite tree.
                    // A mount never creates a tree: every branch below opens an
                    // EXISTING db through `open_existing_view`.
                    // PURE ALIAS resolution mirrors the meta-server's M_OPEN
                    // gate (see `load_view_aliases`): a governed real name is
                    // not a local mount entry point either, and the public
                    // alias name resolves to the real tree — full shape,
                    // writable.
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
                            "view '{view}' is governed by an alias — mount the alias name instead (`grovefs view list` shows it)"
                        )
                        .into());
                    }
                    if layout.is_none() {
                        if let Ok(rd) = std::fs::read_dir(base.join("meta").join("views")) {
                            for ent in rd.flatten() {
                                let p = ent.path();
                                if p.file_stem().and_then(|s| s.to_str()) == Some(view.as_str())
                                    && p.extension().and_then(|s| s.to_str()) == Some("layout")
                                {
                                    let d = grovefs::layout::LayoutRules::parse_def_file(&p)
                                        .map_err(|e| {
                                            format!("bad view definition '{view}': {e}")
                                        })?;
                                    local_def = Some(d);
                                    break;
                                }
                            }
                        }
                    }
                    // A local mount resolves the SAME definition a meta-server
                    // would (`<home>/meta/views/<name>.layout`), for the
                    // EFFECTIVE name after alias resolution: a lens wraps the
                    // base tree's engine, a fork opens its OWN tree, a physical
                    // name opens its own db. An explicit `--layout FILE` takes
                    // precedence (no lens stacking).
                    let def = base
                        .join("meta")
                        .join("views")
                        .join(format!("{effective}.layout"));
                    let effective_def: Option<grovefs::layout::ViewDef> = if layout.is_some() {
                        None
                    } else if effective == view {
                        local_def.clone()
                    } else if def.is_file() {
                        Some(
                            grovefs::layout::LayoutRules::parse_def_file(&def)
                                .map_err(|e| format!("bad view definition '{effective}': {e}"))?,
                        )
                    } else {
                        None
                    };
                    match effective_def {
                        Some(grovefs::layout::ViewDef::Derived(b, rules)) => {
                            println!(
                                "derived view '{view}' over base '{b}': {} hide, {} only, {} reloc rule(s)",
                                rules.hide.len(),
                                rules.only.len(),
                                rules.reloc.len()
                            );
                            // Same read-only contract the meta-server enforces
                            // for derived names; a local mount honors it without
                            // a wire.
                            view_ro = true;
                            println!(
                                "note: derived (virtual) views mount read-only — write to base view '{b}'"
                            );
                            let engine = open_existing_view(&base, &b)?;
                            Arc::new(grovefs::layout::LayoutMeta::new(Arc::new(engine), rules))
                        }
                        Some(grovefs::layout::ViewDef::Fork(b)) => {
                            println!(
                                "fork view '{effective}' of base '{b}': independent tree — structure writable, content read-only (EROFS)"
                            );
                            // The fork's tree is its OWN db: never fall back to
                            // creating a fresh empty one silently.
                            content_ro = true;
                            Arc::new(open_existing_view(&base, &effective)?)
                        }
                        Some(grovefs::layout::ViewDef::Alias(real)) => {
                            // Alias resolution already swapped `effective` to the
                            // real name; this is unreachable but stays honest.
                            Arc::new(open_existing_view(&base, &real)?)
                        }
                        None => Arc::new(open_existing_view(&base, &effective)?),
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
            let provider_for_events: Option<Arc<dyn MetaProvider>> = if meta.is_some() {
                Some(meta_provider.clone())
            } else {
                None
            };
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
                    let addrs: Vec<&str> = addr
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .collect();
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
                                remote::RemoteBackend::new_with_creds(
                                    a,
                                    token.as_deref().unwrap_or(""),
                                    &api_key,
                                )?
                                .with_fence(fence_epoch.clone(), fence_refresh.clone()),
                            ));
                        }
                        Arc::new(backend::ShardedBackend::with_default_stripe(members))
                    } else {
                        let one = addrs.first().copied().unwrap_or(addr.as_str());
                        println!("using remote data service at {one}");
                        Arc::new(
                            remote::RemoteBackend::new_with_creds(
                                one,
                                token.as_deref().unwrap_or(""),
                                &api_key,
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
                content_ro,
                strict || o.iter().any(|s| s == "strict"),
                !no_readdirplus,
                tune_cache,
                bw_read,
                bw_write,
            );

            // Build the fuser 0.18 session Config. The old `--o` string surface is
            // preserved; only `allow_root`/`allow_other` changed shape — they are no
            // longer MountOption variants but the `acl` field (session-wide request
            // filtering), with All == allow_other and RootAndOwner == allow_root.
            let mut cfg = fuser::Config::default();
            cfg.mount_options
                .push(fuser::MountOption::FSName(format!("grovefs:{view}")));
            for raw in &o {
                match raw.as_str() {
                    "allow_root" => cfg.acl = fuser::SessionACL::RootAndOwner,
                    "allow_other" => cfg.acl = fuser::SessionACL::All,
                    "default_permissions" => cfg
                        .mount_options
                        .push(fuser::MountOption::DefaultPermissions),
                    "auto_unmount" => cfg.mount_options.push(fuser::MountOption::AutoUnmount),
                    "ro" => cfg.mount_options.push(fuser::MountOption::RO),
                    other => cfg
                        .mount_options
                        .push(fuser::MountOption::CUSTOM(other.to_string())),
                }
            }
            // `--ro` (or `-o ro`) mounts the view read-only at the kernel level;
            // a derived (virtual) view is forced RO regardless of the flag.
            let want_ro = ro || view_ro || o.iter().any(|s| s == "ro");
            if want_ro && !o.iter().any(|s| s == "ro") {
                cfg.mount_options.push(fuser::MountOption::RO);
            }
            // Concurrent dispatch: n_threads worker event loops, each with its own
            // cloned /dev/fuse fd. `threads = 0` maps to single-threaded mode.
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
                let _listener = remote::spawn_invalidation_listener(
                    addr,
                    &view,
                    &tok,
                    &api_key,
                    uid,
                    notifier,
                    provider_for_events,
                );
            }
            spawn_term_unmounter(mountpoint.clone());
            // From here the mount is live, so a credential REFUSED mid-session
            // (an `apikey revoke` landing on this mount) is an auth loss and may
            // tear the mountpoint down instead of serving a broken view.
            remote::arm_mount_serving();
            session.run()?;
            println!("unmounted view '{view}' at {}", mountpoint.display());
        }
        Cmd::DataServer {
            listen,
            root,
            token,
            require_api_key,
            keys_from,
            store,
        } => {
            // Comma-separated roots = several disks behind one data-server;
            // placement across them is automatic (see PlacementBackend).
            let roots: Vec<PathBuf> = root
                .to_string_lossy()
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .collect();
            if roots.is_empty() {
                return Err("data-server --root: no usable path".into());
            }
            for r in &roots {
                std::fs::create_dir_all(r)?;
            }
            // Same flag > env > file chain as the mount side.
            let token = token.or_else(|| std::env::var("GROVEFS_TOKEN").ok());
            let backend = Arc::from(backend::open_local_roots(&roots)?);
            let srv = match token.as_deref() {
                Some(t) if !t.is_empty() => remote::DataServer::with_token(&listen, backend, t),
                _ => remote::DataServer::new(&listen, backend),
            };
            // Per-caller credential gate. The store FOLLOWS the metadata
            // backend: a `--store postgres://…` (or `GROVEFS_STORE`) data-server
            // gates mounts against the shared `grovefs_control` tables — the SAME
            // registry its meta-server uses — with no local file to colocate; a
            // SQLite node reads `<keys-from>/meta/api_keys.db` (defaulting to
            // this node's first `--root`, or a synced copy for a split one).
            let srv = if require_api_key {
                let store = resolve_store(store);
                let kbase = keys_from.unwrap_or_else(|| roots[0].clone());
                srv.with_api_keys(grovefs::apikeys::ApiKeyStore::open_for(
                    &kbase,
                    store.as_deref(),
                )?)
            } else {
                srv
            };
            // Durable fencing token: a witness restart must not zero the epoch
            // and let a deposed primary sneak back in as a writer.
            let srv = srv.with_fence_store(roots[0].join(".fence_token"));
            srv.serve()?;
        }
        Cmd::MetaServer {
            listen,
            db_path,
            token,
            require_api_key,
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
            // Backend for BOTH the view trees and the control plane (view
            // definitions + api_keys): `--store` > `GROVEFS_STORE` > SQLite.
            let store = resolve_store(store);
            // `--db-path` is the explicit, self-describing form of the metadata
            // root; the global `--home` is the default and loses if both are
            // given. A `GROVEFS_DB_PATH` env default slots between them,
            // and `GROVEFS_TOKEN` fills a missing `--token` (same flag > env >
            // config-file chain as the mount side).
            let token = token.or_else(|| std::env::var("GROVEFS_TOKEN").ok());
            let db_path =
                db_path.or_else(|| std::env::var("GROVEFS_DB_PATH").ok().map(PathBuf::from));
            let meta_base = db_path.unwrap_or_else(|| base.clone());
            let mut srv = match token.as_deref() {
                Some(t) if !t.is_empty() => {
                    grovefs::remote::MetaServer::with_token(&listen, &meta_base, t)
                }
                _ => grovefs::remote::MetaServer::new(&listen, &meta_base),
            };
            // Same registry `grovefs mcp --db-path` and `grovefs apikey --db-path`
            // use; key scopes decide which entrance each one may open. Under a PG
            // store the keys live in `grovefs_control.api_keys`, not a local file,
            // so a minted/revoked key is authoritative across every node at the URI.
            if require_api_key {
                srv = srv.with_api_keys(grovefs::apikeys::ApiKeyStore::open_for(
                    &meta_base,
                    store.as_deref(),
                )?);
            }
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
        Cmd::View {
            db_path,
            store,
            action,
        } => {
            // View definitions are CONTROL-PLACE state and FOLLOW the metadata
            // backend: under SQLite they are `meta/views/*.layout` files beside
            // `api_keys.db`; under a `--store postgres://…` they are the shared
            // `grovefs_control.view_defs` rows the meta-server reads at M_OPEN.
            // The governance ops below run against ONE `ControlStore` so the CLI
            // and the MCP server can never drift per backend. Base-view
            // creation and export are PHYSICAL-tree operations (a local db file)
            // and stay file-based.
            let vbase = db_path.unwrap_or_else(|| base.clone());
            let store = resolve_store(store);
            let store_is_pg = store
                .as_deref()
                .is_some_and(|u| u.starts_with("postgres://") || u.starts_with("postgresql://"));
            let ctrl = grovefs::mcp::control_store_for(&vbase, store.as_deref())?;
            // A PG deployment has no local per-view db file: a base tree is a
            // `grovefs_v_<hex(view)>` schema that `view create` provisions
            // explicitly, and its bytes move through a mount — so the file-level
            // physical ops below are refused rather than silently writing a
            // local artifact the PG meta-server never reads.
            let require_sqlite = |op: &str| -> Result<(), Box<dyn std::error::Error>> {
                if store_is_pg {
                    Err(format!(
                        "{op} operates on a local SQLite view db; under `--store postgres://…` a base tree is a PG schema (create one with `view create`) and its bytes move through a mounted path — drop --store for a local store"
                    )
                    .into())
                } else {
                    Ok(())
                }
            };
            match action {
                ViewAction::Create { view, manifest } => {
                    // Base (physical) view creation — the same authoritative
                    // tree `init` produces, framed as an explicit first-class
                    // step so the "only base views GC for real" rule has an
                    // obvious entry point. This is the ONLY way a tree comes
                    // into existence: a mount (local or M_OPEN) refuses an
                    // unknown name instead of materializing an empty one.
                    if !grovefs::layout::valid_view_name(&view) {
                        return Err(format!("invalid view name '{view}'").into());
                    }
                    if ctrl.has_def(&view)? {
                        return Err(format!(
                            "view '{view}' already has a derived/alias/fork definition — choose a different base name (`grovefs view list` shows it)"
                        )
                        .into());
                    }
                    if ctrl.has_physical(&view)? {
                        return Err(format!(
                            "view '{view}' already exists — remove it first (`grovefs view rm --name {view} --base`) or choose a different name"
                        )
                        .into());
                    }
                    if store_is_pg {
                        // A PG base view is one `grovefs_v_<hex(view)>` schema.
                        // `PgEngine::open` creates the schema, its three tables
                        // and the root row; dropping the handle leaves the tree
                        // in place for mounts to open.
                        if manifest.is_some() {
                            return Err("`view create --manifest` seeds a SQLite db file; under `--store postgres://…` mount the view and write the files instead".into());
                        }
                        let uri = store.as_deref().expect("store_is_pg implies a URI");
                        drop(grovefs::pg_store::PgEngine::open(uri, &view, false)?);
                        println!("created BASE view '{view}' in {uri} (schema grovefs_v_<hex>)");
                    } else {
                        let mp = meta_path(&vbase, &view);
                        if let Some(dir) = mp.parent() {
                            std::fs::create_dir_all(dir)?;
                        }
                        let meta = MetaEngine::open(&mp)?;
                        println!("created BASE view '{view}' at {}", mp.display());
                        if let Some(mf) = manifest {
                            let backend = backend::open_local(&vbase)?;
                            seed_manifest(&meta, backend.as_ref(), &mf)?;
                            println!("applied seed manifest {}", mf.display());
                        }
                    }
                    println!("  deletes in a base view free the underlying object once no other view references it;");
                    println!("  reshape it with `view define` or publish it under a name with `view alias`.");
                }
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
                    for l in grovefs::mcp::define_view(&*ctrl, &name, &src_base, &text)? {
                        match l {
                            grovefs::mcp::OutLine::Info(s) => println!("{s}"),
                            grovefs::mcp::OutLine::Note(s) => eprintln!("{s}"),
                        }
                    }
                }
                ViewAction::Fork { name, base, empty } => {
                    // Same shared implementation as the MCP `fork_view` tool
                    // (validation, tree seeding, ledger booking, definition
                    // slot); `--empty` only changes how many rows it starts with.
                    for l in grovefs::mcp::fork_view_seeded(&*ctrl, &name, &base, !empty)? {
                        match l {
                            grovefs::mcp::OutLine::Info(s) => println!("{s}"),
                            grovefs::mcp::OutLine::Note(s) => eprintln!("{s}"),
                        }
                    }
                }
                ViewAction::Graft {
                    name,
                    map,
                    src,
                    dest,
                    skip_existing,
                } => {
                    // Same shared implementation as the MCP `graft_view` tool.
                    // The plan is text either way: a file serves an editor, `-`
                    // serves a pipe — one parser, no second code path.
                    let text = if map.to_str() == Some("-") {
                        std::io::read_to_string(std::io::stdin())
                            .map_err(|e| format!("cannot read plan from stdin: {e}"))?
                    } else {
                        std::fs::read_to_string(&map)
                            .map_err(|e| format!("cannot read plan file {}: {e}", map.display()))?
                    };
                    for l in grovefs::mcp::graft_view(
                        &*ctrl,
                        &name,
                        src.as_deref(),
                        &text,
                        dest.as_deref().unwrap_or(""),
                        skip_existing,
                    )? {
                        match l {
                            grovefs::mcp::OutLine::Info(s) => println!("{s}"),
                            grovefs::mcp::OutLine::Note(s) => eprintln!("{s}"),
                        }
                    }
                }
                ViewAction::List => {
                    for l in grovefs::mcp::list_views_text(&*ctrl)? {
                        match l {
                            grovefs::mcp::OutLine::Info(s) => println!("{s}"),
                            grovefs::mcp::OutLine::Note(s) => eprintln!("{s}"),
                        }
                    }
                    // Inventory hint: keys are per-view and unlimited, so the
                    // "who can open what" question is one filtered listing away.
                    println!(
                        "view keys: `grovefs apikey --db-path {} list [--view NAME]` (add --view to see only that view's keys)",
                        vbase.display()
                    );
                }
                ViewAction::Alias { name, real, retire } => {
                    // Same shared implementation as the MCP `alias_view` tool
                    // (validation, gapless --retire rotation, notes).
                    for l in grovefs::mcp::alias_view(&*ctrl, &name, &real, retire.as_deref())? {
                        match l {
                            grovefs::mcp::OutLine::Info(s) => println!("{s}"),
                            grovefs::mcp::OutLine::Note(s) => eprintln!("{s}"),
                        }
                    }
                }
                ViewAction::Rm {
                    name,
                    base: remove_base,
                    data_root,
                } => {
                    if !remove_base {
                        // Definition slot removal (derived/alias/fork) — the
                        // shared implementation behind the MCP `remove_view`
                        // tool.
                        for l in grovefs::mcp::remove_view(&*ctrl, &name)? {
                            match l {
                                grovefs::mcp::OutLine::Info(s) => println!("{s}"),
                                grovefs::mcp::OutLine::Note(s) => eprintln!("{s}"),
                            }
                        }
                        return Ok(());
                    }
                    // Whole BASE view removal: guards first, then tree + ledger,
                    // then byte reclaim through the store's data root.
                    require_sqlite("`view rm --base`")?;
                    if ctrl.has_def(&name)? {
                        return Err(format!(
                            "'{name}' is governed by a view definition — remove it first with `grovefs view rm --name {name}` (without --base)"
                        )
                        .into());
                    }
                    // A base others' definitions still resolve through must not
                    // vanish from under them: an alias/lens/fork referencing it
                    // would dangle (mounting it afterwards is refused, not
                    // auto-created).
                    for (dname, text) in ctrl.list_defs()? {
                        let dep = match grovefs::layout::LayoutRules::parse_def(&text) {
                            Ok(grovefs::layout::ViewDef::Derived(b, _)) => b == name,
                            Ok(grovefs::layout::ViewDef::Fork(b)) => b == name,
                            Ok(grovefs::layout::ViewDef::Alias(r)) => r == name,
                            Err(_) => false,
                        };
                        if dep {
                            return Err(format!(
                                "view '{dname}' is governed by a definition over base '{name}' — remove it first (`grovefs view rm --name {dname}`) before `view rm --name {name} --base`"
                            )
                            .into());
                        }
                    }
                    if let Some(mp) = view_mounted_at(&name) {
                        return Err(format!(
                            "view '{name}' is mounted here at {mp} — unmount it first (`grovefs umount -m {mp}`); mounts on OTHER machines are not visible in this mount table, stop them too"
                        )
                        .into());
                    }
                    if !grovefs::layout::valid_view_name(&name) {
                        return Err(format!("invalid view name '{name}'").into());
                    }
                    let rm = meta::remove_base_view(&vbase, &name)?;
                    let droot = data_root.unwrap_or_else(|| vbase.clone());
                    let backend = backend::open_local(&droot)?;
                    let (mut freed, mut stayed, mut missing, mut failed) =
                        (0usize, 0usize, 0usize, 0usize);
                    for dr in &rm.blob_refs {
                        if meta::ledger_holds(&vbase, dr).unwrap_or(true) {
                            stayed += 1; // another view still names it
                            continue;
                        }
                        // Striped keys (s<N>:<stripe>:<uuid>) live as chunks on
                        // the data-servers a ShardedBackend knows — a plain local
                        // root cannot free them; report honestly, never half-delete.
                        if dr.starts_with("s") && dr.matches(':').count() >= 2 {
                            failed += 1;
                            eprintln!(
                                "note: striped object {dr} not freed from a plain local root — reclaim it through a mount or the data-servers"
                            );
                            continue;
                        }
                        // `remove` is idempotent (Ok for a never-existing key),
                        // so probe existence via `len` FIRST: a wrong
                        // --data-root must report "not found here", never a
                        // phantom free.
                        if backend.len(dr).is_err() {
                            missing += 1;
                            continue;
                        }
                        match backend.remove(dr) {
                            Ok(()) => freed += 1,
                            Err(e) => {
                                failed += 1;
                                eprintln!("warning: cannot free blob {dr}: {e}");
                            }
                        }
                    }
                    println!(
                        "removed base view '{name}': {} node(s) gone, {} blob reference(s) dropped, {} object(s) freed from {}, {} still held by other views{}{}",
                        rm.nodes,
                        rm.blob_refs.len(),
                        freed,
                        droot.join("blocks").display(),
                        stayed,
                        if missing > 0 {
                            format!(", {missing} not found under that data root (wrong --data-root? they live on another store)")
                        } else {
                            String::new()
                        },
                        if failed > 0 {
                            format!(", {failed} could not be freed (see warnings above)")
                        } else {
                            String::new()
                        }
                    );
                    if missing > 0 {
                        eprintln!(
                            "note: {missing} unreferenced blob(s) were NOT deleted — re-run with --data-root pointing at the store that holds them (default: this metadata root)"
                        );
                    }
                    eprintln!(
                        "note: new mounts of '{name}' are now refused — run `init --view {name}` / `view create` to start over; live mounts of the removed name on other machines keep their resolved handles until they remount"
                    );
                }
                ViewAction::Export { view, out, force } => {
                    if !grovefs::layout::valid_view_name(&view) {
                        return Err(format!("invalid view name '{view}'").into());
                    }
                    require_sqlite("`view export`")?;
                    // A virtual name has no tree: exporting its definition file
                    // would silently produce an empty/nonexistent export, so
                    // point at the physical base instead.
                    let slot = vbase
                        .join("meta")
                        .join("views")
                        .join(format!("{view}.layout"));
                    if slot.is_file() {
                        match grovefs::layout::LayoutRules::parse_def_file(&slot)
                            .map_err(|e| format!("bad view definition '{view}': {e}"))?
                        {
                            grovefs::layout::ViewDef::Alias(real) => {
                                return Err(format!(
                                    "'{view}' is a pure alias of real view '{real}' — export '{real}' instead (an alias has no metadata tree of its own)"
                                )
                                .into())
                            }
                            grovefs::layout::ViewDef::Derived(base, _) => {
                                return Err(format!(
                                    "'{view}' is a derived (virtual) view over '{base}' — export '{base}' instead (a derived view has no metadata tree of its own)"
                                )
                                .into())
                            }
                            grovefs::layout::ViewDef::Fork(_) => {}
                        }
                    }
                    let st = meta::export_view(&vbase, &view, &out, force)?;
                    println!(
                        "exported view '{view}' metadata: {} node(s), {} blob reference(s) -> {}",
                        st.nodes,
                        st.blobs,
                        out.display()
                    );
                    if st.kind == "fork" {
                        println!("note: this is a fork tree; `view import` creates a normal writable base tree unless you pass --as-fork <base>");
                    }
                    println!("note: metadata only — copy the store's `blocks/` tree (the whole data root) as well; blobs are located by their data_ref key, not by any host path");
                }
                ViewAction::Import {
                    view,
                    input,
                    replace,
                    as_fork,
                } => {
                    if !grovefs::layout::valid_view_name(&view) {
                        return Err(format!("invalid view name '{view}'").into());
                    }
                    require_sqlite("`view import`")?;
                    let meta_dir = vbase.join("meta");
                    let dir = meta_dir.join("views");
                    if dir.join(format!("{view}.layout")).is_file() {
                        return Err(format!(
                            "view '{view}' already has a definition — remove it first (`grovefs view rm --name {view}`) before importing"
                        )
                        .into());
                    }
                    if let Some(base) = &as_fork {
                        if !grovefs::layout::valid_view_name(base) {
                            return Err(format!("invalid base view name '{base}'").into());
                        }
                        if base == &view {
                            return Err("fork name must differ from its base".into());
                        }
                        if dir.join(format!("{base}.layout")).is_file() {
                            return Err(format!(
                                "base '{base}' is governed by a definition (alias/derived/fork) — fork the physical base tree instead"
                            )
                            .into());
                        }
                        if !meta_dir.join(format!("{base}.db")).is_file() {
                            return Err(format!(
                                "no physical base tree {}/meta/{base}.db — import the base first, or omit --as-fork",
                                vbase.display()
                            )
                            .into());
                        }
                    }
                    let st = meta::import_view(&vbase, &view, &input, replace)?;
                    println!(
                        "imported view '{view}' from {}: {} node(s), {} blob reference(s){}; cross-view ledger rebuilt",
                        input.display(),
                        st.nodes,
                        st.blobs,
                        if st.replaced {
                            ", replacing the previous tree"
                        } else {
                            ""
                        }
                    );
                    if let Some(base) = as_fork {
                        std::fs::create_dir_all(&dir)?;
                        if let Err(e) = std::fs::write(
                            dir.join(format!("{view}.layout")),
                            format!("fork {base}\n"),
                        ) {
                            for ext in ["", "-wal", "-shm"] {
                                let _ =
                                    std::fs::remove_file(meta_dir.join(format!("{view}.db{ext}")));
                            }
                            let _ = meta::rebuild_ledger(&vbase);
                            return Err(
                                format!("cannot record fork definition for '{view}': {e}").into()
                            );
                        }
                        println!("note: recorded `fork {base}` — mounts of '{view}' are structure-writable, content read-only (EROFS)");
                    } else if st.kind == "fork" {
                        println!("note: the source was a fork tree; imported as a normal writable base view (use --as-fork <base> when you need fork semantics)");
                    }
                    println!("note: content bytes are not in the export — make sure `blocks/` from the source store is present under the target data root before mounting");
                }
            }
        }
        Cmd::Mcp {
            db_path,
            require_api_key,
            store,
        } => {
            // The agent entrance onto the same governance API: an MCP client
            // spawns this over stdio; EOF on stdin shuts it down. Control
            // plane only — no mount, no data IO. `--require-api-key` gates
            // tools/list + tools/call behind a `grovefs apikey` credential. The
            // control plane FOLLOWS the metadata backend: `--store postgres://…`
            // (or `GROVEFS_STORE`) binds definitions, keys and the call audit to
            // the shared `grovefs_control` schema; unset governs the local files.
            let store = resolve_store(store);
            grovefs::mcp::serve(
                db_path.unwrap_or_else(|| base.clone()),
                store.as_deref(),
                require_api_key,
            )?;
        }
        Cmd::Apikey {
            db_path,
            store,
            action,
        } => {
            // Keys + audit log are CONTROL-PLACE state and FOLLOW the metadata
            // backend: a local `api_keys.db` under SQLite, the shared
            // `grovefs_control.api_keys`/`mcp_calls` tables under a
            // `--store postgres://…` — so one mint is authoritative for every
            // server pointing at the URI, with no per-node file to colocate.
            let ak_base = db_path.unwrap_or_else(|| base.clone());
            let store = resolve_store(store);
            let keys = grovefs::apikeys::ApiKeyStore::open_for(&ak_base, store.as_deref())?;
            match action {
                ApikeyAction::Create {
                    label,
                    r#for,
                    view,
                    ro,
                } => {
                    let scope = grovefs::apikeys::Scope::parse(&r#for)
                        .map_err(Box::<dyn std::error::Error>::from)?;
                    let (id, full) = keys.create_bound(&label, scope, view.as_deref(), ro)?;
                    println!(
                        "apikey created — shown ONCE (only its SHA-256 is stored; save it now):"
                    );
                    println!("  id:    {id}");
                    println!("  label: {label}");
                    println!("  scope: {}", scope.as_str());
                    println!("  view:  {}", view.as_deref().unwrap_or("<any>"));
                    println!(
                        "  rw:    {}",
                        if ro {
                            "ro (read-only)"
                        } else {
                            "rw (read-write)"
                        }
                    );
                    println!("  key:   {full}");
                    println!();
                    match scope {
                        grovefs::apikeys::Scope::Mount | grovefs::apikeys::Scope::Any => {
                            println!("Present it to a `--require-api-key` server as");
                            println!("  grovefs mount --meta ... --data ... --api-key '{full}' -m /mnt/x");
                            println!("(or export GROVEFS_API_KEY='{full}' so it stays out of the command line).");
                        }
                        grovefs::apikeys::Scope::Mcp => {
                            println!("Present it as the `apiKey` param in the MCP `initialize` handshake, or");
                            println!("export GROVEFS_MCP_API_KEY='{full}' when launching");
                            println!(
                                "`grovefs mcp --db-path {} --require-api-key`.",
                                ak_base.display()
                            );
                        }
                    }
                    if scope == grovefs::apikeys::Scope::Any {
                        println!("Scope it to one entrance with `--for mount` / `--for mcp` to keep a mount");
                        println!("credential out of the governance API (and vice versa).");
                    }
                    println!(
                        "Revoke later with: grovefs apikey --db-path {} revoke --id {id}",
                        ak_base.display()
                    );
                }
                ApikeyAction::List { view } => {
                    let all = keys.list()?;
                    // Resolve an alias name to its real view so keys bound to
                    // the real name also show under `--view <alias>` — read from
                    // the SAME control plane as the keys (PG rows or local files),
                    // never a path the chosen backend would ignore.
                    let real = match view.as_deref() {
                        None => None,
                        Some(v) => {
                            let cs = grovefs::mcp::control_store_for(&ak_base, store.as_deref())
                                .map_err(Box::<dyn std::error::Error>::from)?;
                            cs.load_alias_map()
                                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?
                                .get(v)
                                .cloned()
                        }
                    };
                    let rows: Vec<_> = all
                        .iter()
                        .filter(|r| match (&view, &real) {
                            (None, _) => true,
                            (Some(v), real) => {
                                r.view.as_deref() == Some(v.as_str())
                                    || real
                                        .as_deref()
                                        .is_some_and(|x| r.view.as_deref() == Some(x))
                            }
                        })
                        .collect();
                    if all.is_empty() && view.is_none() {
                        println!(
                            "no api keys yet (under {}); mint one with `grovefs apikey create --label L`",
                            ak_base.display()
                        );
                        return Ok(());
                    }
                    println!(
                        "{:<3} {:<34} {:<16} {:<7} {:<12} {:<4} {:<25} {:<25} revoked",
                        "#", "id", "label", "scope", "view", "rw", "created", "last_used"
                    );
                    for (i, r) in rows.iter().enumerate() {
                        println!(
                            "{:<3} {:<34} {:<16} {:<7} {:<12} {:<4} {:<25} {:<25} {}",
                            i + 1,
                            r.id,
                            r.label,
                            r.scope,
                            r.view.as_deref().unwrap_or("<any>"),
                            if r.ro { "ro" } else { "rw" },
                            r.created_at,
                            r.last_used_at.as_deref().unwrap_or("-"),
                            r.revoked_at.as_deref().unwrap_or("-"),
                        );
                    }
                    if let Some(v) = &view {
                        println!(
                            "{} bound key(s) for view '{}' of {} total",
                            rows.len(),
                            v,
                            all.len()
                        );
                        if let Some(real) = &real {
                            println!(
                                "  ('{}' is a pure alias — keys bound to its real view '{}' are included)",
                                v, real
                            );
                        }
                        let unbound = all.iter().filter(|r| r.view.is_none()).count();
                        if unbound > 0 {
                            println!(
                                "  note: {unbound} unbound key(s) admit every view too — plain `apikey list` shows them"
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
                        all.iter()
                            .filter(|k| k.id.starts_with(id.as_str()))
                            .collect()
                    };
                    match resolved.len() {
                        0 => {
                            return Err(format!(
                                "no api key with id '{id}' (see `grovefs apikey list`)"
                            )
                            .into())
                        }
                        1 => {
                            let k = resolved[0];
                            if k.revoked_at.is_some() {
                                println!("apikey: id='{}' (label '{}') is already revoked", k.id, k.label);
                            } else if keys.revoke(&k.id)? {
                                println!("apikey: revoked id='{}' (label '{}'); it can no longer authenticate, past calls keep attribution", k.id, k.label);
                                println!("  a LIVE mount presenting this key is refused on its next op (within the 1s re-check window) and unmounts itself;");
                                println!("  a `grovefs mcp` session in flight keeps its identity until it reconnects.");
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
                ApikeyAction::SetRo { id, rw, .. } => {
                    // Default (neither flag) and `--ro` set read-only; `--rw`
                    // sets read-write. `ro` conflicts with `rw` at parse time.
                    let ro_now = !rw;
                    // Same id resolution as `revoke`: exact, else a UNIQUE prefix.
                    let all = keys.list()?;
                    let resolved: Vec<_> = if let Some(e) = all.iter().find(|k| k.id == id) {
                        vec![e]
                    } else {
                        all.iter()
                            .filter(|k| k.id.starts_with(id.as_str()))
                            .collect()
                    };
                    match resolved.len() {
                        0 => {
                            return Err(format!(
                                "no api key with id '{id}' (see `grovefs apikey list`)"
                            )
                            .into())
                        }
                        1 => {
                            let k = resolved[0];
                            if k.revoked_at.is_some() {
                                return Err(format!(
                                    "apikey id='{}' is revoked — set-ro never resurrects a revoked key (mint a new one)",
                                    k.id
                                )
                                .into());
                            }
                            if keys.set_ro(&k.id, ro_now)? {
                                if ro_now {
                                    println!("apikey: id='{}' (label '{}') is now ro (read-only)", k.id, k.label);
                                    println!("  a LIVE mount presenting this key starts refusing writes on its next re-check window (≤1s) — no reconnect, no remount; its reads keep flowing.");
                                } else {
                                    println!("apikey: id='{}' (label '{}') is now rw (read-write)", k.id, k.label);
                                    println!("  a LIVE mount presenting this key resumes admitting writes on its next re-check window (≤1s) — no reconnect, no remount.");
                                }
                            } else {
                                return Err(
                                    format!("apikey id='{}' became revoked concurrently", k.id).into()
                                );
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
                        println!("no logged calls yet (a `grovefs mcp` run appends one row per tools/call, and a `--require-api-key` meta/data-server one per mount connect, here)");
                    } else {
                        println!("last {} call(s), newest first:", rows.len());
                        println!(
                            "{:<25} {:<34} {:<12} {:<22} {:<4} detail",
                            "ts", "key_id", "method", "tool", "ok"
                        );
                        for r in &rows {
                            println!(
                                "{:<25} {:<34} {:<12} {:<22} {:<4} {}",
                                r.ts,
                                r.key_id.as_deref().unwrap_or("-"),
                                r.method,
                                if r.tool.is_empty() {
                                    "-"
                                } else {
                                    r.tool.as_str()
                                },
                                if r.ok { "yes" } else { "no" },
                                r.detail,
                            );
                        }
                    }
                }
            }
        }
        Cmd::Refs { db_path, action } => {
            let rbase = db_path.unwrap_or_else(|| base.clone());
            match action {
                RefsAction::Rebuild => {
                    let counted = grovefs::meta::rebuild_ledger(&rbase)?;
                    println!(
                        "refs ledger rebuilt at {}: {} holder entries across the surviving views",
                        rbase.join("meta").join("refs.db").display(),
                        counted
                    );
                }
            }
        }
        Cmd::Gc {
            db_path,
            data_root,
            older_than,
            apply,
            json,
            force,
        } => {
            // The trees this command audits are local `<view>.db` files. Under
            // PG they are schemas and the bytes belong to the data servers —
            // there is no offline page tree to walk here.
            if resolve_store(None)
                .as_deref()
                .is_some_and(|u| u.starts_with("postgres://") || u.starts_with("postgresql://"))
            {
                return Err("`gc` audits LOCAL page files under <data-root>/blocks; a PostgreSQL deployment's trees are schemas and its bytes are owned by the data servers — reclaim there with the DBA/data-server tooling, not this command".into());
            }
            let mbase = db_path.unwrap_or_else(|| base.clone());
            let root_arg = data_root.unwrap_or_else(|| mbase.clone());
            let roots: Vec<PathBuf> = root_arg
                .to_string_lossy()
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .collect();
            if roots.is_empty() {
                return Err("gc: no data root to scan (pass --data-root)".into());
            }
            let age = grovefs::gc::parse_age(&older_than)?;
            let views = grovefs::gc::physical_views(&mbase)?;
            // Offline gate: a live writable mount of any tree here (or an alias
            // that writes through to one) can commit a node row between our
            // scan and delete. A derived lens is read-only by contract and is
            // not a blocker.
            if apply {
                let live = gc_live_mounts(&mbase, &roots, &views);
                if !live.is_empty() {
                    return Err(format!(
                        "gc --apply refused: this store has live mounts — {}; unmount them first (reclaim must run offline)",
                        live.join("; ")
                    )
                    .into());
                }
            }
            let users = local_store_users(&mbase, &roots);
            if !users.is_empty() {
                let list = users.join("; ");
                if apply {
                    return Err(format!(
                        "gc --apply refused: this store is in use by local grovefs process(es) — {list}; stop them first (a hard-linked import can expose an OLD mtime before its node row commits, so the grace window alone cannot protect it)"
                    )
                    .into());
                }
                eprintln!("note: this store is in use by local grovefs process(es) — {list}");
            }
            let rep = grovefs::gc::scan(&mbase, &roots, age)?;
            // Wrong-root guard: a data root that shares ZERO objects with the
            // surviving trees is almost always a --db-path/--data-root mixup,
            // and deleting there would kill another store's bytes.
            if rep.objects > 0 && rep.refs > 0 && rep.matched == 0 && !force {
                return Err(format!(
                    "gc: the data root(s) share no object with any view tree in {} ({} object(s), {} referenced key(s)) — wrong --db-path/--data-root? Nothing was deleted; pass --force only if this is expected",
                    mbase.display(), rep.objects, rep.refs
                )
                .into());
            }
            let reclaimed = if apply {
                Some(grovefs::gc::reclaim(&rep.orphans, age))
            } else {
                None
            };
            if json {
                println!("{}", gc_json(&rep, reclaimed.as_ref()));
            } else {
                print_gc_report(&rep, reclaimed.as_ref());
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
                let slot = mbase
                    .join("meta")
                    .join("views")
                    .join(format!("{view}.layout"));
                let target = grovefs::layout::LayoutRules::parse_def_file(&slot)
                    .map(|d| match d {
                        grovefs::layout::ViewDef::Alias(real) => format!(
                            "'{view}' is a pure alias of real view '{real}' — import into '{real}' directly (an alias is not a storage identity)"
                        ),
                        grovefs::layout::ViewDef::Derived(..) => format!(
                            "import target '{view}' is a derived (virtual) view — import into its physical base view instead (derived views mount read-only)"
                        ),
                        grovefs::layout::ViewDef::Fork(base) => format!(
                            "import target '{view}' is a fork of base '{base}' — a fork refuses content writes (EROFS); import into the base view '{base}' instead"
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
            // Same multi-root contract as `data-server --root`: a
            // comma-separated --data-root registers imported objects onto the
            // disk their key hashes to, colocated with the running server.
            let roots: Vec<PathBuf> = droot
                .to_string_lossy()
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .collect();
            if roots.is_empty() {
                return Err("import --data-root: no usable path".into());
            }
            let backend = backend::open_local_roots(&roots)?;
            let mut dirs: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
            dirs.insert("/".to_string(), meta::ROOT_INO);
            ensure_dir_soft(&meta, &prefix, &mut dirs)?;
            let mut st = ImportStats::default();
            import_walk(
                &meta,
                backend.as_ref(),
                &source,
                &prefix,
                &mut dirs,
                &mut st,
            )?;
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
        Cmd::Du {
            view,
            meta,
            meta_token,
            meta_api_key,
            store,
            db_path,
            data,
            data_root,
            api_key,
            token,
            path,
            depth,
            bytes,
            total,
        } => {
            let opts = grovefs::du::DuOpts {
                view,
                meta: meta.or_else(|| std::env::var("GROVEFS_META").ok().filter(|s| !s.is_empty())),
                meta_token: meta_token.or_else(|| {
                    std::env::var("GROVEFS_TOKEN")
                        .ok()
                        .filter(|s| !s.is_empty())
                }),
                meta_api_key: meta_api_key.or_else(|| {
                    std::env::var("GROVEFS_API_KEY")
                        .ok()
                        .filter(|s| !s.is_empty())
                }),
                store: resolve_store(store),
                db_path: db_path.or_else(|| Some(base.clone())),
                data: data.or_else(|| std::env::var("GROVEFS_DATA").ok().filter(|s| !s.is_empty())),
                data_root,
                api_key: api_key.or_else(|| {
                    std::env::var("GROVEFS_API_KEY")
                        .ok()
                        .filter(|s| !s.is_empty())
                }),
                token: token.or_else(|| {
                    std::env::var("GROVEFS_TOKEN")
                        .ok()
                        .filter(|s| !s.is_empty())
                }),
                path,
                depth: if total { 0 } else { depth },
                bytes,
            };
            grovefs::du::run(&opts).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// Tallies for `grovefs import`.
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
/// what makes a re-run of `grovefs import` an incremental no-op over the
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
            None => meta.create_node(
                cur_ino,
                comp,
                true,
                None,
                0o0755,
                unsafe { libc::getuid() },
                unsafe { libc::getgid() },
            )?,
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
                eprintln!(
                    "import: cannot read an entry under {}: {err}",
                    src.display()
                );
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
            let fm = match std::fs::metadata(e.path()) {
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
                meta.create_node(
                    parent_ino,
                    &name,
                    false,
                    Some(&key),
                    0o0644,
                    unsafe { libc::getuid() },
                    unsafe { libc::getgid() },
                )?;
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
        let ino = meta.create_node(
            cur_ino,
            comp,
            true,
            None,
            0o0755,
            unsafe { libc::getuid() },
            unsafe { libc::getgid() },
        )?;
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

// ---- Detached (background) mounts ------------------------------------------
//
// `mount` without `--foreground` re-execs this exact command line with
// `--foreground` appended, in a brand-new session (setsid) so the mount
// survives its shell/terminal. Readiness is confirmed by polling
// /proc/mounts: the parent returns only once the FUSE mount is actually
// live (or with the child's failure), which makes scripts deterministic —
// no sleep loops. Stdio: if the parent is attached to a terminal the
// child's output goes to a log file (`--log-file`, default
// `$TMPDIR/grovefs-mount-<mountpoint>.log` — deliberately NOT under
// `--home`, a detached mount still writes nothing there); if the parent's
// stdio is already redirected (a script's `> x.log 2>&1`), the child
// inherits those descriptors so existing sinks and greps keep working.
fn daemonize_and_await(
    view: &str,
    mountpoint: &Path,
    log_file: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::IsTerminal;
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;

    let attach_tty = std::io::stdout().is_terminal() || std::io::stderr().is_terminal();
    let log_path = match log_file {
        Some(p) => Some(p.to_path_buf()),
        None if attach_tty => {
            let name = mountpoint
                .to_string_lossy()
                .replace(['/', ':'], "-")
                .trim_matches('-')
                .to_string();
            Some(std::env::temp_dir().join(format!(
                "grovefs-mount-{}.log",
                if name.is_empty() { "root" } else { &name }
            )))
        }
        None => None,
    };

    // Candidate strings /proc/mounts may show for this mountpoint (raw, cwd-
    // joined, kernel-canonical). The dir is created here ONLY so canonicalize
    // can resolve it pre-mount; the child re-creates it idempotently.
    let mut wants = vec![mountpoint.to_string_lossy().to_string()];
    if !mountpoint.is_absolute() {
        if let Ok(cwd) = std::env::current_dir() {
            wants.push(cwd.join(mountpoint).to_string_lossy().into_owned());
        }
    }
    let _ = std::fs::create_dir_all(mountpoint);
    if let Ok(c) = mountpoint.canonicalize() {
        let s = c.to_string_lossy().into_owned();
        if !wants.contains(&s) {
            wants.push(s);
        }
    }

    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.args(std::env::args_os().skip(1)).arg("--foreground");
    match &log_path {
        Some(p) => {
            if let Some(dir) = p.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)?;
            cmd.stdin(Stdio::null())
                .stdout(Stdio::from(f.try_clone()?))
                .stderr(Stdio::from(f));
        }
        None => {
            cmd.stdin(Stdio::null())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit());
        }
    }
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("spawn background mount: {e}"))?;
    let pid = child.id();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if mounted_at(&wants) {
            match &log_path {
                Some(p) => println!(
                    "grovefs: mounted view '{view}' at {} (background pid {pid}, log {})",
                    mountpoint.display(),
                    p.display()
                ),
                None => println!(
                    "grovefs: mounted view '{view}' at {} (background pid {pid})",
                    mountpoint.display()
                ),
            }
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            // Surface the tail of the child's own error output when we have a
            // log file; with inherited stdio it already went to the terminal.
            if let Some(p) = &log_path {
                if let Ok(data) = std::fs::read(p) {
                    let start = data.len().saturating_sub(2000);
                    eprint!("{}", String::from_utf8_lossy(&data[start..]));
                }
            }
            return Err(format!(
                "grovefs: background mount exited ({status}) before mounting{}",
                log_path
                    .as_ref()
                    .map(|p| format!("; full log {}", p.display()))
                    .unwrap_or_default()
            )
            .into());
        }
        if std::time::Instant::now() > deadline {
            return Err(format!(
                "grovefs: mount not visible at {} within 30s (background pid {pid} still running)",
                mountpoint.display()
            )
            .into());
        }
        std::thread::sleep(std::time::Duration::from_millis(80));
    }
}

/// True when /proc/mounts carries an entry at any of the given mountpoint
/// spellings (octal escapes \040 \011 \012 \134 decoded, per proc(5)).
fn mounted_at(wants: &[String]) -> bool {
    let Ok(content) = std::fs::read_to_string("/proc/mounts") else {
        return false;
    };
    content.lines().any(|l| {
        let mut it = l.split_whitespace();
        let _dev = it.next();
        match it.next() {
            Some(mp) => {
                let mp = mp
                    .replace("\\040", " ")
                    .replace("\\011", "\t")
                    .replace("\\012", "\n")
                    .replace("\\134", "\\");
                wants.contains(&mp)
            }
            None => false,
        }
    })
}

/// The first mountpoint on THIS machine whose FUSE source is `grovefs:<view>`
/// (the FSName every mount sets), if any. Used by `view rm --base` to refuse
/// deleting a tree out from under a live local mount. Mounts served by a remote
/// meta-server on ANOTHER machine are not in this table — documented, not hidden.
fn view_mounted_at(view: &str) -> Option<String> {
    let Ok(content) = std::fs::read_to_string("/proc/mounts") else {
        return None;
    };
    let tag = format!("grovefs:{view}");
    for l in content.lines() {
        let mut it = l.split_whitespace();
        let (Some(dev), Some(mp)) = (it.next(), it.next()) else {
            continue;
        };
        if dev == tag {
            return Some(
                mp.replace("\\040", " ")
                    .replace("\\011", "\t")
                    .replace("\\012", "\n")
                    .replace("\\134", "\\"),
            );
        }
    }
    None
}

/// Live mounts of THIS store that can write: every physical tree name (base or
/// fork) plus pure ALIASES resolving to one. A derived lens is read-only by
/// contract, so it cannot commit a node row and is not a gc blocker.
/// Mounts of another store's names are ignored (their db is not under `mbase`).
fn gc_live_mounts(mbase: &Path, roots: &[PathBuf], views: &[String]) -> Vec<String> {
    let Ok(content) = std::fs::read_to_string("/proc/mounts") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for l in content.lines() {
        let mut it = l.split_whitespace();
        let (Some(dev), Some(mp)) = (it.next(), it.next()) else {
            continue;
        };
        let Some(name) = dev.strip_prefix("grovefs:") else {
            continue;
        };
        let Some(reason) = gc_writer_reason(name, mbase, views) else {
            continue;
        };
        let mp = mp
            .replace("\\040", " ")
            .replace("\\011", "\t")
            .replace("\\012", "\n")
            .replace("\\134", "\\");
        let mpp = Path::new(&mp);
        // The same view NAME can mount in unrelated stores, so attribute the
        // entry to the process that serves it before calling it a writer of
        // THIS store. A daemon that names another store — or is a remote
        // `--meta`/`--data` client — never touches these pages.
        match gc_mount_daemon(mpp) {
            Some(pid) if !proc_store_hit(pid, mbase, roots) => continue,
            // No daemon visible: a stale FUSE entry whose mountpoint no longer
            // answers is dead. Anything else stays a conservative block.
            None if !mount_answers(mpp) => continue,
            _ => {}
        }
        out.push(format!("'{name}' at {mp} ({reason})"));
    }
    out.sort();
    out.dedup();
    out
}

/// The `grovefs mount` daemon serving `mp`, matched by its `-m/--mountpoint`
/// value (absolute, or relative to the daemon's cwd). `None` means the FUSE
/// entry outlived its daemon (or the daemon is not visible in /proc).
fn gc_mount_daemon(mp: &Path) -> Option<u32> {
    let want = std::fs::canonicalize(mp).unwrap_or_else(|_| mp.to_path_buf());
    for pid in proc_pids() {
        let Some(args) = proc_args(pid) else {
            continue;
        };
        if !args.iter().any(|a| a.contains("grovefs")) || !args.iter().any(|a| a == "mount") {
            continue;
        }
        let cwd = std::fs::read_link(format!("/proc/{pid}/cwd")).ok();
        let mut i = 0;
        while i < args.len() {
            let a = &args[i];
            let target = if a == "-m" || a == "--mountpoint" {
                let v = args.get(i + 1).cloned();
                i += 2;
                v
            } else if let Some(v) = a.strip_prefix("--mountpoint=") {
                i += 1;
                Some(v.to_string())
            } else {
                i += 1;
                None
            };
            let Some(target) = target else {
                continue;
            };
            let p = Path::new(&target);
            let abs = if p.is_absolute() {
                p.to_path_buf()
            } else if let Some(c) = &cwd {
                c.join(p)
            } else {
                continue;
            };
            let canon = std::fs::canonicalize(&abs).unwrap_or_else(|_| abs.clone());
            if canon == want || abs == mp {
                return Some(pid);
            }
        }
    }
    None
}

/// Does `mp` still answer? A dead FUSE daemon leaves `/proc/mounts` entries
/// behind until someone unmounts them; probing the root returns ENOTCONN (107)
/// or EIO (5) for those, while a served mount always stats.
fn mount_answers(mp: &Path) -> bool {
    match std::fs::metadata(mp) {
        Ok(_) => true,
        Err(e) => !matches!(e.raw_os_error(), Some(107) | Some(5)),
    }
}

fn proc_pids() -> Vec<u32> {
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    rd.flatten()
        .filter_map(|e| e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()))
        .collect()
}

fn proc_args(pid: u32) -> Option<Vec<String>> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    if raw.is_empty() {
        return None;
    }
    Some(
        raw.split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect(),
    )
}

fn proc_env(pid: u32, key: &str) -> Option<String> {
    let raw = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    for kv in raw.split(|b| *b == 0) {
        let kv = String::from_utf8_lossy(kv);
        if let Some((k, v)) = kv.split_once('=') {
            if k == key {
                return Some(v.to_string());
            }
        }
    }
    None
}

fn gc_writer_reason(name: &str, mbase: &Path, views: &[String]) -> Option<String> {
    if views.iter().any(|v| v == name) {
        return Some("writable view tree".to_string());
    }
    let slot = mbase
        .join("meta")
        .join("views")
        .join(format!("{name}.layout"));
    if !slot.is_file() {
        return None;
    }
    match grovefs::layout::LayoutRules::parse_def_file(&slot) {
        Ok(grovefs::layout::ViewDef::Alias(real)) if views.iter().any(|v| v == &real) => {
            Some(format!("alias of '{real}'"))
        }
        // Derived (read-only by contract) and foreign names cannot write.
        _ => None,
    }
}

/// Local `grovefs` processes that name this metadata root or one of its data
/// roots on the command line (or through the GROVEFS_* env knobs). Best-effort
/// `/proc` probe: a remote client of a meta-server on ANOTHER machine is not
/// visible here, which is why `--apply` is documented as strictly offline.
fn local_store_users(mbase: &Path, roots: &[PathBuf]) -> Vec<String> {
    let me = std::process::id();
    let mut out = Vec::new();
    for pid in proc_pids() {
        if pid == me || !proc_store_hit(pid, mbase, roots) {
            continue;
        }
        let mut cmd = proc_args(pid).unwrap_or_default().join(" ");
        if cmd.len() > 160 {
            cmd.truncate(157);
            cmd.push_str("...");
        }
        out.push(format!("pid {pid}: {cmd}"));
    }
    out.sort();
    out.dedup();
    out
}

/// Does one process reference this store? Args first (`--home`/`--db-path`/
/// `--data-root`/`--root`, in both `--flag value` and `--flag=value` form),
/// then the GROVEFS_* env knobs. A grovefs process that names NO store falls
/// back to `$HOME/.grovefs` unless it is a remote client (`--meta`/`--data`),
/// which never writes a local page tree.
fn proc_store_hit(pid: u32, mbase: &Path, roots: &[PathBuf]) -> bool {
    let Some(args) = proc_args(pid) else {
        return false;
    };
    if !args.iter().any(|a| a.contains("grovefs")) {
        return false;
    }
    let cwd = std::fs::read_link(format!("/proc/{pid}/cwd")).ok();
    let mut named = false;
    let mut remote = false;
    // Which knobs were given on the command line; a flag overrides its env twin.
    let mut seen = (false, false, false); // --home, --db-path, --data-root/--root
    for (i, a) in args.iter().enumerate() {
        if a == "--meta" || a == "--data" || a.starts_with("--meta=") || a.starts_with("--data=") {
            remote = true;
        }
        let (flag, val) = if let Some(eq) = a.find('=') {
            (&a[..eq], Some(a[eq + 1..].to_string()))
        } else if matches!(a.as_str(), "--home" | "--db-path" | "--data-root" | "--root") {
            (a.as_str(), args.get(i + 1).cloned())
        } else {
            continue;
        };
        if !matches!(flag, "--home" | "--db-path" | "--data-root" | "--root") {
            continue;
        }
        named = true;
        match flag {
            "--home" => seen.0 = true,
            "--db-path" => seen.1 = true,
            _ => seen.2 = true,
        }
        if let Some(v) = val {
            if arg_hits_store(flag, &v, cwd.as_deref(), mbase, roots) {
                return true;
            }
        }
    }
    // Env fallbacks, per knob: a flag on the command line overrides its env
    // counterpart, so a mount started with `--home other` is NOT attributed to
    // this store just because GROVEFS_HOME was inherited from the shell.
    for (given, key) in [
        (seen.0, "GROVEFS_HOME"),
        (seen.1, "GROVEFS_DB_PATH"),
        (seen.2, "GROVEFS_DATA"),
    ] {
        if given {
            continue;
        }
        if let Some(v) = proc_env(pid, key) {
            named = true;
            if arg_hits_store("--home", &v, cwd.as_deref(), mbase, roots) {
                return true;
            }
        }
    }
    if named || remote {
        return false;
    }
    // No store named anywhere: the process uses the default home.
    proc_env(pid, "HOME")
        .map(|h| arg_hits_store("--home", &format!("{h}/.grovefs"), None, mbase, roots))
        .unwrap_or(false)
}

/// Does one flag value (a path or comma-separated path list) name `mbase` or
/// one of `roots`? Relative values resolve against the process's cwd.
fn arg_hits_store(
    flag: &str,
    val: &str,
    cwd: Option<&Path>,
    mbase: &Path,
    roots: &[PathBuf],
) -> bool {
    if !matches!(flag, "--home" | "--db-path" | "--data-root" | "--root") {
        return false;
    }
    val.split(',').any(|part| {
        let part = part.trim();
        if part.is_empty() {
            return false;
        }
        let p = Path::new(part);
        let abs = if p.is_absolute() {
            p.to_path_buf()
        } else if let Some(c) = cwd {
            c.join(p)
        } else {
            return false;
        };
        let canon = std::fs::canonicalize(&abs).unwrap_or_else(|_| abs.clone());
        let same = |w: &Path| {
            let wc = std::fs::canonicalize(w).unwrap_or_else(|_| w.to_path_buf());
            canon == wc || abs == w
        };
        same(mbase) || roots.iter().any(|r| same(r))
    })
}

fn fmt_bytes(n: u64) -> String {
    const K: f64 = 1024.0;
    let f = n as f64;
    if f >= K * K * K {
        format!("{:.1} GiB", f / (K * K * K))
    } else if f >= K * K {
        format!("{:.1} MiB", f / (K * K))
    } else if f >= K {
        format!("{:.1} KiB", f / K)
    } else {
        format!("{n} B")
    }
}

fn fmt_age(secs: u64) -> String {
    if secs >= 86400 {
        format!("{}d{}h", secs / 86400, (secs % 86400) / 3600)
    } else if secs >= 3600 {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

fn print_gc_report(rep: &grovefs::gc::Report, rec: Option<&grovefs::gc::Reclaimed>) {
    let roots: Vec<String> = rep
        .data_roots
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    println!("gc: meta {}", rep.meta_root.display());
    println!("  data root(s): {}", roots.join(", "));
    println!(
        "  views: {} tree(s), {} referenced key(s), {} striped layout(s)",
        rep.views.len(),
        rep.refs,
        rep.striped_layouts
    );
    println!(
        "  objects: {} ({})",
        rep.objects,
        fmt_bytes(rep.objects_bytes)
    );
    println!(
        "  orphans: {} ({}; {} reclaimable, {} within grace <{})",
        rep.orphans.len(),
        fmt_bytes(rep.orphan_bytes()),
        rep.reclaimable(),
        rep.within_grace(),
        fmt_age(rep.grace.as_secs())
    );
    for c in rep.orphans.iter().take(20) {
        println!(
            "    {:>10}  {:>8}  {:<5}  {}",
            fmt_bytes(c.size),
            fmt_age(grovefs::gc::age_secs(c.mtime)),
            c.kind_str(),
            c.key
        );
    }
    if rep.orphans.len() > 20 {
        println!(
            "    ... {} more (use --json for the full list)",
            rep.orphans.len() - 20
        );
    }
    if !rep.held.is_empty() {
        println!(
            "  ledger-held: {} ({}) — NOT deleted; run `grovefs refs --db-path {} rebuild` first if these are stale",
            rep.held.len(),
            fmt_bytes(rep.held_bytes()),
            rep.meta_root.display()
        );
        for c in rep.held.iter().take(5) {
            println!("    {:>10}  {}", fmt_bytes(c.size), c.key);
        }
        if rep.held.len() > 5 {
            println!("    ... {} more", rep.held.len() - 5);
        }
    }
    if !rep.skipped.is_empty() {
        println!(
            "  skipped: {} entr(ies) left untouched (never deleted)",
            rep.skipped.len()
        );
        for s in rep.skipped.iter().take(5) {
            println!("    {s}");
        }
        if rep.skipped.len() > 5 {
            println!("    ... {} more", rep.skipped.len() - 5);
        }
    }
    match rec {
        None => println!(
            "dry run: nothing deleted. Re-run with --apply (offline) to reclaim {} object(s), {}.",
            rep.reclaimable(),
            fmt_bytes(rep.reclaimable_bytes())
        ),
        Some(r) => {
            println!(
                "reclaimed: {} object(s), {} freed; {} within grace kept; {} failed",
                r.deleted,
                fmt_bytes(r.bytes),
                r.within_grace,
                r.failed.len()
            );
            for f in r.failed.iter().take(10) {
                println!("    failed: {f}");
            }
        }
    }
}

fn gc_json(rep: &grovefs::gc::Report, rec: Option<&grovefs::gc::Reclaimed>) -> serde_json::Value {
    let cand = |c: &grovefs::gc::Candidate| {
        serde_json::json!({
            "key": c.key,
            "data_root": c.data_root.display().to_string(),
            "size": c.size,
            "age_secs": grovefs::gc::age_secs(c.mtime),
            "kind": c.kind_str(),
            "within_grace": c.within_grace,
        })
    };
    serde_json::json!({
        "meta_root": rep.meta_root.display().to_string(),
        "data_roots": rep.data_roots.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        "views": &rep.views,
        "referenced_keys": rep.refs,
        "striped_layouts": rep.striped_layouts,
        "objects": rep.objects,
        "object_bytes": rep.objects_bytes,
        "matched_objects": rep.matched,
        "orphans": rep.orphans.iter().map(cand).collect::<Vec<_>>(),
        "orphan_bytes": rep.orphan_bytes(),
        "reclaimable": rep.reclaimable(),
        "reclaimable_bytes": rep.reclaimable_bytes(),
        "within_grace": rep.within_grace(),
        "ledger_held": rep.held.iter().map(cand).collect::<Vec<_>>(),
        "ledger_held_bytes": rep.held_bytes(),
        "skipped": &rep.skipped,
        "grace_secs": rep.grace.as_secs(),
        "applied": rec.is_some(),
        "deleted": rec.map(|r| r.deleted).unwrap_or(0),
        "deleted_bytes": rec.map(|r| r.bytes).unwrap_or(0),
        "delete_failed": rec.map(|r| r.failed.clone()).unwrap_or_default(),
    })
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
///
/// The attempt is repeated and finally escalated to a lazy detach because a
/// termination can land while a request on this mount is still in flight — which
/// is exactly what a lost credential does: the refusal arrives AS the reply to an
/// operation, so the kernel is still finishing that open when we ask for the
/// umount and a plain umount answers `EBUSY`. Stopping there would leave a live
/// mountpoint whose backend is gone (and exiting anyway would leave the classic
/// stale `Transport endpoint is not connected`), so: retry briefly, then `-u -z`,
/// which detaches the mountpoint even while busy and still ends the session. If
/// even that fails we keep serving — a mount that answers with an error is
/// recoverable by an operator, a dead endpoint is not.
fn spawn_term_unmounter(mountpoint: PathBuf) {
    std::thread::spawn(move || {
        install_termination_handlers();
        wait_for_termination();
        let mp = mountpoint.to_string_lossy();
        for attempt in 0..5 {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            if run_fusermount(&["-u", &mp]).is_ok() {
                return;
            }
        }
        let _ = run_fusermount(&["-u", "-z", &mp]);
    });
}

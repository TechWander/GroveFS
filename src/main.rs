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
use std::io::Write;
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
paired with a `meta-server` that many clients mount with `--data-servers`/`--meta-server`),
or as an agent-facing governance service over MCP (`grovefs mcp`).
"#,
    after_long_help = r#"COMMON USAGE

  Local single machine:
    grovefs --home ~/grove init  --view alice
    grovefs --home ~/grove mount --view alice -m ~/mnt
    grovefs umount -m ~/mnt

  Central cluster (one shared view across machines):
    grovefs data-server --data-dir /data    --listen 0.0.0.0:50100 --token SECRET
    grovefs meta-server --meta-dir /meta --listen 0.0.0.0:50053 --token SECRET
    grovefs mount --view alice -m /mnt --data-servers host:50100 --meta-server host:50053 --token SECRET

  Onboard an existing tree (zero-copy when it shares the filesystem):
    grovefs import --meta-dir /meta --view alice --source /srv/legacy

  Per-caller mount credentials instead of one static cluster token (revocable,
  audited; a revoke closes a LIVE mount within 1s and it unmounts itself):
    grovefs apikey      --meta-dir /meta create --label analyst --for mount
    grovefs meta-server --meta-dir /meta --listen 0.0.0.0:50053 --require-api-key
    grovefs data-server --data-dir /data    --listen 0.0.0.0:50100 --require-api-key --meta-dir /meta
    grovefs mount --view alice -m /mnt --data-servers host:50100 --meta-server host:50053 --api-key ID:SECRET

  Server-side governance (no local rules file needed on clients):
    grovefs view --meta-dir /meta define --name lens   --base alice --rules lens.rules
    grovefs view --meta-dir /meta alias  --name public --real alice

  Audit/reclaim orphan bytes in a LOCAL store (offline; scan is the default):
    grovefs gc --meta-dir ~/grovefs              # report orphans / ledger-held keys
    grovefs gc --meta-dir ~/grovefs --apply      # delete >24h orphans (store offline)

  AI-agent surface over MCP (stdio), guarded by an API key:
    grovefs apikey --meta-dir /meta create --label claude-desktop --for mcp   # prints <id>:<secret> once
    grovefs mcp    --meta-dir /meta --require-api-key
    grovefs mcp --help   # list the tools an agent can call

Run `grovefs <command> --help` for a command's full options.

  Long commands shrink: a KEY=VALUE file is auto-loaded into the environment
  ($GROVEFS_CONFIG, else $HOME/.config/grovefs/grovefs.conf),
  and the repeat-heavy flags fall back to env — GROVEFS_META_SERVER / GROVEFS_DATA_SERVERS /
  GROVEFS_TOKEN / GROVEFS_STRIPE_BYTES for `mount` and the servers, GROVEFS_HOME, GROVEFS_META_DIR,
  GROVEFS_STATS, GROVEFS_API_KEY, GROVEFS_MCP_API_KEY, GROVEFS_BW_READ /
  GROVEFS_BW_WRITE. Precedence is always flag > live env > config file;
  keep the file 600 if it holds secrets.
"#
)]
struct Cli {
    /// Base directory for LOCAL mode (single machine).
    ///
    /// Holds `blocks/` (data objects) and `meta/<view>.db` (one DB per view).
    /// Used by `init` and by a mount that has no `--meta-server`/`--data-servers`.
    /// Defaults to $GROVEFS_HOME or ~/.grovefs.
    ///
    /// Remote mode does NOT touch it: clients name the servers directly with
    /// `mount --meta-server/--data-servers`, so nothing is ever written under this path.
    /// On a storage node each service names its own store path instead
    /// (`meta-server --meta-dir`, `data-server --data-dir`).
    #[arg(long, global = true, verbatim_doc_comment)]
    home: Option<PathBuf>,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Initialize a BASE (physical) view (creates its meta DB; optional seed manifest)
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
    grovefs mount --view alice -m /mnt --meta-server HOST:50053 --data-servers HOST:50100 --token SECRET

  Fail over across meta-server replicas (stays on the first reachable node):
    grovefs mount --view alice -m /mnt --meta-server PRIMARY:50053,STANDBY:50053 --data-servers HOST:50100

  Stripe one big file across several data-servers (comma list, order-stable):
    grovefs mount --view alice -m /mnt --data-servers HOST1:50100,HOST2:50100,HOST3:50100

  Strict coherence (no client cache; a peer's writes show immediately):
    grovefs mount --view alice -m /mnt --meta-server HOST:50053 --data-servers HOST:50100 --no-cache

  Read-only mount (every mutation returns EROFS):
    grovefs mount --view alice -m /mnt --ro

  Mount a server-side derived lens or public alias by its name:
    grovefs mount --view lens -m /mnt --meta-server HOST:50053 --data-servers HOST:50100

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
        #[arg(long = "data-servers", value_name = "ADDR[,ADDR]")]
        data: Option<String>,
        /// Stripe chunk size for cross-server striping: a byte count or a
        /// K/M/G suffix (1024-based, e.g. `16M`, `4194304`). Default 16 MiB.
        /// Only meaningful with a multi-address `--data-servers` list. The
        /// value is baked into each NEW object's `data_ref`
        /// (`s<N>:<stripe>:<uuid>`); existing objects keep the stripe they
        /// were written with, so changing this never requires re-ingesting
        /// data. Falls back to env `GROVEFS_STRIPE_BYTES`.
        #[arg(
            long = "stripe-bytes",
            value_name = "SIZE",
            value_parser = grovefs::bwlimit::parse_size_bytes
        )]
        stripe_bytes: Option<u64>,
        /// Remote metadata service address (host:port). If set, the view tree is
        /// served by a central `meta-server` instead of a local `<home>/meta` DB,
        /// so multiple clients mounting the same view share ONE authoritative view
        /// (consistent across machines) rather than diverging local copies.
        /// May be a comma-separated failover list (`primary,standby`): the mount
        /// stays on the first reachable node and falls over to a backup when the
        /// primary dies (pairs with HA `meta-server --role/--peer`).
        #[arg(long = "meta-server", value_name = "ADDR[,ADDR]")]
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
        /// caching, so more metadata/data round-trips. Pairs with `--meta-server`.
        #[arg(long = "no-cache")]
        strict: bool,
        /// Access token presented to a `meta-server` started with `--token`.
        /// Must match the server's token or the mount fails at connect time with
        /// a permission error. Ignored for local metadata (no `--meta-server`).
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
        #[arg(long = "run-as-uid", value_name = "UID")]
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
        /// size/mtime change; `--no-cache` overrides this to 0 (full bypass).
        #[arg(long = "cache-ttl", value_name = "SECS", default_value_t = 2)]
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
        #[arg(short, long = "data-dir", value_name = "DIR[,DIR]")]
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
        /// `<meta-dir>/meta/api_keys.db`; `--meta-dir` defaults to this node's
        /// first `--data-dir`, so a colocated deployment needs no extra flag and a
        /// split one points at a synced/colocated copy of that file.
        #[arg(long)]
        require_api_key: bool,
        /// Metadata root holding `meta/api_keys.db` for `--require-api-key` (the
        /// same layout `grovefs apikey --meta-dir` manages). Defaults to the
        /// first `--data-dir`.
        #[arg(long = "meta-dir", value_name = "DIR")]
        keys_from: Option<PathBuf>,
        /// Key/audit backend for `--require-api-key`. Unset / `"sqlite"` reads
        /// the local `<meta-dir>/meta/api_keys.db`; a `postgres://…`
        /// (`postgresql://…`) URI — or the `GROVEFS_META_STORE` env — reads the shared
        /// `grovefs_control.api_keys` table, so a data-server on a PG deployment
        /// gates mounts against the SAME registry its meta-server does with no
        /// file to colocate (and `--meta-dir` is then unused).
        #[arg(long = "meta-store", value_name = "URI")]
        store: Option<String>,
        /// This node's stable identity within a striped cluster. When clients
        /// mount with a comma `--data-servers h1:port,h2:port,...` list, chunk
        /// placement is normally decided by list position, so reordering the
        /// addresses would silently misroute reads/writes of existing objects.
        /// Each data-server started with a DISTINCT `--member-id` (0, 1, 2, …)
        /// lets those clients place chunks by member IDENTITY instead: the
        /// `--data-servers` order then no longer matters, and a disk replaced
        /// behind the same id keeps its bytes addressable. Members of one
        /// stripe must use unique ids; the id must stay FIXED for a given
        /// store — changing it re-maps every chunk family. Falls back to
        /// `GROVEFS_MEMBER_ID`. Unset (or single-address use) keeps the
        /// historical positional placement.
        #[arg(long = "member-id", value_name = "N")]
        member_id: Option<u64>,
    },
    /// Run the central metadata service many `mount --meta-server` clients share.
    ///
    /// Hosts the authoritative per-view DB that those clients share (no NFS);
    /// pairs with `data-server`.
    MetaServer {
        /// Listen address, e.g. 0.0.0.0:50053
        #[arg(short, long, default_value = "127.0.0.1:50053")]
        listen: String,
        /// Authoritative metadata root for this server: under SQLite every
        /// view DB lives under `<meta-dir>/meta/<view>.db` (created explicitly
        /// by `view create` / `init` / `view import`, never by a mount) and
        /// the API-key/audit store lives at `<meta-dir>/meta/api_keys.db`;
        /// under PostgreSQL (`--meta-store postgres://…`) the view trees AND the
        /// control plane
        /// (definitions, keys, audit) live in that engine, making `<meta-dir>` a
        /// config-only directory (logs, scratch — no authoritative record). Takes
        /// precedence over the global `--home` when both are given.
        #[arg(long = "meta-dir", value_name = "DIR")]
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
        /// store is `<meta-dir>/meta/api_keys.db` — the same file `grovefs mcp
        /// --meta-dir` authenticates against and `grovefs apikey --meta-dir`
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
        /// Requires a *separate* metadata root (`--meta-dir`, or the global
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
        /// SQLite path: one db file per view under `<meta-dir>/meta/`, HA snapshot
        /// shipping available. Set to `postgres://user:pw@host:port/db`
        /// (`postgresql://…` also accepted) to place the authoritative view
        /// tree in PostgreSQL: one `grovefs_v_<hex(view)>` schema per view,
        /// transactions for rename/unlink/lock. Note that the PostgreSQL mode
        /// is **incompatible** with `--role standby` /
        /// `--peer` / `--witness`: GroveFS-native HA snapshot shipping is
        /// `VACUUM INTO`-based and only makes sense on the embedded SQLite
        /// backend; under PG, run PostgreSQL's own streaming/logical replication
        /// for HA instead.
        /// Any other `scheme://` URI is refused at startup — it is never
        /// silently served by SQLite.
        #[arg(long = "meta-store", value_name = "URI")]
        store: Option<String>,
        /// Retire a view's cached metadata engine — its PG connection pool
        /// (8 writers + 4 readers) or SQLite handle — after this many seconds
        /// with no mounted session and no traffic; `0` never evicts (engines
        /// stay resident for the process lifetime). Idle views re-open
        /// transparently on the next op, so resident PG connections scale
        /// with RECENTLY USED views instead of every view opened since start:
        /// the view-count ceiling stops being `max_connections / 12`.
        #[arg(long = "engine-idle-secs", value_name = "SECS", default_value_t = 300)]
        engine_idle_secs: u64,
        /// WARM floor of each PG view's writer pool: connections pre-filled at
        /// engine open and NEVER reaped (they carry the session's prepared
        /// statements). Default 8, exactly the historical `GROVEFS_PG_WRITE_POOL`
        /// value, so setting nothing changes nothing. Lower it (with
        /// `--pg-write-pool-max` raised) to make the per-view connection floor
        /// track real metadata concurrency instead of the configured ceiling.
        /// `GROVEFS_PG_WRITE_POOL` fills the same slot when this flag is absent.
        #[arg(long = "pg-write-pool", value_name = "N")]
        pg_write_pool: Option<usize>,
        /// RETAIN CEILING of each PG view's writer pool: burst connections a
        /// `checkout` dialled are kept up to this many, then shed back to the
        /// warm floor after `--pg-pool-idle-secs` idle. Default (unset or `0`)
        /// is the floor, i.e. the pre-split behavior where anything above the
        /// floor was dropped on return. `GROVEFS_PG_WRITE_POOL_MAX` fills the
        /// same slot.
        #[arg(long = "pg-write-pool-max", value_name = "N")]
        pg_write_pool_max: Option<usize>,
        /// WARM floor of each PG view's reader lane (default 4; env
        /// `GROVEFS_PG_READ_POOL`).
        #[arg(long = "pg-read-pool", value_name = "N")]
        pg_read_pool: Option<usize>,
        /// RETAIN CEILING of each PG view's reader lane (default: the floor;
        /// env `GROVEFS_PG_READ_POOL_MAX`).
        #[arg(long = "pg-read-pool-max", value_name = "N")]
        pg_read_pool_max: Option<usize>,
        /// Idle window after which a PG pool connection held ABOVE its warm
        /// floor is closed (default 60, `0` never reaps). Sized to keep a
        /// bursty view from paying connect + SERIALIZABLE pragma + prepared
        /// re-parse again on every write (measured 577us vs 316us per write
        /// transaction). `GROVEFS_PG_POOL_IDLE_SECS` fills the same slot.
        #[arg(long = "pg-pool-idle-secs", value_name = "SECS")]
        pg_pool_idle_secs: Option<u64>,
        /// Expose the REMOTE CONTROL PLANE on a second listener
        /// (`--control-listen HOST:PORT`): `grovefs view` / `grovefs apikey` /
        /// `grovefs refs` calls arrive as framed requests and are executed HERE,
        /// by the process that already owns the metadata store. That is what
        /// replaces handing every operator machine a database password: a
        /// remote admin presents an ACCOUNT KEY (`apikey create --for admin
        /// --owner <account>`) and never sees a store URI — and it works for the
        /// SQLite backend too, whose trees were previously local-only.
        ///
        /// The port ALWAYS authenticates as `Scope::Admin`; there is no open
        /// mode (mint the first admin key on this host). Bound before `--listen`
        /// accepts, so a bad address fails the start. Every call is audited as
        /// `method=control` in the same `mcp_calls` table `apikey calls` reads.
        /// Unset (default) keeps this process exactly as it was.
        #[arg(long = "control-listen", value_name = "HOST:PORT")]
        control_listen: Option<String>,
        /// Seed the web console's FIRST owner at install time
        /// (`grovefs web --control-server` signs in against the accounts this
        /// process creates). Applied ONLY when the account store is empty:
        /// a governance entrance that can mint its own administrator has no
        /// gate. Re-named on a later start with the same account is ignored (a
        /// systemd unit re-reads its flags on every restart, and that must not
        /// be an outage); a DIFFERENT account into a non-empty store is refused.
        /// After the first sign-in, accounts come from the console or
        /// `grovefs web account create`, never from a start-up flag.
        #[arg(long = "admin-user", value_name = "ACCOUNT")]
        admin_user: Option<String>,
        /// File holding `--admin-user`'s password (its first line, trailing
        /// newline stripped). A PATH and not the password itself because argv is
        /// readable by every local process (`ps`, the journal, a shell history)
        /// — and because this value, unlike every other credential here, is
        /// stretched into `web_accounts` and then never needed again. `0600` by
        /// the owner root is the expectation; nothing here can enforce it.
        #[arg(long = "admin-password-file", value_name = "PATH")]
        admin_password_file: Option<PathBuf>,
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
    /// fork. Definitions live in the control-plane store (SQLite
    /// `<meta-dir>/meta/views/<name>.layout`, PG `grovefs_control.view_defs`),
    /// clients simply mount
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
        /// The metadata root — same option name and directory as `meta-server
        /// --meta-dir` / `grovefs apikey --meta-dir`; falls back to the global
        /// `--home` when omitted. Under SQLite it holds `meta/views/*.layout`.
        #[arg(long = "meta-dir", value_name = "DIR")]
        db_path: Option<PathBuf>,
        /// Control-plane backend. Unset / `"sqlite"` governs the local
        /// `<meta-dir>/meta/views/*.layout` files; a `postgres://…`
        /// (`postgresql://…` too) URI governs the shared `grovefs_control.view_defs`
        /// table. A `postgres://…` URI — or the `GROVEFS_META_STORE` env — makes
        /// definitions and the
        /// meta-server's own resolver read/write ONE system of record. Must
        /// match the `meta-server --meta-store` this root is served by.
        #[arg(long = "meta-store", value_name = "URI")]
        store: Option<String>,
        /// Govern the REMOTE deployment served by `meta-server --control-listen
        /// HOST:PORT` instead of a store on this machine: the server performs
        /// the work and this host needs only an admin ACCOUNT KEY (no database
        /// URI, no access to `<meta-dir>`). Works for BOTH backends, including
        /// SQLite. Cannot be combined with `--meta-dir`/`--meta-store`: a
        /// remote control plane is bound to its server, so a local path here
        /// would describe a store this deployment never sees (the pair is
        /// refused, not silently merged).
        #[arg(long = "control-server", value_name = "HOST:PORT")]
        control_server: Option<String>,
        /// The `admin`-scope account key (`<id>:<secret>`, printed once by
        /// `apikey create --for admin --owner <account>`) for --control-server.
        /// Falls back to `GROVEFS_CONTROL_KEY`, then `GROVEFS_API_KEY`.
        #[arg(long = "api-key", value_name = "ID:SECRET")]
        control_key: Option<String>,
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
    /// writes the store's view definitions (live meta-server mounts hot-reload
    /// them) but never mounts or touches data. `--meta-dir`
    /// sets the default metadata root (falls back to `--home`); a per-call
    /// `db_path` argument overrides it.
    #[command(
        after_long_help = r#"MCP TOOL SURFACE — what `tools/list` returns (10 tools).
Every tool takes an optional `db_path` (defaults to this server's --meta-dir/--home),
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
        /// directory as `grovefs view` / `meta-server --meta-dir`.
        #[arg(long = "meta-dir", value_name = "DIR")]
        db_path: Option<PathBuf>,
        /// Require an API key before the tool surface is usable. With it, an
        /// MCP client must present a `grovefs apikey create` key as `apiKey` in
        /// the `initialize` params (or the operator exports
        /// `GROVEFS_MCP_API_KEY` to pre-authenticate the whole server); until a
        /// key authenticates, `tools/list` and `tools/call` are refused with a
        /// JSON-RPC error. `initialize`/`ping` stay open so a client can always
        /// handshake. Off by default: a plain `grovefs mcp` trusts whoever can
        /// spawn it (the same OS boundary as a local mount). Either way, every
        /// tool call is appended to `<meta-dir>/meta/api_keys.db`'s audit log.
        #[arg(long)]
        require_api_key: bool,
        /// Control-plane backend. Unset / `"sqlite"` governs the local
        /// `<meta-dir>/meta/views/*.layout` files and `api_keys.db`; a
        /// `postgres://…` (`postgresql://…`) URI — or the `GROVEFS_META_STORE` env —
        /// binds the server to the shared `grovefs_control` schema (definitions,
        /// keys and the call audit all in PG). A PG-bound server ignores a
        /// per-call `db_path` override: one URI, one system of record.
        #[arg(long = "meta-store", value_name = "URI")]
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
        /// directory as `grovefs mcp --meta-dir` / `meta-server --meta-dir`; falls
        /// back to the global `--home`.
        #[arg(long = "meta-dir", value_name = "DIR")]
        db_path: Option<PathBuf>,
        /// Key/audit backend. Unset / `"sqlite"` uses the local
        /// `<meta-dir>/meta/api_keys.db`; a `postgres://…` (`postgresql://…`)
        /// URI — or the `GROVEFS_META_STORE` env — uses the shared
        /// `grovefs_control.api_keys`/`mcp_calls` tables, so a key minted here
        /// is authoritative for every meta-/data-server and MCP pointing at that
        /// URI with no per-node file to colocate.
        #[arg(long = "meta-store", value_name = "URI")]
        store: Option<String>,
        /// Manage the keys of the REMOTE deployment behind `meta-server
        /// --control-listen HOST:PORT` with an admin account key. The first
        /// admin key must be minted on the server host (a control port that
        /// would mint its own administrator would defeat the gate).
        #[arg(long = "control-server", value_name = "HOST:PORT")]
        control_server: Option<String>,
        /// The `admin`-scope account key for --control-server
        /// (`GROVEFS_CONTROL_KEY` / `GROVEFS_API_KEY` fill a missing value).
        #[arg(long = "api-key", value_name = "ID:SECRET")]
        control_key: Option<String>,
        #[command(subcommand)]
        action: ApikeyAction,
    },
    /// Serve the MANAGEMENT CONSOLE, and administer its sign-in accounts.
    ///
    /// With no subcommand this is a small HTTP gateway in front of `meta-server
    /// --control-listen`: it serves the embedded single-page app (overview, views,
    /// keys, accounts and grants, audit), turns a browser session into an ACCOUNT,
    /// and forwards each click to the control plane — where the rule that decides
    /// what that account may touch lives beside the data. It holds ONE
    /// `admin`-scope key and no store credential, adds no governance rule of its
    /// own, and only ever REMOVES capability: the verbs it refuses to route
    /// (`gc`, `refs rebuild`, `view rm-base`, `fork graft`) stay CLI-only, where
    /// their dry-run reports and typed confirmations are part of calling them.
    ///
    /// Sign in as the account seeded at install time (`meta-server --admin-user
    /// <name> --admin-password-file <path>`, applied only while the account store
    /// is empty), then create the rest here or in the page. Loopback is the
    /// default bind and this server speaks plain HTTP: a routable `--listen` needs
    /// `--allow-insecure-http`, or TLS on a reverse proxy in front of it (with
    /// `--use-x-forwarded-for`, so sign-ins are throttled per client rather than
    /// per proxy).
    #[cfg(feature = "web")]
    Web {
        /// The governance entrance to put the console in front of: the
        /// `meta-server --control-listen HOST:PORT` that owns the accounts, the
        /// views and the audit log. Required — this process stores nothing.
        #[arg(long = "control-server", value_name = "HOST:PORT", global = true)]
        control_server: Option<String>,
        /// The `admin`-scope account key for --control-server
        /// (`GROVEFS_CONTROL_KEY` / `GROVEFS_API_KEY` fill a missing value). This
        /// is the deployment's governance credential: it must stay on this host,
        /// which is what `--api-key-file` (a `0600` secret file a unit reads) and
        /// the loopback default are for.
        #[arg(long = "api-key", value_name = "ID:SECRET", global = true)]
        control_key: Option<String>,
        /// File holding that key (its first line) instead of `--api-key`, for a
        /// systemd unit or a mounted secret — argv is visible to every local
        /// process and lands in the shell history. Beats `GROVEFS_CONTROL_KEY`.
        #[arg(long = "api-key-file", value_name = "PATH", global = true)]
        control_key_file: Option<PathBuf>,
        /// Address to serve the console on. `127.0.0.1` by default; a browser on
        /// another host reaches it through a reverse proxy or an SSH tunnel
        /// (`ssh -L 8787:127.0.0.1:8787 <host>`).
        #[arg(long = "listen", value_name = "HOST:PORT", default_value = grovefs::web::DEFAULT_LISTEN)]
        listen: String,
        /// Idle window of a signed-in session (`30m`, `8h`, `2d`). The console
        /// mints keys and changes roles, so an unattended browser gets a shorter
        /// leash than an unattended shell. A revoked grant or a disabled account
        /// takes effect sooner than this: on that session's NEXT call, because the
        /// server re-reads the principal per request.
        #[arg(long = "session-ttl", value_name = "AGE", default_value = "8h")]
        session_ttl: String,
        /// Worker threads, i.e. how many console requests may be in flight. One
        /// governance call blocks its thread, so this caps concurrency and waiting
        /// browsers at once; further requests queue in the listener.
        #[arg(long, default_value_t = 4)]
        workers: usize,
        /// Bind a NON-LOOPBACK address over plain HTTP anyway: every sign-in and
        /// every governance call then crosses the wire unencrypted. Only correct
        /// when the network itself is trusted end to end.
        #[arg(long)]
        allow_insecure_http: bool,
        /// Take the client address from `X-Forwarded-For` instead of the socket.
        /// Correct ONLY behind a proxy that sets and sanitizes that header: an
        /// untrusted value would turn the sign-in throttle and the audit's `ip`
        /// into something the caller chooses.
        #[arg(long)]
        use_x_forwarded_for: bool,
        #[command(subcommand)]
        action: Option<WebAction>,
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
    /// actually exist. Works on the SQLite store (`meta/refs.db`) and, through
    /// `--meta-store postgres://…`, on the ledger that lives IN the PostgreSQL
    /// schemas. See `grovefs view create`.
    Refs {
        /// The metadata root holding `meta/refs.db` and the per-view trees
        /// `meta/*.db` — same option name and directory as `meta-server
        /// --meta-dir`; falls back to the global `--home`.
        #[arg(long = "meta-dir", value_name = "DIR")]
        db_path: Option<PathBuf>,
        /// Metadata STORE URI: a `postgres://…` URI rebuilds the ledger that
        /// lives IN the PostgreSQL schemas instead of `meta/refs.db`. Falls back
        /// to `$GROVEFS_META_STORE`, exactly like `gc`'s backend guards.
        #[arg(long = "meta-store", value_name = "URI")]
        store: Option<String>,
        /// Rebuild the ledger on the REMOTE deployment behind `meta-server
        /// --control-listen HOST:PORT` (either backend) with an admin account
        /// key instead of touching the store here.
        #[arg(long = "control-server", value_name = "HOST:PORT")]
        control_server: Option<String>,
        /// The `admin`-scope account key for --control-server
        /// (`GROVEFS_CONTROL_KEY` / `GROVEFS_API_KEY` fill a missing value).
        #[arg(long = "api-key", value_name = "ID:SECRET")]
        control_key: Option<String>,
        #[command(subcommand)]
        action: RefsAction,
    },
    /// Audit and reclaim ORPHAN objects (offline GC).
    ///
    /// An orphan is an object that NO surviving view tree names and the
    /// cross-view ledger does not book — the residue of an out-of-band
    /// `rm meta/<view>.db` (no unlink ever ran) or a crash between object
    /// creation and the node insert. SCAN IS THE DEFAULT: it prints the counts,
    /// the biggest orphans and any ledger-held keys that must first be released
    /// with `grovefs refs rebuild`.
    ///
    /// Two ways to reach the store. `<data-dir>/blocks/` is audited in place,
    /// which is the local deployment. With `--control-server` the TREES are read
    /// by the server that owns them — SQLite or PostgreSQL — and
    /// the objects are listed from `--data-servers`, so any machine holding an
    /// admin key runs the same audit for either backend.
    ///
    /// `--apply` deletes only objects older than `--older-than` (a hard-linked
    /// import inherits the SOURCE file's mtime, so young-object filtering alone
    /// is not enough), never a key the ledger still holds, and refuses to run
    /// while a view is mounted or a grovefs process is using the store. A view
    /// tree that cannot be read aborts the whole scan rather than guessing — a
    /// skipped tree would make every blob it names look orphaned.
    Gc {
        /// Metadata root holding `meta/*.db` — same option name and directory
        /// as `meta-server --meta-dir`; falls back to the global `--home`.
        #[arg(long = "meta-dir", value_name = "DIR")]
        db_path: Option<PathBuf>,
        /// Data root(s) holding `blocks/` — comma-separated for several disks,
        /// same convention as `data-server --data-dir`; defaults to `--meta-dir`.
        #[arg(long = "data-dir", value_name = "DIR[,DIR]")]
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
        /// Audit the deployment behind `meta-server --control-listen HOST:PORT`
        /// with an admin account key: the server reads its own trees (any
        /// backend) and the objects are listed from `--data-servers`.
        #[arg(long = "control-server", value_name = "HOST:PORT")]
        control_server: Option<String>,
        /// The `admin`-scope account key for --control-server
        /// (`GROVEFS_CONTROL_KEY` / `GROVEFS_API_KEY` fill a missing value).
        #[arg(long = "api-key", value_name = "ID:SECRET")]
        control_key: Option<String>,
        /// Data server(s) holding the bytes — comma-separated `HOST:PORT`, the
        /// same addresses a mount uses (`GROVEFS_DATA_SERVERS` fills a missing
        /// value). Required with --control-server.
        #[arg(long = "data-servers", value_name = "HOST:PORT[,HOST:PORT]")]
        data_servers: Option<String>,
        /// Static token the data server was started with (`--require-token`);
        /// `GROVEFS_TOKEN` fills a missing value.
        #[arg(long = "data-token", value_name = "TOKEN")]
        data_token: Option<String>,
        /// Account key the data server was started with (`--require-api-key`);
        /// `GROVEFS_API_KEY` fills a missing value.
        #[arg(long = "data-api-key", value_name = "ID:SECRET")]
        data_api_key: Option<String>,
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
    /// remount); a PostgreSQL deployment is REFUSED outright — under
    /// `--meta-store postgres://…`
    /// the authoritative tree is a PG schema, so the
    /// bytes move through a mounted path (rsync into the mount), not through
    /// this command. `--meta-store` (or `GROVEFS_META_STORE`) declares the
    /// deployment's backend, and a `postgres://…` URI is refused rather than
    /// served as a local import the deployment never sees.
    Import {
        /// Metadata root holding `meta/<view>.db` — same option name and
        /// directory as `meta-server --meta-dir`; falls back to `--home`.
        #[arg(long = "meta-dir", value_name = "DIR")]
        db_path: Option<PathBuf>,
        /// Data root holding `blocks/` — same option name and values as
        /// `data-server --data-dir` (a comma-separated list spreads imported
        /// objects across the node's disks by the same key-hash placement);
        /// defaults to `--meta-dir` (the colocated topology used by every
        /// deployment example).
        #[arg(long = "data-dir", value_name = "DIR[,DIR]")]
        data_root: Option<PathBuf>,
        /// Target view (its meta DB is created if absent, like `init`).
        #[arg(long)]
        view: String,
        /// Existing directory to mirror in.
        #[arg(long)]
        source: PathBuf,
        /// Metadata backend of the deployment this import would write into. The
        /// command produces a LOCAL SQLite tree (`meta/<view>.db` plus hardlinks
        /// or copies under `blocks/`), so a `postgres://…` store —
        /// given here or through `GROVEFS_META_STORE` — is REFUSED with the
        /// alternative named (rsync/cp through a mounted path), never silently
        /// served as a SQLite import the deployment's meta-server cannot see.
        #[arg(long = "meta-store", value_name = "URI")]
        store: Option<String>,
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
        #[arg(long = "meta-server", value_name = "ADDR[,ADDR]")]
        meta: Option<String>,
        /// Access token for meta-server (if --meta-server requires auth).
        #[arg(long)]
        meta_token: Option<String>,
        /// API key for meta-server (if --meta-server requires auth).
        #[arg(long)]
        meta_api_key: Option<String>,
        /// PostgreSQL store URI (same as meta-server --meta-store).
        #[arg(long = "meta-store", value_name = "URI")]
        store: Option<String>,
        /// Metadata root for SQLite mode (meta/<view>.db lives under here).
        #[arg(long = "meta-dir", value_name = "DIR")]
        db_path: Option<PathBuf>,
        /// Data-server endpoint(s) for batch stat: `host:port`, or a
        /// comma-separated list in the same order the mount used. Needed when
        /// the tree comes through `--meta-server`/`--meta-store`, which imply no local
        /// storage root to stat. A striped store (`s<N>` data_refs) can only
        /// be measured with exactly the `N` addresses the writer used: a list
        /// of any other length is refused in either direction, not zeroed.
        #[arg(long = "data-servers", value_name = "ADDR[,ADDR]")]
        data: Option<String>,
        /// Local data root holding blocks/; defaults to the store root named
        /// by `--meta-dir`/`--home` (same convention as `grovefs import
        /// --data-dir`). Without either a local root or `--data-servers`, sizes would
        /// be guessed at zero, so the command refuses.
        #[arg(long = "data-dir", value_name = "DIR[,DIR]")]
        data_root: Option<PathBuf>,
        /// API key for data-server authentication.
        #[arg(long = "data-api-key")]
        api_key: Option<String>,
        /// Access token for data-server (legacy shared-token mode).
        #[arg(long = "data-token")]
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
#[command(
    after_help = "Four view kinds - the verb that creates each:\n  base     -> view create --view V                 (or: grovefs init --view V)\n  derived  -> view define  --base B --name D --rules F   (read-only lens)\n  alias    -> view alias   --real R --name A             (public name, writable)\n  fork     -> view fork    --base B --name F             (fill it with: view graft)\nOnly a BASE view owns physical deletion; derived/alias/fork reshape or rename one."
)]
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
        /// `<meta-dir>/meta/<view>.db`.
        #[arg(long)]
        view: String,
        /// Optional seed manifest (`name<TAB>content` lines) to populate the
        /// freshly created base view with, exactly like `init --manifest`.
        #[arg(long)]
        manifest: Option<PathBuf>,
        /// With a multi-instance PostgreSQL store (`--meta-store file:/path/…`)
        /// the instance (registry name) to place this view's tree on; without
        /// it the view lands on the instance carrying the fewest placed views.
        /// Single-instance stores accept the flag only if it names their one
        /// instance.
        #[arg(long = "pg-instance", value_name = "NAME")]
        pg_instance: Option<String>,
    },
    /// Create a FORK `--name` of the physical base view `--base`: an
    /// INDEPENDENT metadata tree seeded with a point-in-time copy of the base's
    /// names. Content bytes are SHARED (booked in the cross-view ledger, no
    /// copies), so a fork costs metadata only.
    ///
    /// A fork is the writable middle ground between a base view and a lens:
    /// structure (mkdir/rename/unlink/symlink/link) is allowed, but CONTENT
    /// writes (write/truncate) are refused with EROFS — uploading bytes belongs
    /// to a base view. Deleting a name removes only the fork's own metadata;
    /// the base's tree is untouched and a shared object is freed only when no
    /// view holds a name for it anymore. Removing the fork (`view rm`) deletes
    /// its tree and releases its ledger holds; content bytes the base still
    /// names stay.
    ///
    /// The base must be a PHYSICAL view (a name with a tree of its own and no
    /// definition). Both metadata backends can fork: SQLite copies a `.db`
    /// file's rows, PostgreSQL copies one `grovefs_v_<hex>` schema into another
    /// — each in one transaction.
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
    /// source brings its whole subtree; the placement is all-or-nothing — one
    /// transaction on either backend — and copies no
    /// bytes: rows enter the fork, objects
    /// stay shared through the cross-view ledger. A plan that does not fit one
    /// transaction fails whole and writes nothing; re-running a partially
    /// applied plan with `--skip-existing` continues
    /// where it stopped. An existing name refuses the
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
    /// automatically (the server re-checks the definition's change ticket per op
    /// — mtime under SQLite; the
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
    /// WITH `--base`: remove an entire physical BASE view `--name` — its
    /// metadata tree dies, the cross-view ledger is rebuilt from the surviving
    /// trees, and every content blob no other view holds is freed through the
    /// store's data root (`--data-dir`, default `--home`; point it at the
    /// root/data-server that actually stores the objects).
    ///
    /// What "the tree" is depends on the backend the name lives in. A local
    /// store loses `meta/<name>.db` (+ WAL/SHM) and the shared `refs` ledger is
    /// recomputed from the trees that survive. With
    /// `--meta-store postgres://…` the tree is a `grovefs_v_<hex>` schema, so
    /// the command unbooks what the schema held and drops it in ONE
    /// transaction (PostgreSQL's DDL is transactional), then frees the same
    /// way. The report states both halves separately (names gone / objects
    /// unbooked / objects not found under this data root), so "the tree is
    /// gone" and "the bytes were reclaimed" are never collapsed into one
    /// number.
    ///
    /// Guards: a governed name needs the plain `view rm` first, and an actively
    /// mounted `grovefs:<name>` is refused. Other machines' live mounts of the
    /// same meta-server are not visible here — stop them first.
    Rm {
        #[arg(long)]
        name: String,
        /// Also remove the PHYSICAL base tree and reclaim unreferenced bytes
        /// (see the long help; default removes only a definition slot).
        #[arg(long)]
        base: bool,
        /// Data root holding `blocks/` for `--base` byte reclaim (same option
        /// name and fallback as `grovefs import --data-dir`).
        #[arg(long = "data-dir", value_name = "DIR[,DIR]")]
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
    /// With `--meta-store postgres://…` the same command reads that view's PG
    /// schema instead of a local tree; a name the schema set never provisioned
    /// is refused outright, so no empty dump is written for it.
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
    /// fork semantics: structure writable, content EROFS). Under
    /// `--meta-store postgres://…` the import instead OPENS that view's schema
    /// in the database — the only tree-building entry besides `view create` —
    /// and `--as-fork` records the definition there too (the tree is the schema
    /// the dump just filled; if writing `fork <base>` fails, the schema is
    /// dropped again).
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
    /// Move a view's tree to another instance of a multi-instance PostgreSQL
    /// store (`--meta-store file:/path/…`): snapshot the source schema, build
    /// the copy on `--pg-instance`, verify it against the dump's own header,
    /// then flip the placement row (the atomic cutover) and retire the source
    /// (schema + its ledger holds, one transaction) unless `--keep-source`
    /// leaves it as a rollback point. Metadata only — content bytes stay in
    /// the data domain they live in (the server-side gc unions ALL instances'
    /// ledgers, so nothing is reclaimed out from under the moved view); mounts
    /// keep pointing at the same `--data-servers`. Refuses a view mounted on
    /// this machine; quiesce other machines yourself.
    Move {
        #[arg(long)]
        view: String,
        /// Target instance (registry name). A `--keep-source` move can be
        /// rolled back by re-running the command with the original home.
        #[arg(long = "pg-instance", value_name = "NAME")]
        pg_instance: String,
        /// Leave the source schema on its instance as a rollback point
        /// instead of dropping it (and unbooking its ledger holds) after the
        /// cutover succeeds.
        #[arg(long)]
        keep_source: bool,
    },
    /// Show or set a view's PostgreSQL connection-pool bounds — the WARM floor
    /// (connections held for the whole engine lifetime) and the RETAIN CEILING
    /// (burst connections kept before the idle reaper sheds them), per lane.
    /// With no flags it shows what the view declares and what the process
    /// defaults to; `--write FLOOR[:MAX]` / `--read FLOOR[:MAX]` / `--idle-secs
    /// N` record overrides in the view's OWN metadata tree (so a `view move`
    /// carries them along), and `--inherit` removes them again.
    ///
    /// This is how a fork-heavy fleet stops paying 12 connections per tree:
    /// a branch that is read and structure-written but never content-written
    /// (content writes are EROFS on a fork) needs a writer floor of 1–2, not 8,
    /// and still gets 8 during an import burst. Bounds take effect on the
    /// view's NEXT engine open — mounted sessions pin the engine and its
    /// connections hold session-scoped prepared statements, so nothing is
    /// yanked out from under a live mount.
    Pool {
        #[arg(long)]
        view: String,
        /// Writer lane as `FLOOR[:MAX]` (e.g. `2:8`); bare `N` keeps the
        /// historical floor==ceiling shape.
        #[arg(long, value_name = "FLOOR[:MAX]")]
        write: Option<String>,
        /// Reader lane as `FLOOR[:MAX]`.
        #[arg(long, value_name = "FLOOR[:MAX]")]
        read: Option<String>,
        /// Per-view idle window (seconds) before a burst connection above the
        /// floor is reaped; `0` never reaps.
        #[arg(long = "idle-secs", value_name = "SECS")]
        idle_secs: Option<u64>,
        /// Drop this view's overrides and follow the meta-server's process-wide
        /// bounds again.
        #[arg(long)]
        inherit: bool,
    },
}

#[derive(Subcommand)]
enum ApikeyAction {
    /// Mint a new key labeled `--label` and print the full plaintext
    /// `<id>:<secret>` ONCE (the secret is not recoverable afterward — only its
    /// SHA-256 is stored; `apikey rotate` mints a fresh secret for the same id
    /// when one is lost). Give this to the agent/client; it presents it as
    /// `apiKey` in the MCP `initialize` params, or as `mount --api-key` for a
    /// `meta-server`/`data-server` started with `--require-api-key`.
    Create {
        #[arg(long)]
        label: String,
        /// Which entrance the key may open: `mount` (a `--require-api-key`
        /// meta/data-server), `mcp` (`grovefs mcp --require-api-key`), `admin`
        /// (the governance port, `meta-server --control-listen` — this is the
        /// DATABASE-REPLACEMENT credential, and it also admits mount+mcp) or
        /// `any` (mount+mcp, the default, and what every key minted before
        /// scopes existed carries — `any` deliberately does NOT imply `admin`).
        /// Narrow it so a mount credential cannot be replayed against the
        /// governance API, and an agent key cannot mount data.
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
        /// The ACCOUNT this key belongs to (a person or a service). One account
        /// holds SEVERAL keys, which is what makes rotation and per-device
        /// revocation real: `revoke --id` kills one key while the account keeps
        /// working — the opposite of a shared database password, where every
        /// holder must be re-taught a new secret. Also what the control-plane
        /// audit attributes a call to. Required for `--for admin`.
        #[arg(long)]
        owner: Option<String>,
        /// Attach a REMARK — free text like "kitchen laptop, staging box, hand
        /// over at the end of March" (`apikey update --note` edits it later,
        /// `list --note` searches it). The field that turns a registry of
        /// hundreds from a guessing game into a list.
        #[arg(long, value_name = "TEXT")]
        note: Option<String>,
        /// Make the key expire after this long: `45m`, `24h`, `7d`, `365d`
        /// (default: never). The deadline is stored and enforced SERVER-side —
        /// a key past it fails its next handshake, and a mount already running
        /// on it is refused on its next op (≤1s) — so a lost or parked laptop
        /// stops being a credential without anyone remembering to revoke it.
        /// Extend or remove it later with `apikey update --ttl`.
        #[arg(long, value_name = "DUR")]
        ttl: Option<String>,
    },
    /// List every key (id, masked key, owner, label, scope, view binding,
    /// ro/rw, created/last-used/expires/revoked, remark) — never the secret
    /// itself, only the 8-hex-digit fingerprint that identifies one row (see
    /// `apikey rotate` for getting a usable key onto a client again). Add
    /// `--view NAME` to show only the keys that can open that view (a pure alias
    /// also matches keys bound to its real view). `--label`/`--owner`/`--note`
    /// match case-insensitively as substrings, `--scope` narrows to one entrance
    /// (`any`/`mount`/`mcp`/`admin`) — for a registry too big to scan by eye.
    List {
        /// Only keys that can open this view: an exact `--view` binding, or
        /// (when NAME is a pure alias) keys bound to its real view. Unbound
        /// system keys admit every view and are summarized separately.
        #[arg(long)]
        view: Option<String>,
        /// Only keys whose label contains this (case-insensitive).
        #[arg(long)]
        label: Option<String>,
        /// Only keys owned by this account (case-insensitive substring).
        #[arg(long)]
        owner: Option<String>,
        /// Only keys whose REMARK contains this (case-insensitive substring) —
        /// the free-text field, so `--note staging` finds the staging keys.
        #[arg(long, value_name = "TEXT")]
        note: Option<String>,
        /// Only keys with this exact entrance scope: `any`, `mount`, `mcp` or
        /// `admin`.
        #[arg(long, value_name = "SCOPE")]
        scope: Option<String>,
    },
    /// Revoke the key with id `--id` (short id prefix accepted). Revoked keys
    /// fail authentication but stay listed so past audit rows keep attribution.
    Revoke {
        #[arg(long)]
        id: String,
    },
    /// Replace the secret of an ACTIVE key in place (`--id`, short id prefix
    /// accepted, like `revoke`) and print the NEW `<id>:<secret>` ONCE — the
    /// recovery path when the secret of a live credential is lost, sparing the
    /// revoke-repair dance: the id, label, scope, view binding, ro flag, owner
    /// and audit history all survive untouched. The old secret fails its next
    /// handshake; a session already established on it lives until it
    /// reconnects (rotation replaces a credential, only revocation kills one
    /// now). A revoked id is refused — rotate never half-resurrects a key.
    Rotate {
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
    /// Edit the BOOKKEEPING of an active key in place (short id prefix accepted,
    /// like `revoke`): its remark (`--note TEXT`, `--note ""` clears it) and its
    /// deadline (`--ttl 7d` from now, `--ttl never` to remove it). The usual
    /// reason is an expired key: it authenticates nowhere until it is given a
    /// new `--ttl`. Nothing else about a key is editable, deliberately — `--for`
    /// and `--view` decide what a credential can REACH, and moving them under a
    /// running mount would make the binding a suggestion; mint a new key (or
    /// `set-ro`, which only moves the write verdict). A revoked id is refused:
    /// revoke is final.
    Update {
        #[arg(long)]
        id: String,
        /// Replace the remark (an empty value clears it).
        #[arg(long, value_name = "TEXT")]
        note: Option<String>,
        /// New lifetime measured from NOW (`45m`, `24h`, `7d`, `365d`) or
        /// `never`.
        #[arg(long, value_name = "DUR")]
        ttl: Option<String>,
    },
}

/// The headless half of the web console: the same account and delegation verbs
/// the page offers, for the cases a browser is wrong for (an install script, an
/// operator over SSH, `--json` for a tool). They run as the OPERATOR — the admin
/// key with no account attached — which is the plane's oldest and widest
/// credential, so these commands are how a deployment's first accounts get made
/// before anyone can sign in.
///
/// There is deliberately no `--meta-dir`/`--meta-store` form of them: the
/// meta-server owns its account store, and rules like "the last enabled admin
/// cannot be demoted" and the password policy live in front of that store. A CLI
/// that opened the table directly would be a second implementation of them, and
/// the one nobody audits.
#[cfg(feature = "web")]
#[derive(Subcommand)]
enum WebAction {
    /// Manage sign-in accounts (`--control-server` + an admin key required).
    #[command(subcommand)]
    Account(WebAccountAction),
    /// Manage which BASE views an account governs — its delegations. An account
    /// with no grant manages nothing: `admin` is the role that ignores grants, so
    /// `create` alone never widens anyone's reach.
    #[command(subcommand)]
    Grant(WebGrantAction),
}

#[cfg(feature = "web")]
#[derive(Subcommand)]
enum WebAccountAction {
    /// Create an account named `--account` with the password in
    /// `--password-file` (or on stdin with `--password-stdin`) and role
    /// `--role user|admin`. A password never goes on a command line: argv is
    /// visible to every local process and lands in the shell history. The
    /// minimum length and the "not the account name" rule are the server's
    /// (`webauth::check_password`) and are reported verbatim.
    Create {
        #[arg(long, value_name = "ACCOUNT")]
        account: String,
        #[arg(long = "password-file", value_name = "PATH")]
        password_file: Option<PathBuf>,
        /// Read the password from stdin — `pass generate … | grovefs web account
        /// create --account alice --password-stdin`, so the secret appears in no
        /// argument list and no file.
        #[arg(long, conflicts_with = "password_file")]
        password_stdin: bool,
        /// `user` (default): governs only the base views it is granted. `admin`:
        /// governs the deployment, grants included.
        #[arg(long, default_value = "user")]
        role: String,
    },
    /// List accounts (name, role, created/last sign-in, disabled, granted base
    /// views) — never a password digest.
    List,
    /// Set a new password for `--account` from `--password-file` /
    /// `--password-stdin`. An active browser session is NOT ended by this (it is
    /// the gateway's cookie table); `disable` is the verb that stops someone now.
    Passwd {
        #[arg(long, value_name = "ACCOUNT")]
        account: String,
        #[arg(long = "password-file", value_name = "PATH")]
        password_file: Option<PathBuf>,
        #[arg(long, conflicts_with = "password_file")]
        password_stdin: bool,
    },
    /// Change an account's role. Demoting the LAST enabled admin is refused —
    /// that is the lock-out this guard exists for; create and verify a second
    /// admin first.
    Role {
        #[arg(long, value_name = "ACCOUNT")]
        account: String,
        #[arg(long, value_name = "user|admin")]
        role: String,
    },
    /// Let `--account` sign in again (undo `disable`). Grants are untouched by
    /// either verb, so re-enabling returns exactly the reach it had.
    Enable {
        #[arg(long, value_name = "ACCOUNT")]
        account: String,
    },
    /// Stop `--account` signing in. Takes effect on that user's NEXT call, not at
    /// the end of a page's session, because the server re-reads the principal per
    /// request. Its api keys are NOT revoked (a disable is reversible; killing a
    /// running mount is not) — `grovefs apikey revoke` is that verb.
    Disable {
        #[arg(long, value_name = "ACCOUNT")]
        account: String,
    },
    /// Delete `--account` and its delegations. Its api keys stay alive and
    /// unattributed on purpose, so a delete cannot silently stop a production
    /// mount; the reply counts them so the next command is obvious.
    Rm {
        #[arg(long, value_name = "ACCOUNT")]
        account: String,
    },
}

#[cfg(feature = "web")]
#[derive(Subcommand)]
enum WebGrantAction {
    /// Give `--account` management of base view `--base` — and of everything
    /// derived from it (lenses, aliases and forks resolve outward to their base),
    /// which is why this names a BASE and not a leaf. A `--base` that is a lens or
    /// a fork is stored against the base it resolves to and says so.
    Add {
        #[arg(long, value_name = "ACCOUNT")]
        account: String,
        #[arg(long, value_name = "VIEW")]
        base: String,
    },
    /// Withdraw one delegation. Effective on that account's next call.
    Rm {
        #[arg(long, value_name = "ACCOUNT")]
        account: String,
        #[arg(long, value_name = "VIEW")]
        base: String,
    },
    /// List delegations (`--account` to narrow to one person).
    List {
        #[arg(long, value_name = "ACCOUNT")]
        account: Option<String>,
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
/// recognized: GROVEFS_HOME, GROVEFS_META_SERVER / GROVEFS_DATA_SERVERS / GROVEFS_TOKEN
/// (mount defaults), GROVEFS_STRIPE_BYTES (mount stripe size), GROVEFS_STATS,
/// GROVEFS_API_KEY, GROVEFS_MCP_API_KEY, …
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

/// `--stripe-bytes` env fallback, same shape as [`resolve_bw`]: an explicit
/// flag wins; an unparsable env value is a hard error (silently ignoring a
/// stripe the operator believes is set would bake the wrong layout into every
/// new `data_ref`). When neither is given, the built-in default
/// (`backend::STRIPE_BYTES`, 16 MiB) is returned. A `0` value is refused by
/// the parser (a zero stripe is meaningless).
fn resolve_stripe(flag: Option<u64>, env_key: &str) -> Result<u64, Box<dyn std::error::Error>> {
    if let Some(v) = flag {
        return Ok(v);
    }
    match std::env::var(env_key) {
        Ok(s) if !s.trim().is_empty() => {
            grovefs::bwlimit::parse_size_bytes(&s).map_err(|e| format!("{env_key}: {e}").into())
        }
        _ => Ok(grovefs::backend::STRIPE_BYTES),
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
            "view '{view}' has no tree at {} — views are created explicitly; run `grovefs view --meta-dir {} create --view {view}` (or `grovefs --home {} init --view {view}`) first",
            mp.display(),
            base.display(),
            base.display()
        )
        .into());
    }
    Ok(MetaEngine::open(&mp)?)
}

/// Resolve the metadata backend URI for a CONTROL-plane command. The explicit
/// `--meta-store` flag wins; otherwise the `GROVEFS_META_STORE` env fills the default
/// (same flag > env chain as `--token`/`--home`). `None` / a value that is not a
/// `postgres[ql]://` URI means the local SQLite/file control plane; a
/// `postgres[ql]://` URI routes api_keys and view definitions to the shared
/// `grovefs_control` schema. This is what makes the control plane FOLLOW the
/// metadata backend — one system of record, never a local file a PG deployment
/// would silently ignore.
///
/// Both backends govern from one place: SQLite from `<meta-dir>` files, PG from
/// `grovefs_control` — so every meta-server/MCP/CLI on a deployment shares one
/// authoritative record. `MetaServer::serve()` still refuses to pair
/// `--meta-store postgres://…` with `--peer`/`--role standby`: GroveFS-native HA
/// snapshot shipping is SQLite-only, and PostgreSQL has its own replication.
fn resolve_store(explicit: Option<String>) -> Option<String> {
    explicit.or_else(|| std::env::var("GROVEFS_META_STORE").ok())
}

/// Resolve the admin credential for a `--control-server` call: the flag wins,
/// then `GROVEFS_CONTROL_KEY`, then `GROVEFS_API_KEY` (the same variable a mount
/// already uses, so one admin key can be exported once for a whole session).
fn resolve_control_key(explicit: Option<String>) -> Option<String> {
    explicit
        .or_else(|| std::env::var("GROVEFS_CONTROL_KEY").ok())
        .or_else(|| std::env::var("GROVEFS_API_KEY").ok())
        .filter(|k| !k.trim().is_empty())
}

/// Attach to a REMOTE governance entrance, or say exactly what is missing.
fn remote_control(
    addr: &str,
    key: Option<String>,
) -> Result<grovefs::control_remote::RemoteControlStore, Box<dyn std::error::Error>> {
    let key = key.ok_or_else(|| {
        format!(
            "--control-server {addr} needs an admin account key: pass --api-key '<id>:<secret>' \
             (or export GROVEFS_CONTROL_KEY). The first one is minted ON THE SERVER HOST with \
             `grovefs apikey --meta-dir <server-meta-dir> create --for admin --owner <account>`."
        )
    })?;
    Ok(grovefs::control_remote::RemoteControlStore::connect(
        addr, &key,
    )?)
}

/// The control plane a governance command should talk to: the remote one when
/// `--control-server` is given, else the local/`--meta-store` implementation.
/// A remote store is bound to the store ITS server owns — a local
/// `--meta-dir`/`--meta-store` beside `--control-server` describes a different
/// one and is refused by [`refuse_local_with_remote`], not merged.
fn control_store_for_target(
    vbase: &Path,
    store: Option<&str>,
    control_server: Option<&str>,
    control_key: Option<String>,
) -> Result<Arc<dyn grovefs::mcp::ControlStore>, Box<dyn std::error::Error>> {
    match control_server {
        Some(addr) => Ok(Arc::new(remote_control(addr, control_key)?)),
        None => Ok(grovefs::mcp::control_store_for(vbase, store)?),
    }
}

/// A governance command reaches ONE store. Asking for the remote control plane
/// and a local store at the same time would leave it ambiguous which one is
/// authoritative, so the combination is refused by name instead of silently
/// preferring one (the admin key alone decides what a remote call governs).
fn refuse_local_with_remote(
    addr: &str,
    db_path: &Option<PathBuf>,
    store: &Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    if db_path.is_some() || store.is_some() {
        return Err(format!(
            "--control-server {addr} cannot be combined with --meta-dir/--meta-store: a remote \
             control plane governs the store ITS server owns, so a local path here would describe \
             a different store. Drop the local flags — the account key alone decides what is governed."
        )
        .into());
    }
    Ok(())
}

/// The first line of a secret file, stripped of its newline. Every credential
/// this CLI can take from a FILE instead of from argv goes through here: a key on
/// a command line is visible to every local process (`ps`, the journal, a shell
/// history), which is a strictly worse exposure than the `0600` file a unit
/// already reads.
#[cfg(feature = "web")]
fn read_secret_file(path: &PathBuf, what: &str) -> Result<String, Box<dyn std::error::Error>> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("cannot read {what} {}: {e}", path.display()))?;
    secret_line(&text, what)
}

/// One secret out of the first line of what was read. Empty is refused HERE on
/// purpose: an empty password would come back as the server's policy refusal
/// ("at least 10 characters"), which hides the actual mistake — that nothing was
/// piped in.
#[cfg(feature = "web")]
fn secret_line(text: &str, what: &str) -> Result<String, Box<dyn std::error::Error>> {
    let line = text.lines().next().unwrap_or("").to_string();
    if line.is_empty() {
        return Err(format!("{what} has no secret in its first line").into());
    }
    Ok(line)
}

/// A password for `web account create|passwd`: from a file or from stdin, never
/// from argv and never invented here — a placeholder generated by the CLI is the
/// same leak with extra steps.
#[cfg(feature = "web")]
fn read_password(
    file: Option<&PathBuf>,
    stdin: bool,
) -> Result<String, Box<dyn std::error::Error>> {
    let text = match (file, stdin) {
        (Some(p), false) => std::fs::read_to_string(p)
            .map_err(|e| format!("cannot read --password-file {}: {e}", p.display()))?,
        (None, true) => {
            use std::io::Read;
            let mut s = String::new();
            let n = std::io::stdin()
                .read_to_string(&mut s)
                .map_err(|e| format!("cannot read the password from stdin: {e}"))?;
            if n == 0 {
                return Err("--password-stdin received nothing: pipe the secret in (a password \n                        manager, a file) rather than running this from a terminal.".into());
            }
            s
        }
        (Some(_), true) => {
            return Err("--password-file and --password-stdin name two sources for one password.".into())
        }
        (None, false) => {
            return Err(
                "a password comes from --password-file PATH or --password-stdin: on a command line \n             it is visible to every local process and lands in the shell history. The length and \n             composition rules are the server's and are reported verbatim."
                    .into(),
            )
        }
    };
    secret_line(&text, "the password")
}

/// One string/number/bool field of a control reply as a table cell (`-` when the
/// server omitted it or sent null, so an optional column cannot panic the CLI).
#[cfg(feature = "web")]
fn jfield(v: &serde_json::Value, f: &str) -> String {
    match v.get(f) {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Bool(b)) => b.to_string(),
        Some(serde_json::Value::Number(n)) => n.to_string(),
        _ => "-".to_string(),
    }
}

/// A string-array field of a control reply, joined for one cell.
#[cfg(feature = "web")]
fn jlist(v: &serde_json::Value, f: &str) -> String {
    v.get(f)
        .and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

/// A reply that is a list of rows.
#[cfg(feature = "web")]
fn jrows(v: &serde_json::Value) -> Vec<serde_json::Value> {
    v.as_array().cloned().unwrap_or_default()
}

/// Run one `grovefs web account|grant` verb over the control plane and print the
/// reply. Each is ONE call, and every rule behind it stays the server's — the
/// password policy, the last-enabled-admin guard, the base a grant on a lens
/// resolves to. Formatting is the only thing this function owns, which is the
/// same argument the web gateway makes about the ACL: a second implementation of
/// a governance rule is the one nobody audits.
#[cfg(feature = "web")]
fn web_govern(
    c: &grovefs::control_remote::ControlClient,
    action: WebAction,
) -> Result<(), Box<dyn std::error::Error>> {
    use serde_json::json;
    let call = |op: &str, a: serde_json::Value| -> Result<serde_json::Value, Box<dyn std::error::Error>> {
        c.call(op, a)
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })
    };
    // The address echoed into every "do this next" line, so a copy-paste of the
    // hint is a working command rather than one the operator has to complete.
    let at = c.addr();
    match action {
        WebAction::Account(WebAccountAction::Create {
            account,
            password_file,
            password_stdin,
            role,
        }) => {
            let password = read_password(password_file.as_ref(), password_stdin)?;
            let r = call(
                "account.create",
                json!({"account": account, "password": password, "role": role}),
            )?;
            let made = jfield(&r, "account");
            println!("web account created: {made}");
            println!("  role:  {}", jfield(&r, "role"));
            let bases = jlist(&r, "bases");
            println!(
                "  bases: {}",
                if bases.is_empty() {
                    "none — it can sign in and govern nothing yet"
                } else {
                    &bases
                }
            );
            if let Some(note) = r.get("note").and_then(|v| v.as_str()) {
                println!("  note:  {note}");
            }
            println!();
            println!("Delegate a base view (a user governs that tree and everything derived from it):");
            println!("  grovefs web --control-server {at} grant add --account {made} --base VIEW");
        }
        WebAction::Account(WebAccountAction::List) => {
            let rows = jrows(&call("account.list", json!({}))?);
            if rows.is_empty() {
                println!(
                    "no web accounts in the control plane at {at}. The first one is seeded at install\ntime while the store is empty (`meta-server --admin-user alice --admin-password-file PATH`);\nafter that they come from the console's Accounts page or from `grovefs web account create`."
                );
                return Ok(());
            }
            println!(
                "{:<20} {:<6} {:<22} {:<22} {:<22} {}",
                "account", "role", "created", "last sign-in", "disabled", "base views governed"
            );
            for r in &rows {
                let mut bases = jlist(r, "bases");
                if bases.is_empty() {
                    bases = if jfield(r, "role") == "admin" {
                        "<all: an admin is not bounded by grants>".to_string()
                    } else {
                        "-".to_string()
                    };
                }
                println!(
                    "{:<20} {:<6} {:<22} {:<22} {:<22} {}",
                    jfield(r, "account"),
                    jfield(r, "role"),
                    jfield(r, "created_at"),
                    jfield(r, "last_login_at"),
                    jfield(r, "disabled_at"),
                    bases
                );
            }
        }
        WebAction::Account(WebAccountAction::Passwd {
            account,
            password_file,
            password_stdin,
        }) => {
            let password = read_password(password_file.as_ref(), password_stdin)?;
            let r = call(
                "account.passwd",
                json!({"account": account, "password": password}),
            )?;
            println!("password changed for {}.", jfield(&r, "account"));
            println!("A browser session already signed in keeps its session until it goes idle (the\ngateway's own table, not a credential); `account disable` is the verb that stops a person\non their next call.");
        }
        WebAction::Account(WebAccountAction::Role { account, role }) => {
            let r = call("account.role", json!({"account": account, "role": role}))?;
            println!("{} is now role {}", jfield(&r, "account"), jfield(&r, "role"));
            if jfield(&r, "role") == "admin" {
                println!("An admin governs the whole deployment: every view, every key, every account. Its\nown grants are not read, so delegate a `user` (`grant add`) rather than raising a role\nto solve a missing grant.");
            }
        }
        WebAction::Account(WebAccountAction::Enable { account }) => {
            let r = call(
                "account.disable",
                json!({"account": account, "disabled": false}),
            )?;
            println!("{} can sign in again.", jfield(&r, "account"));
        }
        WebAction::Account(WebAccountAction::Disable { account }) => {
            let r = call(
                "account.disable",
                json!({"account": account, "disabled": true}),
            )?;
            println!("{} cannot sign in.", jfield(&r, "account"));
            println!("Effective on that user's NEXT call (the server re-reads the principal per request); its\ndelegations are untouched, so `account enable` returns exactly this reach. Its API keys are\nNOT revoked — a disable is reversible and stopping a live mount is not, so revoking one\nis a separate, deliberate verb: `grovefs apikey --control-server {at} revoke --id <ID>`.");
        }
        WebAction::Account(WebAccountAction::Rm { account }) => {
            let r = call("account.delete", json!({"account": account}))?;
            println!("web account {} deleted.", jfield(&r, "account"));
            println!("  {}", jfield(&r, "note"));
        }
        WebAction::Grant(WebGrantAction::Add { account, base }) => {
            let r = call("grant.add", json!({"account": account, "base": base}))?;
            let got = jfield(&r, "base");
            if r.get("asNamed").and_then(|v| v.as_bool()) == Some(false) {
                println!("'{base}' resolves to base view '{got}' — the delegation is stored on that (a grant\non a lens would be a row the ACL can never match).");
            }
            println!(
                "{} now governs '{got}' and everything derived from it, as granted by {}.",
                jfield(&r, "account"),
                jfield(&r, "grantedBy")
            );
            println!("That covers its forks, aliases and lenses, and the keys bound to any of those names.");
        }
        WebAction::Grant(WebGrantAction::Rm { account, base }) => {
            let r = call("grant.remove", json!({"account": account, "base": base}))?;
            println!(
                "{}",
                if r.get("removed").and_then(|v| v.as_bool()) == Some(true) {
                    format!("{} no longer governs '{}'.", jfield(&r, "account"), jfield(&r, "base"))
                } else {
                    format!(
                        "there was no such delegation: {} did not govern '{}'.",
                        jfield(&r, "account"),
                        jfield(&r, "base")
                    )
                }
            );
        }
        WebAction::Grant(WebGrantAction::List { account }) => {
            let arg = match &account {
                Some(a) => json!({"account": a}),
                None => json!({}),
            };
            let rows = jrows(&call("grant.list", arg)?);
            if rows.is_empty() {
                println!(
                    "no delegations{}

An account with role `user` governs nothing until it is granted a base view:
  grovefs web --control-server {at} grant add --account alice --base mybase",
                    match &account {
                        Some(a) => format!(" for '{a}'"),
                        None => String::new(),
                    }
                );
                return Ok(());
            }
            println!(
                "{:<20} {:<24} {:<8} {:<22} {}",
                "account", "base view", "perm", "granted", "by"
            );
            for r in &rows {
                println!(
                    "{:<20} {:<24} {:<8} {:<22} {}",
                    jfield(r, "account"),
                    jfield(r, "base_view"),
                    jfield(r, "perm"),
                    jfield(r, "granted_at"),
                    jfield(r, "granted_by")
                );
            }
        }
    }
    Ok(())
}

/// Undo a just-imported tree whose `fork <base>` definition could not be
/// recorded. The tree exists on whichever backend the store names, so the undo
/// has to be backend-aware: a `<view>.db` file or a PG schema.
/// Bytes are never touched — they are still named by the source tree, and the
/// ledger follows the trees that exist.
fn rollback_imported_tree(vbase: &Path, store: Option<&str>, view: &str) -> String {
    let uri = store.unwrap_or("");
    if grovefs::pg_store::is_pg_store(uri) {
        return match grovefs::pg_store::PgEngine::remove_view(uri, view) {
            Ok(_) => "the imported tree was removed again".to_string(),
            Err(e) => {
                format!("the imported tree could NOT be removed ({e}) — drop its schema by hand")
            }
        };
    }
    for ext in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(vbase.join("meta").join(format!("{view}.db{ext}")));
    }
    let _ = meta::rebuild_ledger(vbase);
    "the imported tree was removed again".to_string()
}

/// Whichever key registry an `apikey` command acts on: the store THIS machine
/// opens (`--meta-dir`/`--meta-store`), or the registry inside a remote
/// meta-server reached through its governance port. An account's keys are
/// guarded by the surface that installs them, so the server that owns the
/// store also owns the mint/revoke — the operator's machine holds only the
/// admin credential, never the store.
///
/// Every verb is one of these five, so the ID resolution and the printing above
/// stay single-sourced instead of being duplicated per transport.
enum KeyAdmin {
    Local(grovefs::apikeys::ApiKeyStore),
    Remote(grovefs::control_remote::ControlClient),
}

impl KeyAdmin {
    fn list(&self) -> Result<Vec<grovefs::apikeys::KeyInfo>, String> {
        match self {
            Self::Local(k) => k.list(),
            Self::Remote(c) => {
                let rows = c.call("apikey.list", serde_json::json!({}))?;
                let arr = rows.as_array().ok_or_else(|| {
                    "malformed control reply: apikey.list is not an array".to_string()
                })?;
                let s = |v: &serde_json::Value, f: &str| -> Result<String, String> {
                    v.get(f)
                        .and_then(|x| x.as_str())
                        .map(str::to_string)
                        .ok_or_else(|| format!("malformed control reply: key row without '{f}'"))
                };
                let opt = |v: &serde_json::Value, f: &str| -> Option<String> {
                    v.get(f).and_then(|x| x.as_str()).map(str::to_string)
                };
                arr.iter()
                    .map(|r| {
                        Ok(grovefs::apikeys::KeyInfo {
                            id: s(r, "id")?,
                            owner: opt(r, "owner"),
                            label: s(r, "label")?,
                            scope: s(r, "scope")?,
                            view: opt(r, "view"),
                            ro: r.get("ro").and_then(|x| x.as_bool()).unwrap_or(false),
                            created_at: s(r, "created_at")?,
                            last_used_at: opt(r, "last_used_at"),
                            revoked_at: opt(r, "revoked_at"),
                            note: opt(r, "note"),
                            expires_at: opt(r, "expires_at"),
                            // The SERVER's verdict — this CLI is a client, and a
                            // client comparing timestamp strings across timezones
                            // is how a key looks live when it is not.
                            expired: r.get("expired").and_then(|x| x.as_bool()).unwrap_or(false),
                            created_by: opt(r, "created_by"),
                            secret_hint: opt(r, "secret_hint"),
                        })
                    })
                    .collect()
            }
        }
    }

    /// Mint a key. `ttl` is a duration spelling (`7d`) resolved to a deadline by
    /// whichever side OWNS the clock that will enforce it: the server on the
    /// remote path (and its answer comes back in the reply), this process on the
    /// local path. Returns `(id, plaintext, deadline)`, the deadline `None` for a
    /// key that never expires. `created_by` is deliberately NOT a parameter: the
    /// remote arm lets the server stamp it from the AUTHENTICATED account (a
    /// client naming its own minter is a claim, not a record), and the local arm
    /// has no identity to record — it is the store itself.
    fn create(
        &self,
        label: &str,
        scope: grovefs::apikeys::Scope,
        view: Option<&str>,
        ro: bool,
        owner: Option<&str>,
        note: Option<&str>,
        ttl: Option<&str>,
    ) -> Result<(String, String, Option<String>), String> {
        match self {
            Self::Local(k) => {
                let expires = grovefs::apikeys::expires_from_ttl(ttl.unwrap_or(""))?;
                let (id, full) = k.create_extended(
                    label,
                    scope,
                    view,
                    ro,
                    owner,
                    note,
                    expires.as_deref(),
                    None,
                )?;
                Ok((id, full, expires))
            }
            // Closes the credential loop: a key for the ADMINISTRATOR of one
            // deployment is itself minted over that deployment's control plane,
            // so a second operator never needs the server's store either.
            Self::Remote(c) => {
                let r = c.call(
                    "apikey.create",
                    serde_json::json!({
                        "label": label,
                        "scope": scope.as_str(),
                        "view": view,
                        "ro": ro,
                        "owner": owner,
                        "note": note,
                        "ttl": ttl,
                    }),
                )?;
                let id = r.get("id").and_then(|v| v.as_str()).ok_or_else(|| {
                    "malformed control reply: apikey.create without an id".to_string()
                })?;
                let key = r.get("key").and_then(|v| v.as_str()).ok_or_else(|| {
                    "malformed control reply: apikey.create without a key".to_string()
                })?;
                let expires = r
                    .get("expires_at")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                Ok((id.to_string(), key.to_string(), expires))
            }
        }
    }

    /// Edit one ACTIVE key's remark and/or deadline (`apikey update`). `ttl`
    /// follows [`Self::create`]'s clock rule; `None` leaves the deadline alone
    /// and `"never"` removes it. Errors name the row that refused, which on the
    /// remote path is the server's own sentence about ACL and revocation.
    fn update(&self, id: &str, note: Option<&str>, ttl: Option<&str>) -> Result<(), String> {
        match self {
            Self::Local(k) => {
                let expires = match ttl {
                    Some(t) => Some(grovefs::apikeys::expires_from_ttl(t)?),
                    None => None,
                };
                if k.update_fields(id, note, expires.as_ref().map(Option::as_deref))? {
                    Ok(())
                } else {
                    Err(format!(
                        "no ACTIVE api key with id '{id}' — the id is unknown, or the key is revoked \
                         (revoke is final; mint a new one)"
                    ))
                }
            }
            Self::Remote(c) => {
                c.call(
                    "apikey.update",
                    serde_json::json!({ "id": id, "note": note, "ttl": ttl }),
                )?;
                Ok(())
            }
        }
    }

    fn revoke(&self, id: &str) -> Result<bool, String> {
        match self {
            Self::Local(k) => k.revoke(id),
            Self::Remote(c) => Ok(c
                .call("apikey.revoke", serde_json::json!({ "id": id }))?
                .as_bool()
                .unwrap_or(false)),
        }
    }

    /// Replace an active key's secret; like `create`, yields the new plaintext
    /// for exactly one display (nothing here ever persists it).
    fn rotate(&self, id: &str) -> Result<Option<String>, String> {
        match self {
            Self::Local(k) => k.rotate(id),
            Self::Remote(c) => {
                let r = c.call("apikey.rotate", serde_json::json!({ "id": id }))?;
                Ok(r.get("key").and_then(|v| v.as_str()).map(str::to_string))
            }
        }
    }

    fn set_ro(&self, id: &str, ro: bool) -> Result<bool, String> {
        match self {
            Self::Local(k) => k.set_ro(id, ro),
            Self::Remote(c) => Ok(c
                .call("apikey.set_ro", serde_json::json!({ "id": id, "ro": ro }))?
                .as_bool()
                .unwrap_or(false)),
        }
    }

    fn recent_calls(&self, limit: usize) -> Result<Vec<grovefs::apikeys::CallLog>, String> {
        match self {
            Self::Local(k) => k.recent_calls(limit),
            Self::Remote(c) => {
                let rows = c.call("apikey.calls", serde_json::json!({ "limit": limit }))?;
                let arr = rows.as_array().ok_or_else(|| {
                    "malformed control reply: apikey.calls is not an array".to_string()
                })?;
                let opt = |v: &serde_json::Value, f: &str| -> Option<String> {
                    v.get(f).and_then(|x| x.as_str()).map(str::to_string)
                };
                Ok(arr
                    .iter()
                    .map(|r| grovefs::apikeys::CallLog {
                        ts: opt(r, "ts").unwrap_or_default(),
                        key_id: opt(r, "key_id"),
                        key_label: opt(r, "key_label"),
                        method: opt(r, "method").unwrap_or_default(),
                        tool: opt(r, "tool").unwrap_or_default(),
                        ok: r.get("ok").and_then(|x| x.as_bool()).unwrap_or(false),
                        detail: opt(r, "detail").unwrap_or_default(),
                    })
                    .collect())
            }
        }
    }
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

fn main() {
    // Rust's default `Termination` for `fn main() -> Result<(), Box<dyn Error>>`
    // prints the Err via Debug, so a failed operation surfaced as
    // `Error: Os { code: 111, kind: ConnectionRefused, message: .. }` and a
    // `String`/`to_string` error came out wrapped in quotes
    // (`Error: "..."`). Neither is readable or greppable (§5-12). Route every
    // fatal error through Display with a stable `error: ` prefix, matching
    // clap's own stderr wording, and preserve the non-zero exit status.
    let rc = match real_main() {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    };
    // Say everything before the network closes: the text above is what an
    // operator and the test board read, and a refused command is judged by it.
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    std::process::exit(rc);
}

fn real_main() -> Result<(), Box<dyn std::error::Error>> {
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
    // (no `--meta-server`). Those create it in their own branches below. A fully
    // remote mount (`--data-servers` + `--meta-server`) writes nothing under `--home` — no
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
            println!("initialized BASE view '{view}' at {}", mp.display());
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
            stripe_bytes,
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
            let data = data.or_else(|| std::env::var("GROVEFS_DATA_SERVERS").ok());
            let meta = meta.or_else(|| std::env::var("GROVEFS_META_SERVER").ok());
            let token = token.or_else(|| std::env::var("GROVEFS_TOKEN").ok());
            // Content rate caps: explicit flag > live env/config-file knob;
            // an unparsable env value refuses the mount (see `resolve_bw`).
            let bw_read = resolve_bw(bw_read, "GROVEFS_BW_READ")?;
            let bw_write = resolve_bw(bw_write, "GROVEFS_BW_WRITE")?;
            // Stripe size for cross-server striping: same precedence, the
            // built-in default when neither knob is set (see `resolve_stripe`).
            let stripe = resolve_stripe(stripe_bytes, "GROVEFS_STRIPE_BYTES")?;
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
            // `--run-as-uid` overrides the process euid (advisory).
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
                    "note: --api-key is only presented to a remote --meta-server/--data-servers server; a local mount is gated by OS permissions, so it is ignored here"
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
            // Metadata source: a central `meta-server` when --meta-server is given (a
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
            // for `--layout` + `--meta-server`, the lens translates source-coordinate
            // events into display coordinates at the kernel notifier.
            let provider_for_events: Option<Arc<dyn MetaProvider>> = if meta.is_some() {
                Some(meta_provider.clone())
            } else {
                None
            };
            let fence_refresh: Arc<dyn Fn() -> u64 + Send + Sync> =
                Arc::new(move || meta_for_fence.fetch_fence_epoch());
            // Pick the data path: remote over our TCP transport (no NFS) when
            // --data-servers is given, otherwise the local disk backend. The FUSE and
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
                            "using sharded remote data service across {} servers (stripe {} bytes): {}",
                            addrs.len(),
                            stripe,
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
                        Arc::new(backend::ShardedBackend::new(members, stripe))
                    } else {
                        // A single data-server never stripes, so the knob is
                        // inert here — say so rather than let a config-file
                        // default silently vanish on a deployment that shrinks
                        // the server list. Not a hard error: the same conf is
                        // meant to survive across multi- and single-server tops.
                        if stripe_bytes.is_some()
                            || std::env::var_os("GROVEFS_STRIPE_BYTES").is_some()
                        {
                            eprintln!(
                                "note: --stripe-bytes / GROVEFS_STRIPE_BYTES has no effect with a single --data-servers address (striping needs a comma list)"
                            );
                        }
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
            let meta_for_pool = meta_provider.clone();
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
            // The clone ioctl (FUSE_DEV_IOC_CLONE) is Linux 4.5+, and fuser
            // propagates its failure out of `Session::run()` instead of falling
            // back — so on an older kernel asking for cloning would make the mount
            // die, not degrade. Probe the running kernel release first and drop to
            // a single worker (with a visible note) when the ioctl is unavailable.
            // An unreadable / unparseable osrelease (non-Linux, exotic string) is
            // treated as supported so a normal mount never loses parallelism.
            let mut clone_ok = true;
            if threads > 1 {
                if let Ok(s) = std::fs::read_to_string("/proc/sys/kernel/osrelease") {
                    let mut it = s.trim().split('.');
                    let major = it.next().and_then(|x| x.parse::<u32>().ok());
                    let minor = it.next().and_then(|x| x.parse::<u32>().ok());
                    if let (Some(maj), Some(mnr)) = (major, minor) {
                        clone_ok = maj > 4 || (maj == 4 && mnr >= 5);
                    }
                }
            }
            let effective_threads = if threads > 1 && !clone_ok {
                eprintln!(
                    "note: running kernel < 4.5 lacks FUSE_DEV_IOC_CLONE; using a single FUSE worker thread (was --threads {threads})"
                );
                1
            } else {
                threads
            };
            cfg.n_threads = if effective_threads > 1 {
                Some(effective_threads)
            } else {
                None
            };
            cfg.clone_fd = effective_threads > 1;
            // Match the metadata read pool to the dispatch width: at most
            // `effective_threads` requests are ever in flight, so a narrower pool
            // makes every surplus read OPEN a SQLite connection on the spot. That
            // was measured as the wall in front of parallel small-file creation
            // (one view, 16 workers: 2.6k -> 6.3k files/s). The env knob still
            // wins when set, so the pool stays tunable per deployment.
            if std::env::var_os("GROVEFS_SQLITE_READ_POOL").is_none() {
                meta_for_pool.set_read_pool(effective_threads.max(1) as usize);
            }
            // Only print `base` when the mount actually reads/writes under it:
            // a fully remote mount (`--data-servers` + `--meta-server`) touches no local store
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
            // coherence without `--no-cache`: pushing kernel-cache invalidations
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
            member_id,
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
                return Err("data-server --data-dir: no usable path".into());
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
            // backend: a `--meta-store postgres://…` (or `GROVEFS_META_STORE`) data-server
            // gates mounts against the shared `grovefs_control` tables — the SAME
            // registry its meta-server uses — with no local file to colocate; a
            // SQLite node reads `<meta-dir>/meta/api_keys.db` (defaulting to
            // this node's first `--data-dir`, or a synced copy for a split one).
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
            // Startup banner echoes the root list + host (display only): root
            // paths repeat across nodes, so the listen line alone is ambiguous.
            let srv = srv.with_roots(&roots);
            // Stable member id for identity-based striping: flag > env >
            // unconfigured. A malformed `GROVEFS_MEMBER_ID` is a HARD error, not
            // a silent drop to positional placement — quietly losing the id an
            // operator believes is set would reintroduce the reorder hazard this
            // feature exists to remove.
            let member_id = match member_id {
                Some(v) => Some(v),
                None => match std::env::var("GROVEFS_MEMBER_ID") {
                    Ok(s) if !s.trim().is_empty() => {
                        Some(s.trim().parse::<u64>().map_err(|_| {
                            format!("GROVEFS_MEMBER_ID: invalid integer '{}'", s.trim())
                        })?)
                    }
                    _ => None,
                },
            };
            let srv = match member_id {
                Some(id) => {
                    println!(
                        "data-server: member-id {id} — clients that stripe across a comma \
                         --data-servers list place chunks by this identity, so their order \
                         is irrelevant (must be unique across the cluster's members)"
                    );
                    srv.with_member_id(id)
                }
                None => srv,
            };
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
            engine_idle_secs,
            pg_write_pool,
            pg_write_pool_max,
            pg_read_pool,
            pg_read_pool_max,
            pg_pool_idle_secs,
            control_listen,
            admin_user,
            admin_password_file,
        } => {
            let role = if role.eq_ignore_ascii_case("standby") {
                grovefs::remote::meta::Role::Standby
            } else {
                grovefs::remote::meta::Role::Primary
            };
            // Backend for BOTH the view trees and the control plane (view
            // definitions + api_keys): `--meta-store` > `GROVEFS_META_STORE` > SQLite.
            let store = resolve_store(store);
            // `--meta-dir` is the explicit, self-describing form of the metadata
            // root; the global `--home` is the default and loses if both are
            // given. A `GROVEFS_META_DIR` env default slots between them,
            // and `GROVEFS_TOKEN` fills a missing `--token` (same flag > env >
            // config-file chain as the mount side).
            let token = token.or_else(|| std::env::var("GROVEFS_TOKEN").ok());
            let db_path =
                db_path.or_else(|| std::env::var("GROVEFS_META_DIR").ok().map(PathBuf::from));
            let meta_base = db_path.unwrap_or_else(|| base.clone());
            let mut srv = match token.as_deref() {
                Some(t) if !t.is_empty() => {
                    grovefs::remote::MetaServer::with_token(&listen, &meta_base, t)
                }
                _ => grovefs::remote::MetaServer::new(&listen, &meta_base),
            };
            // Same registry `grovefs mcp --meta-dir` and `grovefs apikey --meta-dir`
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
            // PG connection-pool bounds: flags over env over defaults, applied
            // BEFORE the first engine opens (engines open lazily per view on
            // M_OPEN, so this is the last safe point). Calling it under a
            // SQLite store is inert — no PG engine is ever constructed — so no
            // branch guards the set; only the banner is store-aware.
            grovefs::pg_store::set_pool_profile(
                pg_write_pool,
                pg_write_pool_max,
                pg_read_pool,
                pg_read_pool_max,
                pg_pool_idle_secs,
            );
            let store_is_pg = store
                .as_deref()
                .is_some_and(grovefs::pg_store::is_pg_store);
            srv = srv.with_store(store);
            if store_is_pg {
                let (w, r, idle) = grovefs::pg_store::pool_profile_now();
                println!(
                    "meta-server: PG pool bounds per view = write {} warm/{} max, read {} warm/{} max, burst above the floor reaped after {}s idle (a view may override with `grovefs view pool`); plan max_connections as active trees x (floor + retained burst)",
                    w.warm, w.max, r.warm, r.max, idle / 1000
                );
            }
            srv = srv.with_engine_idle_secs(engine_idle_secs);
            // The install-time web owner, if the flags ask for one. Read here and
            // handed to `serve()` as (account, password) — the digest is written
            // by the process that owns the account store, and this value is then
            // dropped rather than kept in the runtime.
            let admin = match (&admin_user, &admin_password_file) {
                (None, None) => None,
                (Some(_), None) => {
                    return Err("--admin-user needs --admin-password-file PATH: a password has no \n                        place on a command line (argv is visible to every local process and \n                        lands in the shell history)".into())
                }
                (None, Some(_)) => {
                    return Err("--admin-password-file needs --admin-user: an owner without a name \n                        cannot be created".into())
                }
                (Some(account), Some(path)) => {
                    if control_listen.is_none() {
                        return Err(format!(
                            "--admin-user needs --control-listen HOST:PORT: the web accounts live \n                             beside the control plane, and without that port nothing can verify a sign-in"
                        )
                        .into())
                    }
                    let text = std::fs::read_to_string(path)
                        .map_err(|e| format!("cannot read --admin-password-file {}: {e}", path.display()))?;
                    let password = text.lines().next().unwrap_or("").to_string();
                    if password.is_empty() {
                        return Err(format!(
                            "--admin-password-file {} is empty (its first line is the password)",
                            path.display()
                        )
                        .into())
                    }
                    // Never print the secret, and never let it survive past this
                    // statement's scope in a variable the banner could echo.
                    drop(text);
                    Some((account.clone(), password))
                }
            };
            srv = srv.with_control_listen(control_listen);
            srv = srv.with_admin_account(
                admin.as_ref().map(|(a, _)| a.clone()),
                admin.map(|(_, p)| p),
            );
            srv.serve()?;
        }
        Cmd::Umount { mountpoint } => {
            run_fusermount(&["-u", &mountpoint.to_string_lossy()])?;
            println!("unmounted {}", mountpoint.display());
        }
        Cmd::View {
            db_path,
            store,
            control_server,
            control_key,
            action,
        } => {
            // View definitions are CONTROL-PLACE state and FOLLOW the metadata
            // backend: under SQLite they are `meta/views/*.layout` files beside
            // `api_keys.db`; under a `--meta-store postgres://…` they are the shared
            // `grovefs_control.view_defs` rows the meta-server reads at M_OPEN.
            // The governance ops below run against ONE `ControlStore` so the CLI
            // and the MCP server can never drift per backend. Base-view
            // creation and export are PHYSICAL-tree operations (a local db file)
            // and stay file-based.
            if let Some(addr) = control_server.as_deref() {
                refuse_local_with_remote(addr, &db_path, &store)?;
            }
            let vbase = db_path.unwrap_or_else(|| base.clone());
            let store = resolve_store(store);
            let store_is_pg = store.as_deref().is_some_and(grovefs::pg_store::is_pg_store);
            // The same `ControlStore` the MCP server drives — except that with
            // `--control-server` it is a thin client of the meta-server's
            // governance port, so the definitions/aliases of a PG *or SQLite*
            // deployment are governed from any machine with an admin key.
            let ctrl = control_store_for_target(
                &vbase,
                store.as_deref(),
                control_server.as_deref(),
                resolve_control_key(control_key.clone()),
            )?;
            let remote_ctl = control_server.as_deref().map(|a| {
                let key = resolve_control_key(control_key).unwrap_or_default();
                grovefs::control_remote::ControlClient::new(a, &key)
            });
            // A PG deployment has no local per-view db file: a base tree is a
            // `grovefs_v_<hex(view)>` schema that `view create` provisions
            // explicitly, and its bytes move through a mount — so the file-level
            // physical ops below are refused rather than silently writing a
            // local artifact the PG meta-server never reads.
            let require_sqlite = |op: &str| -> Result<(), Box<dyn std::error::Error>> {
                if store_is_pg {
                    Err(format!(
                        "{op} operates on a local SQLite view db; under `--meta-store postgres://…` a base tree is a PG schema (create one with `view create`) and its bytes move through a mounted path — drop --meta-store for a local store"
                    )
                    .into())
                } else {
                    Ok(())
                }
            };
            match action {
                ViewAction::Create {
                    view,
                    manifest,
                    pg_instance,
                } => {
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
                    if let Some(rc) = &remote_ctl {
                        // The manifest is a FILE here but only TEXT to the
                        // server: the tree it seeds lives on the server's store,
                        // so the lines travel and the seeding happens there.
                        let mut args = serde_json::json!({ "view": view });
                        if let Some(pgi) = pg_instance.as_deref() {
                            args["pg_instance"] = serde_json::json!(pgi);
                        }
                        if let Some(mf) = manifest.as_ref() {
                            let text = std::fs::read_to_string(mf).map_err(|e| {
                                format!("cannot read manifest {}: {e}", mf.display())
                            })?;
                            args["manifest"] = serde_json::json!(text);
                        }
                        let r = rc
                            .call("view.create", args)
                            .map_err(Box::<dyn std::error::Error>::from)?;
                        println!("{} (via {})", r.as_str().unwrap_or("created"), rc.addr());
                    } else if store_is_pg {
                        // A PG base view is one `grovefs_v_<hex(view)>` schema.
                        // `PgEngine::open` creates the schema, its three tables
                        // and the root row; dropping the handle leaves the tree
                        // in place for mounts to open.
                        if manifest.is_some() {
                            return Err("`view create --manifest` seeds a SQLite db file; under `--meta-store postgres://…` mount the view and write the files instead".into());
                        }
                        let spec = store.as_deref().expect("store_is_pg implies a store spec");
                        // Place BEFORE the first open: the home row decides which
                        // instance every later resolve binds this view to. An
                        // explicit `--pg-instance` pins; otherwise least-loaded.
                        let (home, _) = grovefs::pg_store::place_new_view(
                            spec,
                            &view,
                            pg_instance.as_deref(),
                        )?;
                        drop(grovefs::pg_store::PgEngine::open(spec, &view, false)?);
                        println!(
                            "created BASE view '{view}' (schema grovefs_v_<hex>) on instance '{home}'"
                        );
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
                    // Named against the SAME entrance this listing came from: a
                    // remote store's keys are behind the control port, not in a
                    // local `--meta-dir` directory.
                    match &remote_ctl {
                        Some(rc) => println!(
                            "view keys: `grovefs apikey --control-server {} --api-key <admin-key> list [--view NAME]` (add --view to see only that view's keys)",
                            rc.addr()
                        ),
                        None => println!(
                            "view keys: `grovefs apikey --meta-dir {} list [--view NAME]` (add --view to see only that view's keys)",
                            vbase.display()
                        ),
                    }
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
                    // then byte reclaim through the store's data root. The guards
                    // below read the LOCAL control plane and this machine's mount
                    // table, so they are backend-independent and run before the
                    // backend split: a PG removal is refused for exactly the
                    // same reasons a SQLite one is.
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
                    if let Some(rc) = &remote_ctl {
                        // The SERVER runs the same guards (definition dependent,
                        // its own mount table) and owns the store whose tree and
                        // ledger this teardown rewrites — so the whole operation
                        // is one call, not a local half that would leave the
                        // remote ledger over-counting.
                        let mut args = serde_json::json!({ "view": name });
                        if let Some(dr) = &data_root {
                            args["data_root"] = serde_json::json!(dr.display().to_string());
                        }
                        let r = rc
                            .call("view.rm_base", args)
                            .map_err(Box::<dyn std::error::Error>::from)?;
                        println!("{}", r.as_str().unwrap_or("removed"));
                        return Ok(());
                    }
                    if store_is_pg {
                        // A PG base view IS a `grovefs_v_<hex>` schema and the
                        // cross-view ledger is `grovefs_refs.holders`, both in the
                        // database — so teardown is "unbook, then drop" inside ONE
                        // transaction (PostgreSQL's DDL is transactional). There is
                        // no local file to unlink and no rebuild pass to run
                        // afterwards: the schema drop and its holds cannot disagree,
                        // even across a crash.
                        let uri = store.as_deref().expect("store_is_pg implies a URI");
                        let rep = grovefs::pg_store::PgEngine::remove_view(uri, &name)?;
                        // Bytes: the ledger just took those refs to zero, so no view
                        // names them any more. Freeing them needs a data root, which
                        // the metadata URI cannot carry — same default and the same
                        // honest accounting as the two arms above.
                        let droot = data_root.clone().unwrap_or_else(|| vbase.clone());
                        let (mut freed, mut missing, mut failed) = (0usize, 0usize, 0usize);
                        let mut root_error = None;
                        match backend::open_local(&droot) {
                            Ok(be) => {
                                for dr in &rep.unbooked_refs {
                                    // Striped keys live as chunks on the data-servers;
                                    // a plain local root cannot free them — report,
                                    // never half-delete.
                                    if dr.starts_with('s') && dr.matches(':').count() >= 2 {
                                        failed += 1;
                                        eprintln!("note: striped object {dr} not freed from a plain local root — reclaim it through a mount or the data-servers");
                                        continue;
                                    }
                                    if be.len(dr).is_err() {
                                        missing += 1;
                                        continue;
                                    }
                                    match be.remove(dr) {
                                        Ok(()) => freed += 1,
                                        Err(e) => {
                                            failed += 1;
                                            eprintln!("warning: cannot free blob {dr}: {e}");
                                        }
                                    }
                                }
                            }
                            Err(e) => root_error = Some(e),
                        }
                        println!(
                            "removed base view '{name}' from {uri}: schema dropped with {} name(s), {} object(s) no longer held by any view{}",
                            rep.entries,
                            rep.unbooked,
                            if rep.still_held > 0 {
                                format!(", {} still held by other views", rep.still_held)
                            } else {
                                String::new()
                            }
                        );
                        match root_error {
                            Some(e) => eprintln!(
                                "note: cannot open data root {} to free {} object(s): {e} — the schema and its bookings are gone and those refs are unreferenced now; free them through the data-server or a mount",
                                droot.display(),
                                rep.unbooked
                            ),
                            None => println!(
                                "{} object(s) freed from {}, {} not found under that data root (wrong --data-dir? they live on another store), {} could not be freed",
                                freed,
                                droot.join("blocks").display(),
                                missing,
                                failed
                            ),
                        }
                        eprintln!(
                            "note: new mounts of '{name}' are now refused — `view create` starts over; this meta-dir's definitions were not touched (the schema is gone, `view list` still shows them)"
                        );
                        return Ok(());
                    }
                    // SQLite arm. (The PG arm above owns a schema and returned
                    // before it; by here the tree really is
                    // the local file `meta/<name>.db`.)
                    require_sqlite("`view rm --base`")?;
                    let rm = meta::remove_base_view(&vbase, &name)?;
                    let droot = data_root.unwrap_or_else(|| vbase.clone());
                    // The metadata tree is already gone at this point, so a bare
                    // io error from opening the data root would leave the user
                    // with no clue that the objects are now orphans (§5-4/§6-4).
                    // Name the root and point at `gc`, which reclaims exactly this
                    // "tree deleted, bytes unreleased" state once the root is usable.
                    let backend = backend::open_local(&droot).map_err(|e| format!(
                        "cannot open data root {} to free view '{}' objects: {}. The metadata tree meta/'{}'.db was already removed, so its now-unreferenced bytes are orphans; once the root is writable, reclaim them offline with `grovefs gc --meta-dir {} --data-dir {}` (add --apply to delete) — re-running the rm will not re-touch them",
                        droot.display(),
                        name,
                        e,
                        name,
                        vbase.display(),
                        droot.display()
                    ))?;
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
                        // --data-dir must report "not found here", never a
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
                            format!(", {missing} not found under that data root (wrong --data-dir? they live on another store)")
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
                            "note: {missing} unreferenced blob(s) were NOT deleted — re-run with --data-dir pointing at the store that holds them (default: this metadata root)"
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
                    // Where does this view's tree live? Under `--control-server`
                    // the SERVER owns it and the dump is pulled over the wire in
                    // chunks (only `--out` is local). Otherwise either backend
                    // dumps here, through its own reader: a SQLite file or a PG
                    // schema — one portable format, one tool.
                    let st = if let Some(rc) = &remote_ctl {
                        rc.export_view(&view, &out, force)?
                    } else {
                        // The definition lives in the control store, not
                        // necessarily in `meta/views/*.layout`: asking it is what
                        // makes the alias/derived refusal right on both
                        // backends instead of only on the file one.
                        let definition = ctrl.get_def_raw(&view)?;
                        if let Some(text) = definition.as_deref() {
                            match grovefs::layout::LayoutRules::parse_def(text)
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
                        if store_is_pg {
                            let uri = store.as_deref().expect("postgres:// is a store uri");
                            grovefs::pg_store::export_view(uri, &view, &out, force, definition)?
                        } else {
                            grovefs::meta::export_view(&vbase, &view, &out, force)?
                        }
                    };
                    println!(
                        "exported view '{view}' metadata: {} node(s), {} blob reference(s) -> {}",
                        st.nodes,
                        st.blobs,
                        out.display()
                    );
                    if let Some(rc) = &remote_ctl {
                        println!(
                            "note: the dump was rendered by the control plane at {} and pulled in \
                             chunks; only this file was written here",
                            rc.addr()
                        );
                    }
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
                    // The control store is the system of record for definitions
                    // on BOTH backends (files and `grovefs_control`), so the
                    // guards below are asked of it rather than
                    // of a `meta/views/*.layout` file a PG deployment may not
                    // even keep.
                    if ctrl.get_def_raw(&view)?.is_some() {
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
                        if ctrl.get_def_raw(base)?.is_some() {
                            return Err(format!(
                                "base '{base}' is governed by a definition (alias/derived/fork) — fork the physical base tree instead"
                            )
                            .into());
                        }
                        if !ctrl.has_physical(base)? {
                            return Err(format!(
                                "no physical base tree '{base}' in this store — import the base first, or omit --as-fork"
                            )
                            .into());
                        }
                    }
                    // The server records `--as-fork` itself (same verdict, same
                    // rollback); locally this end writes the definition below.
                    let def_written = remote_ctl.is_some();
                    let st = if let Some(rc) = &remote_ctl {
                        rc.import_view(&view, &input, replace, as_fork.as_deref())?
                    } else if store_is_pg {
                        let uri = store.as_deref().expect("postgres:// is a store uri");
                        grovefs::pg_store::import_view(uri, &view, &input, replace)?
                    } else {
                        grovefs::meta::import_view(&vbase, &view, &input, replace)?
                    };
                    let kind_word = if as_fork.is_some() {
                        "view"
                    } else {
                        "BASE view"
                    };
                    println!(
                        "imported {kind_word} '{view}' from {}: {} node(s), {} blob reference(s){}; cross-view ledger adjusted",
                        input.display(),
                        st.nodes,
                        st.blobs,
                        if st.replaced {
                            ", replacing the previous tree"
                        } else {
                            ""
                        }
                    );
                    if let Some(rc) = &remote_ctl {
                        println!(
                            "note: the dump was pushed in chunks to the control plane at {}; the \
                             tree now lives on that store",
                            rc.addr()
                        );
                    }
                    if let Some(base) = &as_fork {
                        if !def_written {
                            if let Err(e) = ctrl.put_def(&view, &format!("fork {base}\n")) {
                                let rollback =
                                    rollback_imported_tree(&vbase, store.as_deref(), &view);
                                return Err(format!(
                                    "cannot record fork definition for '{view}': {e}; {rollback}"
                                )
                                .into());
                            }
                        }
                        println!("note: recorded `fork {base}` — mounts of '{view}' are structure-writable, content read-only (EROFS)");
                    } else if st.kind == "fork" {
                        println!("note: the source was a fork tree; imported as a normal writable base view (use --as-fork <base> when you need fork semantics)");
                    }
                    println!("note: content bytes are not in the export — make sure `blocks/` from the source store is present under the target data root before mounting; a cross-machine import copies METADATA only and the CLI never silently copies bytes");
                }
                ViewAction::Move {
                    view,
                    pg_instance,
                    keep_source,
                } => {
                    // Move a tree between instances of a multi-instance store.
                    // This talks to the PostgreSQL instances DIRECTLY (snapshot,
                    // build, cutover, retire), so it runs on the storage host
                    // with --meta-store — not through --control-server — and
                    // only under a PG backend at all.
                    if !grovefs::layout::valid_view_name(&view) {
                        return Err(format!("invalid view name '{view}'").into());
                    }
                    if remote_ctl.is_some() {
                        return Err("view move talks to the PostgreSQL instances directly — run it on the storage host with --meta-store (drop --control-server)".into());
                    }
                    if !store_is_pg {
                        return Err("view move moves a tree between instances of a multi-instance PostgreSQL store (--meta-store file:/path/…); this store is SQLite and has exactly one home per view — there is nothing to move".into());
                    }
                    if let Some(mp) = view_mounted_at(&view) {
                        return Err(format!(
                            "view '{view}' is mounted here at {mp} — unmount it first (`grovefs umount -m {mp}`); mounts on OTHER machines are not visible in this mount table, quiesce them too (a stale copy is the failure mode)"
                        )
                        .into());
                    }
                    // An alias/derived name has no tree of its own (it renders
                    // another view's) — nothing to move. A fork owns a tree and
                    // moves like any other; its definition stays in the control
                    // plane, but its BASE does not follow: a later graft into
                    // this fork from its base would be a cross-instance pair
                    // and refused (move the base too, or graft from a view on
                    // the fork's new instance).
                    if let Some(text) = ctrl.get_def_raw(&view)? {
                        match grovefs::layout::LayoutRules::parse_def(&text)
                            .map_err(|e| format!("bad view definition '{view}': {e}"))?
                        {
                            grovefs::layout::ViewDef::Alias(real) => {
                                return Err(format!(
                                    "'{view}' is a pure alias of real view '{real}' — an alias has no metadata tree of its own, nothing to move"
                                )
                                .into())
                            }
                            grovefs::layout::ViewDef::Derived(base, _) => {
                                return Err(format!(
                                    "'{view}' is a derived (virtual) view over '{base}' — a derived view has no metadata tree of its own, nothing to move"
                                )
                                .into())
                            }
                            grovefs::layout::ViewDef::Fork(_) => {
                                println!("note: '{view}' is a fork — its tree moves with it, but its base stays where it is; a later graft from the base would be a cross-instance pair and refused");
                            }
                        }
                    }
                    let spec = store.as_deref().expect("store_is_pg implies a store spec");
                    let rep = grovefs::pg_store::move_view(spec, &view, &pg_instance, keep_source)?;
                    println!(
                        "moved view '{view}': {} node(s), {} blob reference(s), instance '{}' -> '{}'",
                        rep.nodes, rep.blobs, rep.from, rep.to
                    );
                    if rep.source_dropped {
                        println!(
                            "  source schema dropped on '{}', its ledger holds unbooked (one transaction); cutover is complete and idempotent to re-run",
                            rep.from
                        );
                    } else {
                        println!(
                            "  source copy left behind — see the note above for the rollback/cleanup path"
                        );
                    }
                    println!(
                        "  metadata only: content bytes stay in the data domain — keep mounts pointed at the same --data-servers (server-side gc unions all instances' ledgers, so nothing is reclaimed out from under this view)"
                    );
                }
                ViewAction::Pool {
                    view,
                    write,
                    read,
                    idle_secs,
                    inherit,
                } => {
                    // A view's pool bounds live in its OWN metadata tree (the
                    // `ctrl` rows an engine adopts at open), so — exactly like
                    // `view move` — this talks to the store directly and runs on
                    // the storage host with --meta-store.
                    if !grovefs::layout::valid_view_name(&view) {
                        return Err(format!("invalid view name '{view}'").into());
                    }
                    if remote_ctl.is_some() {
                        return Err("view pool writes into the view's own metadata tree — run it on the storage host with --meta-store (drop --control-server)".into());
                    }
                    if !store_is_pg {
                        return Err("view pool sets PostgreSQL connection-pool bounds; this store is SQLite, whose engine holds one write handle plus a --threads-sized read pool — there is no per-view PG pool to bound".into());
                    }
                    let spec = store.as_deref().expect("store_is_pg implies a store spec");
                    let mut want = grovefs::pg_store::PgPoolSettings::default();
                    if let Some(text) = &write {
                        let (warm, max) = grovefs::pg_store::parse_lane_spec(text)?;
                        want.write_warm = Some(warm);
                        want.write_max = max;
                    }
                    if let Some(text) = &read {
                        let (warm, max) = grovefs::pg_store::parse_lane_spec(text)?;
                        want.read_warm = Some(warm);
                        want.read_max = max;
                    }
                    want.idle_secs = idle_secs;
                    let bare_show = !inherit
                        && want.write_warm.is_none()
                        && want.read_warm.is_none()
                        && want.idle_secs.is_none();
                    let declared = if bare_show {
                        grovefs::pg_store::view_pool_get(spec, &view)?
                    } else {
                        grovefs::pg_store::view_pool_set(spec, &view, want, inherit)?
                    };
                    let lane = |warm: Option<usize>, max: Option<usize>| match (warm, max) {
                        (Some(w), Some(m)) if m != w => format!("{w}:{m}"),
                        // A floor with no ceiling: the burst stays whatever the
                        // serving process allows, only the floor is declared here.
                        (Some(w), None) => format!("{w}:(process ceiling)"),
                        (Some(w), Some(_)) => w.to_string(),
                        // A ceiling with no floor: the floor still comes from
                        // the serving process, only the burst is declared here.
                        (None, Some(m)) => format!("(process floor):{m}"),
                        (None, None) => "(process default)".to_string(),
                    };
                    println!(
                        "view '{}' pool bounds: write {}  read {}  reap-after {}",
                        view,
                        lane(declared.write_warm, declared.write_max),
                        lane(declared.read_warm, declared.read_max),
                        match declared.idle_secs {
                            Some(s) => format!("{s}s"),
                            None => "(process default)".to_string(),
                        }
                    );
                    if declared == grovefs::pg_store::PgPoolSettings::default() {
                        println!(
                            "  nothing declared — the view follows the serving meta-server's process bounds; that process prints them at startup (`--pg-write-pool` and friends), and this CLI cannot see another machine's settings"
                        );
                    } else {
                        println!(
                            "  takes effect on the view's NEXT engine open: unmount it, or wait out --engine-idle-secs — a live mount keeps the pool it already holds (its connections carry session-scoped prepared statements)"
                        );
                    }
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
            // control plane FOLLOWS the metadata backend: `--meta-store postgres://…`
            // (or `GROVEFS_META_STORE`) binds definitions, keys and the call audit to
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
            control_server,
            control_key,
            action,
        } => {
            // Keys + audit log are CONTROL-PLACE state and FOLLOW the metadata
            // backend: a local `api_keys.db` under SQLite, the shared
            // `grovefs_control.api_keys`/`mcp_calls` tables under a
            // `--meta-store postgres://…` — so one mint is authoritative for every
            // server pointing at the URI, with no per-node file to colocate.
            //
            // With `--control-server` the registry is the one INSTALLED IN the
            // remote meta-server's store: an operator rotates keys of a PG *or
            // SQLite* deployment from any machine, holding only an admin key
            // and never the store credential.
            let remote = match control_server.as_deref() {
                Some(addr) => {
                    refuse_local_with_remote(addr, &db_path, &store)?;
                    Some(remote_control(addr, resolve_control_key(control_key))?)
                }
                None => None,
            };
            let ak_base = db_path.unwrap_or_else(|| base.clone());
            let store = resolve_store(store);
            // Where this command's keys/audit actually live, for the messages
            // that point an operator at the next command.
            let where_keys = match &remote {
                Some(rc) => format!("the control plane at {}", rc.client().addr()),
                None => ak_base.display().to_string(),
            };
            let keys = match &remote {
                Some(rc) => KeyAdmin::Remote(rc.client().clone()),
                None => KeyAdmin::Local(grovefs::apikeys::ApiKeyStore::open_for(
                    &ak_base,
                    store.as_deref(),
                )?),
            };
            match action {
                ApikeyAction::Create {
                    label,
                    r#for,
                    view,
                    ro,
                    owner,
                    note,
                    ttl,
                } => {
                    let scope = grovefs::apikeys::Scope::parse(&r#for)
                        .map_err(Box::<dyn std::error::Error>::from)?;
                    // An admin key with no account is an unattributable
                    // administrator — the one thing the account model exists to
                    // prevent, so it is refused instead of silently defaulted.
                    if scope == grovefs::apikeys::Scope::Admin && owner.is_none() {
                        return Err(format!(
                            "`--for admin` needs `--owner <account>`: an administrator key is \
                             attributed by account in the control-plane audit, and one account \
                             (with several keys) is what makes rotation possible. Retry with e.g.\n  \
                             grovefs apikey create --label {label} --for admin --owner alice"
                        )
                        .into());
                    }
                    let (id, full, expires) = keys
                        .create(
                            &label,
                            scope,
                            view.as_deref(),
                            ro,
                            owner.as_deref(),
                            note.as_deref(),
                            ttl.as_deref(),
                        )
                        .map_err(Box::<dyn std::error::Error>::from)?;
                    println!(
                        "apikey created — shown ONCE (only its SHA-256 is stored; save it now):"
                    );
                    println!("  id:    {id}");
                    println!("  owner: {}", owner.as_deref().unwrap_or("<none>"));
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
                    if let Some(n) = note.as_deref().filter(|t| !t.trim().is_empty()) {
                        println!("  note:  {n}");
                    }
                    // Echoed from the side that stored it, so a deadline printed
                    // here is the deadline the handshake will compare against.
                    println!(
                        "  expires: {}",
                        match &expires {
                            Some(e) => e.as_str(),
                            None => "never (set one with --ttl, or `apikey update --ttl`)",
                        }
                    );
                    println!("  key:   {full}");
                    println!();
                    match scope {
                        // `admin` admits mount/mcp too (`admits` is a table), but
                        // the block below is the one that describes how to USE it;
                        // printing the mount recipe first would bury that.
                        grovefs::apikeys::Scope::Mount | grovefs::apikeys::Scope::Any => {
                            println!("Present it to a `--require-api-key` server as");
                            println!("  grovefs mount --meta-server ... --data-servers ... --api-key '{full}' -m /mnt/x");
                            println!("(or export GROVEFS_API_KEY='{full}' so it stays out of the command line).");
                        }
                        grovefs::apikeys::Scope::Mcp => {
                            println!("Present it as the `apiKey` param in the MCP `initialize` handshake, or");
                            println!("export GROVEFS_MCP_API_KEY='{full}' when launching");
                            println!("`grovefs mcp --require-api-key`.");
                        }
                        grovefs::apikeys::Scope::Admin => {}
                    }
                    if scope == grovefs::apikeys::Scope::Admin {
                        println!("Admin scope also governs a `meta-server --control-listen` from any machine, with");
                        println!(
                            "  grovefs view --control-server HOST:PORT --api-key '{full}' list"
                        );
                        println!("  grovefs apikey --control-server HOST:PORT --api-key '{full}' create --label k2 --for mount");
                        println!("(or export GROVEFS_CONTROL_KEY='{full}'). The operator never needs the store URI —");
                        println!("the server performs the work and audits every call under account '{}'.", owner.as_deref().unwrap_or("-"));
                    }
                    if scope == grovefs::apikeys::Scope::Any {
                        println!("Scope it to one entrance with `--for mount` / `--for mcp` to keep a mount");
                        println!("credential out of the governance API (and vice versa); `--for admin` is the");
                        println!("governance credential and is never implied by `any`.");
                    }
                    println!(
                        "Revoke later with: grovefs apikey {}",
                        match &remote {
                            Some(rc) =>
                                format!("--control-server {} revoke --id {id}", rc.client().addr()),
                            None => format!("--meta-dir {} revoke --id {id}", ak_base.display()),
                        }
                    );
                }
                ApikeyAction::List {
                    view,
                    label,
                    owner,
                    note,
                    scope,
                } => {
                    let all = keys.list()?;
                    // Resolve an alias name to its real view so keys bound to
                    // the real name also show under `--view <alias>` — read from
                    // the SAME control plane as the keys (PG rows or local files),
                    // never a path the chosen backend would ignore.
                    let real = match view.as_deref() {
                        None => None,
                        Some(v) => {
                            // The SAME control plane the keys come from: a remote
                            // registry's aliases live behind the remote server, not
                            // in this machine's `--home`.
                            let cs: Arc<dyn grovefs::mcp::ControlStore> = match &remote {
                                Some(rc) => Arc::new(rc.clone()),
                                None => {
                                    grovefs::mcp::control_store_for(&ak_base, store.as_deref())?
                                }
                            };
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
                        // Substring searches on the human-facing columns and an
                        // exact one on scope — a registry of hundreds is only
                        // manageable if `list` can slice it without `jq`.
                        .filter(|r| {
                            label
                                .as_deref()
                                .is_none_or(|q| r.label.to_lowercase().contains(&q.to_lowercase()))
                        })
                        .filter(|r| {
                            owner.as_deref().is_none_or(|q| {
                                r.owner
                                    .as_deref()
                                    .unwrap_or("")
                                    .to_lowercase()
                                    .contains(&q.to_lowercase())
                            })
                        })
                        .filter(|r| {
                            scope
                                .as_deref()
                                .is_none_or(|q| r.scope.eq_ignore_ascii_case(q))
                        })
                        .filter(|r| {
                            note.as_deref().is_none_or(|q| {
                                r.note
                                    .as_deref()
                                    .unwrap_or("")
                                    .to_lowercase()
                                    .contains(&q.to_lowercase())
                            })
                        })
                        .collect();
                    if all.is_empty() && view.is_none() {
                        println!(
                            "no api keys yet (in {where_keys}); mint one with `grovefs apikey create --label L`"
                        );
                        return Ok(());
                    }
                    println!(
                        "{:<3} {:<18} {:<12} {:<16} {:<16} {:<7} {:<12} {:<4} {:<25} {:<25} {}",
                        "#",
                        "id",
                        "key",
                        "owner",
                        "label",
                        "scope",
                        "view",
                        "rw",
                        "created",
                        "last_used",
                        "expires / revoked / note"
                    );
                    for (i, r) in rows.iter().enumerate() {
                        // The three tail fields share one trailing column: each is
                        // optional, and a fixed 3 more padded columns would push the
                        // note — the one field worth reading in full — off the edge.
                        let mut tail = match (r.revoked_at.as_deref(), r.expired) {
                            (Some(t), _) => format!("revoked {t}"),
                            (None, true) => format!("EXPIRED {}", r.expires_at.as_deref().unwrap_or("")),
                            (None, false) => match r.expires_at.as_deref() {
                                Some(e) => format!("expires {e}"),
                                None => "never".to_string(),
                            },
                        };
                        if let Some(n) = r.note.as_deref().filter(|t| !t.is_empty()) {
                            tail.push_str(&format!(" | note: {n}"));
                        }
                        println!(
                            "{:<3} {:<18} {:<12} {:<16} {:<16} {:<7} {:<12} {:<4} {:<25} {:<25} {}",
                            i + 1,
                            r.id,
                            r.secret_hint.as_deref().unwrap_or("-"),
                            r.owner.as_deref().unwrap_or("-"),
                            r.label,
                            r.scope,
                            r.view.as_deref().unwrap_or("<any>"),
                            if r.ro { "ro" } else { "rw" },
                            r.created_at,
                            r.last_used_at.as_deref().unwrap_or("-"),
                            tail,
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
                ApikeyAction::Rotate { id } => {
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
                                    "apikey id='{}' is revoked — rotate revives nothing (mint a new key)",
                                    k.id
                                )
                                .into());
                            }
                            let Some(full) = keys.rotate(&k.id)? else {
                                return Err(format!(
                                    "apikey id='{}' became revoked concurrently",
                                    k.id
                                )
                                .into());
                            };
                            println!("apikey rotated — NEW key shown ONCE (only its SHA-256 is stored; save it now):");
                            println!("  id:    {}", k.id);
                            println!("  owner: {}", k.owner.as_deref().unwrap_or("<none>"));
                            println!("  label: {}", k.label);
                            println!("  scope: {}", k.scope);
                            println!("  view:  {}", k.view.as_deref().unwrap_or("<any>"));
                            println!("  key:   {full}");
                            println!();
                            println!("  the id and every binding are unchanged — only the secret was replaced;");
                            println!("  the OLD secret fails its next handshake, and a live mount established on it");
                            println!("  stays open until it reconnects — `apikey revoke` is the kill-now verb.");
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
                ApikeyAction::Update { id, note, ttl } => {
                    if note.is_none() && ttl.is_none() {
                        return Err(
                            "nothing to update: pass --note TEXT and/or --ttl DUR (`--ttl never` removes a deadline)"
                                .into(),
                        );
                    }
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
                                    "apikey id='{}' is revoked — update edits nothing on a dead key (mint a new one)",
                                    k.id
                                )
                                .into());
                            }
                            keys.update(&k.id, note.as_deref(), ttl.as_deref())?;
                            // Re-read the row instead of echoing the flags: on the
                            // remote path the SERVER resolved `--ttl` against its own
                            // clock, and that answer is the one worth printing.
                            let after = keys.list()?.into_iter().find(|x| x.id == k.id);
                            println!("apikey updated: id='{}' (label '{}')", k.id, k.label);
                            if let Some(x) = after {
                                println!("  note:    {}", x.note.as_deref().unwrap_or("<none>"));
                                println!(
                                    "  expires: {}",
                                    match x.expires_at.as_deref() {
                                        Some(e) if x.expired => format!(
                                            "{e}  (EXPIRED — it authenticates nowhere until --ttl gives it a new deadline)"
                                        ),
                                        Some(e) => e.to_string(),
                                        None => "never".to_string(),
                                    }
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
                        println!("no logged calls yet (in {where_keys}; a `grovefs mcp` run appends one row per tools/call, a `--require-api-key` meta/data-server one per mount connect, and a `--control-listen` server one per governance call)");
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
        #[cfg(feature = "web")]
        Cmd::Web {
            control_server,
            control_key,
            control_key_file,
            listen,
            session_ttl,
            workers,
            allow_insecure_http,
            use_x_forwarded_for,
            action,
        } => {
            // ONE credential, three sources, one precedence: `--api-key` >
            // `--api-key-file` > `GROVEFS_CONTROL_KEY` > `GROVEFS_API_KEY`.
            let explicit = match (control_key, control_key_file) {
                (Some(k), _) => Some(k),
                (None, Some(p)) => Some(read_secret_file(&p, "--api-key-file")?),
                (None, None) => None,
            };
            let addr = control_server.ok_or_else(|| -> Box<dyn std::error::Error> {
                "grovefs web needs --control-server HOST:PORT. The console is a gateway in \n  front of `meta-server --control-listen HOST:PORT`: it holds no accounts, views, keys \n  or audit of its own, so there is nothing for it to serve without that address.\n  Try: grovefs web --control-server 127.0.0.1:9502 --api-key-file /etc/grovefs/control.key"
                    .into()
            })?;
            let key = resolve_control_key(explicit).ok_or_else(
                || -> Box<dyn std::error::Error> {
                    format!(
                        "--control-server {addr} needs the console's admin credential: --api-key \n  '<id>:<secret>', --api-key-file PATH (first line), or GROVEFS_CONTROL_KEY. It must be an\n  `admin`-scope key — mint one on the server host with `grovefs apikey --meta-dir \n  <server-meta-dir> create --for admin --owner <account>` (the first one cannot come \n  from the governance API, or that API would install its own administrators)."
                    )
                    .into()
                },
            )?;
            let client = grovefs::control_remote::ControlClient::new(&addr, &key);
            match action {
                // The gateway: foreground, one accept loop, no daemonizing — the
                // same contract as every other server subcommand here, so a unit
                // file's `ExecStart` works for all of them.
                None => {
                    let ttl = grovefs::gc::parse_age(&session_ttl)
                        .map_err(|e| -> Box<dyn std::error::Error> {
                            format!("--session-ttl '{session_ttl}': {e}").into()
                        })?;
                    if ttl.is_zero() {
                        return Err("--session-ttl cannot be 0: every request would sign itself out \n                        before it answered.".into());
                    }
                    grovefs::web::serve(grovefs::web::Config {
                        listen,
                        control: client,
                        session_ttl: ttl,
                        xff: use_x_forwarded_for,
                        workers: workers.max(1),
                        allow_insecure_http,
                    })?;
                }
                Some(a) => web_govern(&client, a)?,
            }
        }
        Cmd::Refs {
            db_path,
            store,
            control_server,
            control_key,
            action,
        } => {
            if let Some(addr) = control_server.as_deref() {
                refuse_local_with_remote(addr, &db_path, &store)?;
                let rc = remote_control(addr, resolve_control_key(control_key))?;
                match action {
                    RefsAction::Rebuild => {
                        // The recount reads the TREES, so it must run where they
                        // live; the server applies it to whatever backend it
                        // serves and answers in its own wording.
                        let r = rc
                            .client()
                            .call("refs.rebuild", serde_json::json!({}))
                            .map_err(Box::<dyn std::error::Error>::from)?;
                        println!("{}", r.as_str().unwrap_or("refs ledger rebuilt"));
                        println!(
                            "  run it QUIESCED: a booking committed between the server's read pass and write pass would be overwritten with the pre-booking count"
                        );
                        return Ok(());
                    }
                }
            }
            let rbase = db_path.unwrap_or_else(|| base.clone());
            let store = resolve_store(store);
            match action {
                RefsAction::Rebuild => {
                    if store.as_deref().is_some_and(grovefs::pg_store::is_pg_store) {
                        // The ledger is `grovefs_refs.holders` IN the database, so
                        // the recount has to happen there too: the same "read the
                        // TREES, not the ledger" rule as the two arms above, and
                        // the same repair of both directions (a lost booking AND
                        // an over-count). Nothing here frees bytes — that is the
                        // data servers' business, and `gc` refuses a store URI.
                        let uri = store.as_deref().expect("just matched a pg uri");
                        let rep = grovefs::pg_store::rebuild_ledger(uri)?;
                        println!(
                            "refs ledger rebuilt across every instance of {uri}: {} tree(s), {} content key(s), {} holder unit(s); {} row(s) corrected, {} row(s) deleted",
                            rep.trees, rep.refs, rep.holders, rep.rows_fixed, rep.rows_deleted
                        );
                        println!(
                            "  the recount reads the TREES (every grovefs_v_* schema), not the ledger — so a lost booking is restored (the unsafe direction) as well as an over-count dropped; nothing here frees object bytes (that is the data servers' business)"
                        );
                        println!(
                            "  run it QUIESCED: a booking committed between the read pass and the write pass would be overwritten with the pre-booking count"
                        );
                        return Ok(());
                    }
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
            control_server,
            control_key,
            data_servers,
            data_token,
            data_api_key,
        } => {
            // The REMOTE audit: the server reads the trees where they live and
            // we only name the bytes. This is the one form that works under PG,
            // where no `<data-dir>` on this machine holds them.
            if let Some(addr) = control_server.as_deref() {
                refuse_local_with_remote(addr, &db_path, &None)?;
                if data_root.is_some() {
                    return Err(format!(
                        "--control-server {addr} audits bytes on the DATA SERVERS, so --data-dir cannot be combined with it — name the roots with --data-servers HOST:PORT instead"
                    )
                    .into());
                }
                let rc = remote_control(addr, resolve_control_key(control_key))?;
                let servers: Vec<String> = data_servers
                    .or_else(|| {
                        std::env::var("GROVEFS_DATA_SERVERS")
                            .ok()
                            .filter(|s| !s.is_empty())
                    })
                    .map(|s| {
                        s.split(',')
                            .map(str::trim)
                            .filter(|a| !a.is_empty())
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                if servers.is_empty() {
                    return Err(format!(
                        "gc --control-server {addr} needs --data-servers HOST:PORT[,HOST:PORT] \
                         (or GROVEFS_DATA_SERVERS): the server owns the trees, the data servers own \
                         the bytes"
                    )
                    .into());
                }
                let token = data_token
                    .or_else(|| std::env::var("GROVEFS_TOKEN").ok())
                    .unwrap_or_default();
                let dkey = data_api_key
                    .or_else(|| std::env::var("GROVEFS_API_KEY").ok())
                    .unwrap_or_default();
                let op = if apply { "gc.apply" } else { "gc.scan" };
                let rep = rc
                    .client()
                    .call(
                        op,
                        serde_json::json!({
                            "data_servers": servers,
                            "data_token": token,
                            "data_api_key": dkey,
                            "older_than": older_than,
                            "force": force,
                        }),
                    )
                    .map_err(Box::<dyn std::error::Error>::from)?;
                if json {
                    println!("{rep}");
                } else {
                    print_gc_remote(&rep);
                }
                return Ok(());
            }
            // The trees this command audits are local `<view>.db` files. Under
            // PG they are schemas and the bytes belong to the data servers —
            // there is no offline page tree to walk here, so this refuses rather
            // than reporting "nothing to reclaim".
            let offline_store = resolve_store(None);
            if offline_store.as_deref().is_some_and(grovefs::pg_store::is_pg_store) {
                return Err("`gc` audits LOCAL page files under <data-dir>/blocks; a PostgreSQL deployment's trees are schemas and its bytes are owned by the data servers — reclaim there with the DBA/data-server tooling, not this command".into());
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
                return Err("gc: no data root to scan (pass --data-dir)".into());
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
            // surviving trees is almost always a --meta-dir/--data-dir mixup,
            // and deleting there would kill another store's bytes.
            if rep.objects > 0 && rep.refs > 0 && rep.matched == 0 && !force {
                return Err(format!(
                    "gc: the data root(s) share no object with any view tree in {} ({} object(s), {} referenced key(s)) — wrong --meta-dir/--data-dir? Nothing was deleted; pass --force only if this is expected",
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
            store,
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
            // `import` writes a LOCAL SQLite tree, so it is only sound where the
            // deployment's backend IS SQLite. The backend is `--meta-store`, or
            // the `GROVEFS_META_STORE` env behind it (the same flag > env chain
            // as every other control-plane command). Refusing
            // beats the alternative — standing up `meta/<view>.db` plus
            // `blocks/` beside a root whose running meta-server serves the
            // cluster, printing a success about a tree nothing ever reads: the
            // same refuse-not-downgrade rule `Cmd::View` applies to the other
            // file-level physical ops.
            if let Some(u) = resolve_store(store).as_deref() {
                if grovefs::pg_store::is_pg_store(u) {
                    return Err(
                        "import operates on a local SQLite view db; under a PostgreSQL deployment (--meta-store postgres://…, or GROVEFS_META_STORE) a base tree is a PG schema (create one with `view create`) and its bytes move through a mounted path — drop --meta-store for a local store"
                            .into(),
                    );
                }
            }
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
            // A view DB that is absent here gets created by MetaEngine::open below,
            // which stands up a new physical BASE view as a side effect of the import.
            let created_base = !mp.exists();
            if let Some(dir) = mp.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let meta = MetaEngine::open(&mp)?;
            // Same multi-root contract as `data-server --data-dir`: a
            // comma-separated --data-dir registers imported objects onto the
            // disk their key hashes to, colocated with the running server.
            let roots: Vec<PathBuf> = droot
                .to_string_lossy()
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .collect();
            if roots.is_empty() {
                return Err("import --data-dir: no usable path".into());
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
            if created_base {
                println!(
                    "created BASE view '{view}' (target was absent); imported {} object(s)",
                    st.linked + st.copied
                );
            }
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
                meta: meta.or_else(|| {
                    std::env::var("GROVEFS_META_SERVER")
                        .ok()
                        .filter(|s| !s.is_empty())
                }),
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
                data: data.or_else(|| {
                    std::env::var("GROVEFS_DATA_SERVERS")
                        .ok()
                        .filter(|s| !s.is_empty())
                }),
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
        // `--meta-server`/`--data-servers` client — never touches these pages.
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

/// Does one process reference this store? Args first (`--home`/`--meta-dir`/
/// `--data-dir`/`--data-dir`, in both `--flag value` and `--flag=value` form),
/// then the GROVEFS_* env knobs. A grovefs process that names NO store falls
/// back to `$HOME/.grovefs` unless it is a remote client (`--meta-server`/`--data-servers`),
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
    let mut seen = (false, false, false); // --home, --meta-dir, --data-dir
    for (i, a) in args.iter().enumerate() {
        if a == "--meta-server"
            || a == "--data-servers"
            || a.starts_with("--meta-server=")
            || a.starts_with("--data-servers=")
        {
            remote = true;
        }
        let (flag, val) = if let Some(eq) = a.find('=') {
            (&a[..eq], Some(a[eq + 1..].to_string()))
        } else if matches!(a.as_str(), "--home" | "--meta-dir" | "--data-dir") {
            (a.as_str(), args.get(i + 1).cloned())
        } else {
            continue;
        };
        if !matches!(flag, "--home" | "--meta-dir" | "--data-dir") {
            continue;
        }
        named = true;
        match flag {
            "--home" => seen.0 = true,
            "--meta-dir" => seen.1 = true,
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
        (seen.1, "GROVEFS_META_DIR"),
        (seen.2, "GROVEFS_DATA_SERVERS"),
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
    if !matches!(flag, "--home" | "--meta-dir" | "--data-dir") {
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
    if rep.objects == 0 && rep.striped_layouts > 0 {
        // A "0" here is not "empty store": the trees reference striped objects
        // whose chunks live on data-servers, and gc only audits local block
        // roots. Say so instead of letting the bare 0 read as "no bytes".
        println!(
            "  objects: 0 local block(s) — {} striped layout(s) place their chunks on data-servers, which gc does not audit (use `grovefs du` for their bytes)",
            rep.striped_layouts
        );
    } else {
        println!(
            "  objects: {} ({})",
            rep.objects,
            fmt_bytes(rep.objects_bytes)
        );
    }
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
            "  ledger-held: {} ({}) — NOT deleted; run `grovefs refs --meta-dir {} rebuild` first if these are stale",
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
        None => {
            // Keep "nothing deleted" as the stable dry-run prefix, then give
            // the reason: only invite `--apply` when there is something to
            // reclaim, rather than suggesting a pointless re-run on 0 bytes.
            let tail = if rep.reclaimable() > 0 {
                format!(
                    " Re-run with --apply (offline) to reclaim {} object(s), {}.",
                    rep.reclaimable(),
                    fmt_bytes(rep.reclaimable_bytes())
                )
            } else if rep.orphans.is_empty() {
                " No unreferenced objects were found.".to_string()
            } else {
                format!(
                    " {} orphan(s) are all within the {} grace; re-run after it elapses.",
                    rep.orphans.len(),
                    fmt_age(rep.grace.as_secs())
                )
            };
            println!("dry run: nothing deleted.{}", tail);
        }
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

/// Render the control plane's `gc` report. The server answers in the shape
/// [`gc_json`] uses (plus where it ran and what it could see), so this prints
/// the lines the local report prints without re-deriving a single verdict here:
/// the deletion decision stays on the server that owns the trees.
fn print_gc_remote(rep: &serde_json::Value) {
    let num = |k: &str| rep.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
    let txt = |k: &str| rep.get(k).and_then(|v| v.as_str()).unwrap_or("");
    let arr = |k: &str| -> Vec<serde_json::Value> {
        rep.get(k)
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
    };
    let servers: Vec<&str> = rep
        .get("data_roots")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    println!("gc: meta {} (backend {})", txt("meta_root"), txt("backend"));
    println!("  data server(s): {}", servers.join(", "));
    println!(
        "  views: {} tree(s), {} referenced key(s), {} striped layout(s)",
        arr("views").len(),
        num("referenced_keys"),
        num("striped_layouts")
    );
    println!(
        "  objects: {} ({})",
        num("objects"),
        fmt_bytes(num("object_bytes"))
    );
    println!(
        "  orphans: {} ({}; {} reclaimable, {} within grace <{})",
        num("orphans"),
        fmt_bytes(num("orphan_bytes")),
        num("reclaimable"),
        num("within_grace"),
        fmt_age(num("grace_secs"))
    );
    let orphans = arr("orphans");
    for c in orphans.iter().take(20) {
        println!(
            "    {:>10}  {:>8}  {:<5}  {}",
            fmt_bytes(c["size"].as_u64().unwrap_or(0)),
            fmt_age(c["age_secs"].as_u64().unwrap_or(0)),
            c["kind"].as_str().unwrap_or("?"),
            c["key"].as_str().unwrap_or("")
        );
    }
    if orphans.len() > 20 {
        println!(
            "    ... {} more (use --json for the full list)",
            orphans.len() - 20
        );
    }
    let held = arr("ledger_held");
    if !held.is_empty() {
        println!(
            "  ledger-held: {} ({}) — NOT deleted; run `grovefs refs rebuild --control-server …` first if these are stale",
            held.len(),
            fmt_bytes(num("ledger_held_bytes"))
        );
        for c in held.iter().take(5) {
            println!(
                "    {:>10}  {}",
                fmt_bytes(c["size"].as_u64().unwrap_or(0)),
                c["key"].as_str().unwrap_or("")
            );
        }
        if held.len() > 5 {
            println!("    ... {} more", held.len() - 5);
        }
    }
    let mounts = arr("server_mounts");
    let sessions = arr("live_sessions");
    if !mounts.is_empty() || !sessions.is_empty() {
        println!(
            "  in use: {} mount(s) on the server host, {} view(s) with live sessions there",
            mounts.len(),
            sessions.len()
        );
        for m in mounts.iter().take(5) {
            println!("    mounted on the server: {}", m.as_str().unwrap_or(""));
        }
        println!(
            "  a mount on another machine is not visible from the server host; stop it too before --apply"
        );
    }
    if rep
        .get("applied")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        let failed = arr("delete_failed");
        println!(
            "reclaimed: {} object(s), {} freed; {} within grace kept; {} failed",
            num("deleted"),
            fmt_bytes(num("deleted_bytes")),
            num("within_grace"),
            failed.len()
        );
        for f in failed.iter().take(10) {
            println!("    failed: {}", f.as_str().unwrap_or(""));
        }
    } else {
        let tail = if num("reclaimable") > 0 {
            format!(
                " Re-run with --apply to reclaim {} object(s), {}.",
                num("reclaimable"),
                fmt_bytes(num("reclaimable_bytes"))
            )
        } else if orphans.is_empty() {
            " No unreferenced objects were found.".to_string()
        } else {
            format!(
                " {} orphan(s) are all within the {} grace; re-run after it elapses.",
                orphans.len(),
                fmt_age(num("grace_secs"))
            )
        };
        println!("dry run: nothing deleted.{tail}");
    }
}

fn run_fusermount(args: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
    let status = std::process::Command::new(fusermount_bin()).args(args).status()?;
    if !status.success() {
        return Err(format!("{} exited with {status}", fusermount_bin()).into());
    }
    Ok(())
}

fn fusermount_bin() -> &'static str {
    if Path::new("/usr/bin/fusermount3").exists() {
        "fusermount3"
    } else {
        "fusermount"
    }
}

/// [`run_fusermount`] with a deadline. A plain `fusermount -u` on a busy FUSE
/// mount BLOCKS in the kernel's `umount()` until every in-flight request is
/// answered — and a daemon tearing itself down stops answering nothing only
/// while that helper blocks, so an unbounded wait here is one edge of a real
/// three-way deadlock (helper blocked on umount ⇄ reverse-inval blocked on
/// the busy directory ⇄ new ops queued behind it). Bounding the wait turns
/// the deadlock into a timeout that escalates to the lazy path.
fn run_fusermount_timeout(
    args: &[&str],
    wait: std::time::Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::process::Stdio;
    let mut child = std::process::Command::new(fusermount_bin())
        .args(args)
        .stdin(Stdio::null())
        .spawn()?;
    let born = std::time::Instant::now();
    loop {
        match child.try_wait()? {
            Some(status) => {
                return if status.success() {
                    Ok(())
                } else {
                    Err(format!("{} exited with {status}", fusermount_bin()).into())
                }
            }
            None if born.elapsed() >= wait => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("timed out after {}ms", wait.as_millis()).into());
            }
            None => std::thread::sleep(std::time::Duration::from_millis(20)),
        }
    }
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
/// stale `Transport endpoint is not connected`), so: retry (each attempt
/// TIME-BOUNDED — a busy umount does not merely answer EBUSY, it can block
/// forever inside the syscall, which is how a revoke landing on a mount under
/// live traffic once deadlocked its own teardown), then `-u -z`, which detaches
/// the mountpoint even while busy and still ends the session. If even the lazy
/// detach wedges (or errors), a detached watchdog repeats it — the helper's
/// blocked `umount()` completes on its own once the queued requests drain, so
/// re-asking without owning the session's exit path guarantees the mountpoint
/// eventually goes down instead of hanging the escalation here. If nothing
/// works we keep serving — a mount that answers with an error is recoverable by
/// an operator, a dead endpoint is not.
fn spawn_term_unmounter(mountpoint: PathBuf) {
    std::thread::spawn(move || {
        install_termination_handlers();
        wait_for_termination();
        let mp = mountpoint.to_string_lossy();
        // Go straight to the lazy detach. A plain `fusermount -u` is NOT a
        // safe first try on a terminating daemon: on a busy mountpoint its
        // `umount()` BLOCKS in the kernel until every queued request drains —
        // and the drain needs this very call to return, so a revoke landing
        // under live traffic once deadlocked its own teardown (helper in
        // `umount()` ⇄ reverse-inval on the busy directory ⇄ new ops queued
        // behind it). Killing a blocked helper is worse (it can strand
        // fusermount's mount lock). `umount2(MNT_DETACH)` never blocks on
        // busy refs and the kernel aborts the connection's pending requests
        // once the last opener goes away, which the watchdog below confirms.
        if run_fusermount_timeout(&["-u", "-z", &mp], std::time::Duration::from_secs(2)).is_err() {
            let mp = mountpoint.clone();
            std::thread::spawn(move || {
                let mp = mp.to_string_lossy().into_owned();
                for _ in 0..30 {
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    let held = std::fs::read_to_string("/proc/mounts")
                        .map(|m| {
                            m.lines()
                                .any(|l| l.split_whitespace().nth(1) == Some(mp.as_str()))
                        })
                        .unwrap_or(false);
                    if !held {
                        break;
                    }
                    let _ = run_fusermount_timeout(
                        &["-u", "-z", &mp],
                        std::time::Duration::from_secs(2),
                    );
                }
            });
        }
    });
}

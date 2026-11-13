# GroveFS

> **A user-view FUSE filesystem** — bytes live centrally on disk while each user mounts an **independent, private directory tree** (with full nesting); no NFS.

[中文](README.md)

---

## What it is

`GroveFS` is a **user-view FUSE filesystem**. It cleanly separates *who sees what* (the view) from *where bytes actually live* (the object):

- **View**: each user gets a private directory tree, stored as one SQLite DB `<home>/meta/<view>.db`. Users cannot see each other's files.
- **Object**: the real bytes of a file, stored as UUID-named object files under `blocks/`, auto-fanned-out over **256 bucket dirs** keyed by a hash of the object id (no single directory grows with the total, so backup/traversal tools stay usable at tens of millions of objects); the view's directory entries only hold object keys (pointers).
- **Base dir `--home`**: in local mode, the physical root holding both `blocks/` (data) and `meta/` (views). A remote mount needs no local store at all; the central `meta-server` points its authoritative metadata root at `--meta-dir` instead.

On top of that, metadata and data can each be served as a network service (`meta-server` / `data-server`) so **many client machines mount and share one authoritative view over the LAN** — a central-storage + in-rack LAN-access topology, and **no NFS**.

## Key features

- **Per-user view isolation** with full custom nesting (`mkdir`/`mknod`/`rename`/`rmdir`).
- **Pluggable storage/metadata backends** via traits: local, remote, and cross-server striped (`ShardedBackend`). The authoritative metadata store can additionally be swapped to **PostgreSQL** (`meta-server --meta-store postgres://…`): one PG schema per view, transactions around rename/unlink/lock; under PG the in-tree HA (snapshot shipping) is intentionally off — use PG's own streaming replication / PITR instead.
- **View-bound / read-only API keys (the recommended access control)**: `grovefs apikey create --for mount --view V [--ro]` mints a credential that carries *which tree it may open* and *read-write or read-only*. A key bound with `--view V` can only `M_OPEN` view V (opening any other view is refused at the handshake with `E_AUTH`); an `--ro` key is refused **server-side on every write-intent op** (`E_NOTPERM` on the meta plane, `EROFS` on content writes at the data server), and the `M_OPEN` ack tells the client to mount read-only, so “read-only” never depends on the client remembering `-o ro`. A key is a real, revocable door (unlike a self-reported uid, which is advisory and forgeable). A system-level key (no `--view`) still opens any view.
- **Deletion safety for objects shared across views (the `refs` ledger)**: one `data_ref` names exactly ONE underlying object, yet several physical views can hold names for it at once (`grovefs import` of the same source file into two views resolves to the same key; derived views/aliases share their base view's bytes). A view's own reference count cannot see other views, and trusting it would delete an object another view still references. So the bookkeeping lives in a shared ledger `<base>/meta/refs.db` (PG: table `grovefs_refs.holders`), which every view ATTACHes so the holder count and the node change commit **atomically in one IMMEDIATE transaction**. A blob is physically removed only when “the last name for that key *in this view* is unlinked **and** no other view still holds it”. Out-of-band view-db removals (`rm meta/x.db`, a fresh `--home`) can only make the ledger *over*-count — bytes are kept, never lost; `grovefs refs --meta-dir D rebuild` recomputes holders from the trees that actually exist. **Only BASE views (physical trees created explicitly via `grovefs view create`) ever free bytes on delete**; derived/alias mounts are read-only and their removal never GCs. Removing a base view is an explicit act: `grovefs view rm --name V --base` deletes its `meta/V.db` (+WAL/SHM), rebuilds the ledger, and frees every blob no other view holds (bytes resolve under `--data-dir`, default `--home`; a governed name, a live `grovefs:<V>` mount, or a `--meta-store postgres://…` deployment is refused — USAGE §3.4.3). A FORK holds real names for shared blobs, so a delete inside a fork frees an object only when it is the last name across every view (base included); removing the fork tree triggers `refs rebuild` to drop its holds. A key with no ledger row is simply private to one view, so an empty ledger needs no initialization step. Orphan pages that no surviving tree names and the ledger holds no row for (an out-of-band `rm meta/x.db`, or a crash between object creation and the node insert) are audited and reclaimed offline with `grovefs gc --meta-dir D [--data-dir R] [--apply]`: the scan is the default; `--apply` only deletes pages older than `--older-than` (default 24h), never a key the ledger still holds, and refuses while the store is mounted or a local grovefs process uses it (a PostgreSQL deployment is refused outright).
- **Per-client presentation overlay (`mount --layout rules.txt`)**: one authoritative tree, one copy of bytes, a different *displayed* shape per mount — `hide` trims a subtree, `only` whitelists, `map` relocates an entry to another path (its source spot vacates), `link` adds an alias visible in both places at once (same inode). The lens is pure `MetaProvider`-layer path translation: real entries keep their real inodes, so data IO, locks, GC and invalidation push go straight through; nothing is copied and other mounts are unaffected.
- **Server-side derived views (`grovefs view define` + `mount --view <name>`)**: when shapes should be managed **centrally**, `grovefs view define --name docsonly --base team --rules F` stores the rules in the meta root (`<meta-dir>/meta/views/<name>.layout`, first line `base <physical view>`) and clients simply `mount --view docsonly` — **no local rules file needed**. Derived (virtual) views mount **READ-ONLY, always**: the shape is centrally governed — distribute it, never modify it — so every write belongs to the original (base) view; a mount of a derived name negotiates the read-only flag at `M_OPEN` and lands on a kernel read-only mount (writes give `EROFS`), with an independent authoritative per-op gate on the `meta-server` and the same check applied to local (`--home`) mounts. One definition renders the same shape for every machine (virtual inodes agree across hosts), **edited definitions hot-apply** (the server checks the definition's mtime per op; live mounts reshape within their ≤2s cache TTL, no remount). A local (`--home`) mount resolves the same `<home>/meta/views/<view>.layout` when no `--layout` file is given.
- **Server-side pure aliases (`grovefs view alias`)**: governance of *what a view is called* rather than *what it looks like* — a public name maps 1:1 onto a physical view (the definition is a single `alias <real view>` line, no rules), so an alias mount shows the **FULL tree and stays WRITABLE**, while the **real name stops being a mount entry point** (both `meta-server` and local `--home` mounts of a governed real name are refused). Public names can rotate freely without exposing the storage identity — guessing the real name no longer opens a door, and a leaked public name is retired in **one gapless command** (`alias --retire`: write-before-remove so the real name stays unmountable throughout). Aliases share `meta/views/` and `list`/`rm` with derived views; alias chains and derived targets are rejected; `grovefs import` refuses an alias target (an alias is not a storage identity).
- **Zero-copy import of existing data (`grovefs import`)**: when the server already holds a pile of directory data (say `/data/legacy`), `grovefs import --view V --source DIR` registers the whole tree into a view without moving bytes — on the same filesystem each object is a **hard link** of the source file (zero extra bytes; both paths read/write one inode, live in both directions), degrading to a streamed copy across devices. Structure/timestamps/mode/owner are preserved, re-running is incremental, and deleting a name in the view only unlinks the `blocks/` path — **the user's source data can never be destroyed by the view**. Composes with derived views/lenses for virtual-directory shapes (`map` the imported paths anywhere; the resulting derived mount is **read-only** — write through the imported base view). `grovefs import` also refuses a derived target name, which would produce a shadow `<view>.db` nobody can read.
- **Custom binary framing over TCP** (not NFS, not gRPC) with a connection pool.
- **Real-time cross-client invalidation push** (default): after a write, the center broadcasts `inval_entry`+`inval_inode` so peers see changes immediately, no polling; `--no-cache` gives a zero-caching fallback.
- **Distributed advisory locks**: `flock` / `fcntl F_SETLK/F_GETLK` arbitrated centrally, effective across processes and machines; crash reclaim via FUSE `release`, plus a short lease (default 15s) and renewal heartbeats.
- **High availability**: primary/standby `meta-server` with heartbeat/epoch promotion and automatic demotion; `--sync-repl` drops write RPO to 0; `--witness` takes a comma-separated list of shared `data-server` arbiters (odd `N`) and arbitrates by **majority** (`quorum_size = N/2+1`), **fail-safe**, holding a non-overlapping lease at a majority (renewed every tick; each arbiter refuses overlapping leases, so a majority live lease is itself proof of exclusive authority and a write is admitted from a local lease check) and stamping content frames so the arbiters can fence a stale writer, bounding the double-write window to the lease safety margin; losing a minority of arbiters still allows writes (`N=1` = a lone witness).
- **Token authentication** (shared cluster passphrase; both planes), **revocable per-caller API-key gates** (`meta-server`/`data-server --require-api-key` + `mount --api-key`; a key can be bound to ONE view with `--view` and forced read-only with `--ro`, both enforced server-side), **read-only mounts** (`--ro`), **multi-threaded `fuser 0.18`** (`clone_fd`), **chunked parallel IO**, and **cross-server striping**.
- **Per-mount content bandwidth shaping (`--bw-read` / `--bw-write`)**: a client-side GCRA slot schedule paces the bytes actually delivered — reads before the reply, writes after persistence — so the long-run rate is exactly the cap (bytes are paced from the first op; a single op larger than one second of budget may start up to 1 s early) and nothing is dropped or truncated, and write-side delay is natural backpressure. `0`/omitted = unlimited (one `rate == 0` branch on the hot path); `--stats`' `bytes read`/`bytes written` counters verify achieved throughput. Honest boundary: self-discipline, not isolation — a raw data-plane client that bypasses FUSE, or N mounts of one view, each get their own budget, and orphaned objects are keyed by UUID and shared across views, so they cannot be attributed to a view server-side.
- **Agent-facing MCP interface (`grovefs mcp`)**: the view governance above (`define`/`alias`/`list`/`rm` of derived views and pure aliases) is also exposed as **MCP (Model Context Protocol) tools** over stdio, so an AI agent can customize a data listing's shape programmatically — rules travel as **inline text** through `tools/call` and the agent never touches a file. Two read-only introspection tools close the discovery loop: `list_view_tree` renders the real listing a view shows (a derived view through its own lens, or an unpublished `base`+`rules` previewed without committing) and `get_view` reads back a single definition, so an agent writes rules against actual paths instead of guessing; both open the LOCAL SQLite metadata read-only, mount nothing, and touch no object bytes; a `validate_definition` dry run additionally checks a whole candidate (name+base+rules) against every governance guard — writing nothing — and returns the exact refusal a real publish would give. Shares the exact validation and error wording with the CLI (one contract, two entrances) and is control-plane only: it mounts nothing and writes no data. Authentication is opt-in — `grovefs mcp --require-api-key` gates the state-touching methods behind a `grovefs apikey` credential (hashed at rest, scoped `mcp`/`any`) and logs every call; the same `--require-api-key` also fronts `meta-server`/`data-server` for mounts (scope `mount`/`any`), with revocation closing live mounts. An `admin`-scoped ACCOUNT key (`--owner`) opens the remote governance port (`meta-server --control-listen HOST:PORT`) instead, so any machine can run `view`/`apikey`/`refs` governance against that store with a revocable credential instead of a database URI — SQLite included (USAGE §3.5.2). See the “Customizing data listings for agents: the MCP interface” section and USAGE §3.5.
- **Parallel metadata reads + paged directory listings + `FUSE_READDIRPLUS`**: alongside its single writer connection, each view's `MetaEngine` keeps a warm read pool sized to the mount's `--threads` (default 4; embedded/meta-server 1, `GROVEFS_SQLITE_READ_POOL` overrides 0–16) that serves `lookup`/`getattr`/`readdir` and the nlink resolution, so concurrent reads never queue behind the write lock (**writes still serialize on that one authoritative connection** — single-writer semantics are untouched); the server-side engine uses the same pool, so concurrent `meta-server` RPC handlers stop waiting on each other too. Directory listing goes through `readdir_page(parent, offset, limit)`, which the SQLite engine pushes down as SQL `LIMIT/OFFSET` while the FUSE callbacks fill each reply buffer with geometrically growing chunks (32→1024 for READDIRPLUS, 128→1024 for plain READDIR), so one kernel request costs `O(buffer)` instead of re-scanning the whole directory; and `FUSE_DO_READDIRPLUS` (kernel >= 4.6) is negotiated by default, shipping attributes with the entries so `ls -l` stops degrading into N `LOOKUP`+`GETATTR` round-trips. Measured on one 5001-entry directory, same mount point, each pass behind a fresh mount (empty kernel dentry cache), `ls -l`: **2.56s**, or 5.22s with `--no-readdirplus` (paging only) — both listings byte-identical once sorted. Paging and READDIRPLUS are enabled on any provider that pages *inside its own store*: local `--home` SQLite and PostgreSQL (each with its own 4-connection read pool + SQL `LIMIT/OFFSET`), and remote mounts against a SQLite/PG `meta-server` — the `M_READDIRP` frame pushes `offset/limit` to the server, so a remote client fetches one page per kernel buffer and negotiates READDIRPLUS. Same 5001-entry dataset over `meta-server`+`data-server`: **2.29s** (4.22s with `--no-readdirplus`, paging only; listings byte-identical once sorted). Only derived/lens views (`LayoutMeta`) and mounts against a server too old to advertise the paging cap still fall through to one full listing per buffer with no READDIRPLUS. `--no-readdirplus` stays as the A/B and compatibility knob.
- **Full regression**: `run_all.sh` — **38 stages (37 gated + bench), all GREEN** (`layout` presentation-overlay e2e incl. server-side derived views and read-only derived mounts, remote & local, and pure aliases — writable alias mounts, real-name mount refusal (remote & local), chain/derived-target guards, rm reopens the real name, and gapless name rotation via `--retire` (a leaked public name is swapped in one step — old name stops resolving, the real name stays unmountable throughout); 78 assertions + `bwlimit` per-mount content bandwidth-shaping e2e (17 assertions: a 24MiB read/write at 8MiB/s is held to a >=1.4s floor while an unlimited mount of the SAME view is materially faster, throttled bytes are intact, `GROVEFS_BW_READ` env fallback, `--bw-read 0` = unlimited, invalid rates refused at parse time, `--bw-write` stays inert on a read-only mount) + `import` zero-copy import e2e + `migrate` metadata-export/import e2e (one view's tree travels as host-neutral JSONL to a different store path — no absolute path/hostname/blocks path in the file; ids/inodes/hard links/mode/owner/symlink targets survive, the ledger is rebuilt, import is atomic so a failed `--replace` keeps the old tree, `--as-fork` restores fork semantics with a shared ledger, and several base views stay independent; 74 assertions) + `mcp` agent-governance e2e + `apikey` MCP-interface API-key auth & call-audit e2e (33 assertions: create/list/revoke/set-ro in-place ro↔rw flip (the flipped key still authenticates), hashed-at-rest, no-key `-32001` refusal, good-key admit + attributed call log, `GROVEFS_MCP_API_KEY` env pre-auth, default no-gate server) + `mountkey` revocable mount-credential e2e (76 assertions: scope minting + typo refusal, refusal reasons carried in the protocol, admission & env fallback with the secret absent from argv, connection audit down to key and view, scope isolation both ways, a revoke closing a LIVE mount with no stale endpoint, other keys unaffected, token+key double gate, an independent data-plane gate, failover carrying the credential, ungated default unchanged, a view-bound key refused across views with its refusal audited, an `--ro` key's writes refused server-side, `set-ro` flipping rw↔ro in place on a LIVE mount with no reconnect) + `config` command-shorthand e2e (auto-loaded `KEY=VALUE` file + env fallbacks: the flag > env > file > default chain for GROVEFS_HOME/META/DATA/TOKEN/DB_PATH, parser edge cases — comments, `export` prefix, quoted values, non-`GROVEFS_*` keys and malformed lines warned-and-ignored, an unreadable explicit `$GROVEFS_CONFIG` non-fatal — and a real token-gated FUSE mount whose coordinates come entirely from the file; 28 assertions) + `gc` offline orphan-object audit/reclaim e2e (41 assertions: a clean store scans clean; an out-of-band `rm meta/<view>.db` leaves its objects reported as ledger-held and NOT deleted; after `refs rebuild` they surface as orphans, the default age gate spares a fresh object and `--older-than 0 --apply` unlinks it; loose files outside the bucket layout are reported but never deleted; `--apply` refuses while THIS store has live mounts yet a same-named view mounted from another store does not block; a `--data-dir` sharing no object with any tree is refused, with `--force` clearing only that guard; a PG store and an invalid duration are refused; `--json` field shapes) + `ctlplane` remote-governance e2e (63 assertions: the CONTROL banner; the first admin key minted on the server host; remote `view`/`apikey`/`refs` governance with nothing written on the operator machine and a second empty home proving the tree lives server-side; `any`/`mount` and garbage keys refused on the port; a remote revoke taking effect immediately; no-key and mixed-local-flag refusals; `fork`/`graft`/`create --manifest`/`export`/`import` all SERVED over the wire, the last two as a dump that streams back in line-aligned chunks and an import the server installs itself; teardown guards still enforced remotely; the PG arm runs the same script) + `forksrv` three-backend remote fork/graft e2e (whole-tree copy with zero bytes moved, independent tree with structure writable and content EROFS, `--empty`+`graft` curation, the ledger ending at exactly zero) + `xfer` three-backend remote export/import e2e (the dump lands on the operator's disk, chunks cut on line boundaries, a 60 B and a 4 MiB window produce the same rows, `--as-fork`/`--replace`/guards, a line-for-line round trip and no ledger leak) + `gcremote` remote-gc e2e (the server reads its own trees and lists bytes from a data-server; `--apply` refuses while the SERVER has a live session and says a mount on another machine is invisible) + `pg` PostgreSQL backend e2e; the PG suite self-skips on hosts without a reachable PostgreSQL, so GREEN is preserved either way).

## Architecture

Three layers, all trait-decoupled:

```
┌─ FUSE mount process  (fs_impl.rs: GroveFS, &self, thread-safe)
│    ├── MetaProvider trait   view-tree access
│    │     ├── local:  MetaEngine  → <home>/meta/<view>.db
│    │     ├── remote: RemoteMeta  → meta-server (authoritative)
│    │     └── overlay: LayoutMeta → mount --layout (per-client presentation, decorates either)
│    │           ├── derived view: server-side LayoutMeta singleton → mount --view <name> (rules defined once via grovefs view define, stored in meta/views/)
│    │           ├── fork view: `fork <base>` → independent meta/<name>.db (structure writable, content EROFS, bytes shared)
│    │           └── pure alias: `alias <real view>` → public name maps 1:1 (full tree, writable; real name not mountable — name governance)
│    └── StorageBackend trait  object IO
│          ├── local:  LocalBackend  → <home>/blocks/<2hex>/<key>
│          ├── remote: RemoteBackend → data-server
│          └── striped: ShardedBackend → multiple data-servers
└─ Transport: custom binary frames over TCP (fixed header + variable body), pooled
```

- **Pluggable trait seams**: local/remote/striped are interchangeable; the interface surfaces, wire-protocol constants and the new-backend on-ramp are summarized in USAGE §16 (tiered walkthrough in docs.html §18/§19).
- **Central MetaServer**: one authoritative view per user shared across machines, so no diverging local copies.
- **Event bus + Notifier**: pushes invalidations after writes for content freshness.
- **fuser 0.18 multi-threading**: each worker gets its own `/dev/fuse` fd (`clone_fd`, Linux 4.5+), requests dispatch concurrently.

## Three questions about views: how to customize, where metadata lives, how permissions work

> A map of the three headline topics; each subsection points to its detailed sections below and in USAGE.

### 1. Customizing a view: a rule lens that reshapes presentation, never moving bytes

The only legal material for customization is the **base view's real display paths** (look them up first — walk the mount, or use MCP `list_view_tree`, which renders metadata without mounting; there is no standalone CLI tree-listing subcommand; never guess from the source layout, since `import` nests the source under a prefix like `/seeds`). Rule text: one verb per line (`#` comments):

```text
hide <src>          prune a subtree
only <src>          whitelist: serve only these subtrees (plus the ancestors needed to reach them)
map  <src> <dst>    move: the entry disappears from <src> and is served at <dst> (parent dirs synthesized)
link <src> <dst>    mirror: <src> stays and <dst> adds another name (same inode, multi-view presentation)
```

Four entrances share one `LayoutMeta` lens; only the rule source differs:

| Entrance | Rule source | Scope | Write semantics |
|---|---|---|---|
| `mount --layout F` | client-local file | this mount only | fully writable, writes hit the real entries |
| `grovefs view define --name D --base B --rules F` | server-side `meta/views/D.layout` | every client mounting `--view D` | derived mounts read-only, writes go to the base |
| `grovefs view fork --name X --base B [--empty]` | independent metadata tree `meta/X.db` (`--empty`: root row only) | its own organizable tree | structure writable, content `EROFS`, bytes shared |
| `grovefs view graft --name X --map P [--src S] [--dest D]` | a plan of `<src>[TAB<dst>]` lines | entries placed in the fork | one transaction, zero bytes copied |
| `grovefs view alias --name P --real R` | a single `alias <R>` line | public name 1:1 governance | full tree writable, real name refuses mounts |

The customized shape **does not have to resemble the base tree**: `<src>` is pinned to the base, `<dst>` is yours to invent; add `only <new-root>` to make the custom tree the whole view. The one thing a lens cannot do is invent content — every rendered entry is some base entry re-pathed, mirrored or pruned, plus synthesized scaffolding; bytes are never copied. Hot-reload: mounts through `meta-server` reshape within ≤2s (the server re-checks the definition's mtime per op); a bare local `--home` mount resolves once at mount time and needs a remount.

> Details: README "Per-client presentation / Server-side derived views / Pure aliases" (fork: USAGE §3.4.1), USAGE §3.1–§3.4.1; the agent workflow in README "Customizing data listings for agents" and `skills/grovefs-mcp/`.

### 2. Where view metadata lives: one engine per view, control plane follows the backend

```text
<meta-dir>/meta/
├── <view>.db            one authoritative tree per physical view (SQLite WAL): node table
│                        (id/ino, parent_id, name, is_dir, data_ref, link_target,
│                        mode/uid/gid, mtime/atime/ctime, UNIQUE(parent_id,name)
│                        + ino/parent/data_ref indexes) + ctrl + locks (POSIX lock leases)
├── refs.db              cross-view holder ledger for shared objects (GC's sole "last name" authority;
│                        derived/alias/fork share the base's bytes)
├── views/*.layout       derived/alias/fork definitions (derived = first line `base <B>` + rule
│                        lines; alias = a single `alias <R>` line; fork = a single `fork <B>` line)
└── api_keys.db          API key SHA-256 digests + call audit (the SQLite control plane; under PG
│                        it lives in `grovefs_control.api_keys` instead)
Data bytes: blocks/<2hex>/<key> (key = UUID, hash-fanned into 256 buckets, so per-directory
  entry count never grows with total objects)
PG backend (meta-server --meta-store postgres://…): one schema `grovefs_v_<hex(view)>` per view with
  the same node/ctrl/locks + the shared `grovefs_refs.holders` ledger; view trees then stop
  landing in .db files — and the control plane follows: view definitions, keys and audit all move
  into the shared `grovefs_control` schema (`view_defs`/`api_keys`/`mcp_calls`), leaving no
  control-plane files on disk. `grovefs view`/`apikey`/`mcp`/`data-server` take the same
  `--meta-store postgres://…` (or `GROVEFS_META_STORE`) to read and write that one system of record.
```

A write splits across two halves: metadata ops land in `node` rows, content writes land in `blocks/`, and `data_ref` is their only join point (striped objects encode `s<N>:<stripe_bytes>:<uuid>`). Backup checklists, the exact GC rule and cross-machine migration paths live in the detailed sections.

> Details: USAGE §5.0 (one-screen anatomy) / §5.1 / §5.3 / §5.4, README "Sharing across machines & storage backends" (SQLite↔PG is either/or), USAGE §3.4.2 (`view export/import` moves one tree across machines).

### 3. Managing permissions: a two-layer stack plus one revocable key

The admission stack has two layers — **token = cluster admission, view name = tree selection**. The `--view` string is not an identity: any uid mounting the same `--view alice` lands in alice's one tree. For **per-caller differentiated access**, add the third layer: revocable API keys (gate with `--require-api-key`, enforced server-side per op):

```text
M_OPEN / OP_OPEN (TCP frame handshake), gates in order:
① token        mismatch → E_AUTH (the only deterministic gate; client-reported uid is advisory)
② api-key      scope split (--for mcp|mount|any); revoked → refused; a key bound with --view opening
               another view → E_AUTH right at the handshake
③ read-only    a --ro key: every write-intent op on the meta plane → E_NOTPERM, content writes on
   write gate   the data plane → EROFS, lock ops → EPERM; M_OPEN replies "read-only mount" —
               never trusts the client's -o ro
④ view name → engine cache   opened at the first M_OPEN, shared by every caller of that view
Governance plane (grovefs mcp): the same key registry (SQLite: api_keys.db; PG:
grovefs_control.api_keys), scope must include `mcp`; every tools/call and
resources/read is audited; revocation freezes live mounts within ≤1s (op-driven re-check), while
historical calls keep their attribution.
```

Honest boundary: display governance (derived/alias/fork) is **not a security boundary** — it only reshapes the presented name tree; what is visible is readable and readable is writable back (a fork's content excepted). The real doors are the token and revocable keys. Keys store only SHA-256 digests and print their plaintext once; the control plane (keys, audit, view definitions) **follows the metadata backend** — SQLite keeps `api_keys.db` + `views/*.layout`, PG keeps everything in the `grovefs_control` schema; one system of record, never split.

> Details: README "Shared views & the permission model" + the API-key subsection, USAGE §6 (incl. §6.1 view-binding / read-only key measurements), §3.5.1 (ten-step key drill).

## Sharing across machines & storage backends (how it works, common questions)

> This section answers the two most-confused questions: (1) does "many servers sharing one directory tree over the LAN" **require PostgreSQL**, and (2) which side does PG actually run on?

### The full path of one write (data plane and metadata plane, half each)

GroveFS splits a file's *bytes* from its *directory tree* into two independent channels — this is precisely why it needs no NFS:

```
app write()/close()
   │
   ├─ data plane ─→ RemoteBackend ─TCP frame→ data-server ─→ blocks/<2hex>/<key>  (UUID-named, 256-bucket fan-out)
   │                (≥4MiB auto-sharded concurrently; large files can stripe across data-servers)
   │
   └─ metadata plane ─→ RemoteMeta ─TCP frame→ meta-server ─→ authoritative tree (SQLite by default, or PostgreSQL)
                      │  (create / rename / setattr / touch_mtime …)
                      └─ on success, the event bus pushes inval_entry + inval_inode to the view's other subscribers
                                → peer's next getattr/lookup sees the invalidation, re-fetches → visible immediately
```

- **Who holds authority**: `meta-server` is a **single authoritative serialization point** — one view's tree is written to the storage engine by that one process alone (SQLite: one `.db` per view; PG: one schema per view); every client's TCP frames converge on it. This is **deliberate** (it eliminates the view divergence you'd get from per-client copies), not an oversight.
- **Clients never touch storage directly**: a client opens neither the `.db` file nor a PG connection — it only speaks the GroveFS wire protocol. Whether the backend is SQLite or PG is **completely transparent** to clients; the mount command is byte-for-byte identical.

### Common questions

**Q: To mount one directory tree from many servers over the LAN, is PostgreSQL mandatory?**
**No.** The "many machines share one tree" capability comes from **`meta-server` + the same `--view`**, independent of whether the backend is SQLite or PG. The default embedded SQLite (`rusqlite bundled`, compiled into the binary, zero external deps) was built for exactly this topology — deploy `data-server` + `meta-server` on a central node and have clients `mount --view alice --meta-server <center>:50110 --data-servers <center>:50100`; they share one authoritative tree with real-time cross-client visibility. This whole path is proven on a pure-SQLite backend by the `metasrv` / `freshness` / `locks` suites — **you do not need to install PostgreSQL first**. The two backends are an **either/or** choice, not a layered deployment: one authoritative tree lives on exactly one backend — stay on SQLite and you never install PG at all; switch to PG and the server stops creating/reading any `<meta-dir>/meta/<view>.db` (the only ever-SQLite exceptions are the control-plane `api_keys.db` and derived-view `.layout` definitions, which are independent of backend choice).

**Q: Which side does PG run on?**
**Server-side only** (on the `meta-server`), as its **pluggable authoritative storage backend**:

```
client (just the grovefs binary)   server-side                 data tier
mount --view alice   ─TCP→        meta-server   ─postgres://→  PostgreSQL server
  · no PG lib / creds / awareness  · --meta-store is ITS flag       (same host as meta-server, or a dedicated DB box)
```

`--meta-store postgres://…` appears only on the `meta-server` subcommand; clients have no such option and never see the connection string. The PostgreSQL server process runs physically in the center (co-located with `meta-server` over loopback, or a dedicated DB machine reached over the LAN) but is **always server-side**.

**Q: So when is PG actually worth it?**
When you want to hand the authoritative tree's **durability / HA / backup / centralized operations** to a mature database: PG brings streaming / logical replication, PITR, `pg_basebackup`, monitoring and DBA tooling, and can tune very large metadata (millions of nodes) with indexes/VACUUM. Note **the in-tree HA (snapshot shipping) is mutually exclusive with PG** (see "Shared views & the permission model") — once on PG, use PG's own replication, not GroveFS's file-level snapshots.

> **Honest boundary**: the write model **differs** between the two engines — the default SQLite backend keeps `meta-server` a **single writer** per view (one authoritative connection + mutex, writes serialized); `--meta-store postgres://…` widens writes to a bounded **SERIALIZABLE write pool** (`PG_WRITE_POOL_SIZE = 8`, the HopsFS route with the transactional substrate handed to PostgreSQL's own SSI) — concurrent writes on disjoint `(parent_id, name)` rows commit in parallel, read-write overlaps abort (`SQLSTATE 40001/40P01`) and are **transparently retried** (200µs–25ms backoff, ≤12 attempts, never leaked to POSIX callers), proven by the `pg_ssi` (5 Rust tests) and `pg_parallel` (bash, 3 mounts × 45 concurrent creates) suites with auto-SKIP on PG-less hosts; governance invariants I1–I4 hold unchanged under any SSI-accepted schedule. Parallelism stays **within one PG cluster** — cross-cluster metadata partitioning is not done (hand that to PG's own replication/sharding). Metadata *read* parallelism spans **both** engines — SQLite `MetaEngine` and PostgreSQL `PgEngine` each keep their own read pool (SQLite: local mounts size it to `--threads`, default 4, embedded/meta-server 1, `GROVEFS_SQLITE_READ_POOL` overrides 0–16; PG: `PG_READ_POOL_SIZE` = 4) and page listings with SQL `LIMIT/OFFSET`. For default deployments, keep the zero-dependency SQLite; PG's incremental value is **write parallelism + durability / replication / operational maturity**.

## Build

**Prerequisites** (Linux): Rust (edition 2021) only. **No `pkg-config`, no libfuse development package** — the FUSE client is the pure-Rust `fuser` (its `libfuse*` features are not enabled); a full `PKG_CONFIG=/nonexistent cargo build --release --features fdb` succeeds and the resulting binary has no fuse library in `DT_NEEDED` (only `libfdb_c` plus five glibc libs). The metadata engine uses `rusqlite` with the `bundled` feature — SQLite is compiled from source, **no system dependency**. Mounting needs the kernel's `/dev/fuse`, plus the distro's setuid helper `fusermount`/`fusermount3` **when unprivileged** (with `CAP_SYS_ADMIN` it goes straight to `mount(2)`) — a userspace tool, not a development package.

```bash
cargo build --release          # artifact: target/release/grovefs
```

> Note: the repo still ships `.build-bin/pkg-config` and the suite scripts still `export PKG_CONFIG=…` —
> that is **legacy** (it mattered when `fuser` used the libfuse backend). No crate in the current
> dependency graph queries `pkg-config`, so the shim can stay or go without affecting the build.
> On a fresh machine just run:
> ```bash
> cargo build --release && export LC_ALL=C LANG=C
> ```

## Parameter meanings at a glance

> Names like `--home` / `--meta-dir` below are the literal CLI flags (copy them as-is); what each one actually points at:

| What it controls (plain meaning) | Flag |
| --- | --- |
| **Local storage root** — the directory holding `blocks/` (data) and `meta/` (views) in single-node mode | `--home <dir>` |
| **Server metadata root** — the host directory holding the authoritative view DBs, view definitions and API keys for `meta-server` | `--meta-dir <dir>` |
| **Data landing root** — where `data-server` writes bytes (defaults to the metadata root) | `--data-dir <dir>` |
| **Existing directory to take over** — data already on disk that you want to turn into a view | `--source <dir>` |
| **Mount point** — the local directory you use to reach the tree | `-m <dir>` (`--mountpoint`) |
| **Central service address** — the meta / data endpoints a client connects to (host:port) | `--meta-server <addr>` / `--data-servers <addr>` |
| **View name** — an arbitrary label for one logical tree (unrelated to Linux usernames) | `--view <name>` |
| **Cluster admission secret** — the shared token a server demands (data face and meta face can differ) | server `--token <str>` + client `mount --token <str>` |
| **Revocable per-caller credential** — the server admits a live API key instead (`grovefs apikey create --for mount`); listable, auditable per caller, and a revoke closes **already-established** mounts | server `--require-api-key` (+ `--meta-dir D` on `data-server`) + client `mount --api-key <ID:SECRET>` (or env `GROVEFS_API_KEY`) |
| **Mount as a given uid** — lock arbitration and audit record this uid as the entry owner (advisory) | `--run-as-uid <uid>` |
| **Read-only / zero-cache strong consistency** | `--ro` / `--no-cache` |
| **Directory-listing fast path escape hatch** — turn off READDIRPLUS (attributes shipped with each entry), falling back to plain READDIR + per-entry `LOOKUP`; A/B benchmarks and compatibility debugging only (mounts whose provider cannot page in its own store never negotiate READDIRPLUS, so this flag is a no-op there) | `--no-readdirplus` |

## Quick start (single-node, local)

```bash
Y=./target/release/grovefs
BASE=/data/myfs

$Y --home $BASE init --view alice
$Y --home $BASE init --view bob

$Y --home $BASE mount --view alice --mountpoint /mnt/alice   # detaches by default; --foreground to stay attached
$Y --home $BASE mount --view bob   --mountpoint /mnt/bob

mkdir -p /mnt/alice/projects/2026
echo "report" > /mnt/alice/projects/2026/report.txt
ls /mnt/bob                                # bob cannot see alice's files

$Y umount --mountpoint /mnt/alice
```

Seed a hierarchy: `init --view alice --manifest seed.txt` (lines of `mkdir <path>` / `touch <path>`, `#` comments, idempotent).

## Shrinking long commands: environment variables & the auto-loaded config file

Repeat-heavy flags don't have to be retyped. Every knob resolves through one chain — **explicit flag > live environment variable > config file > built-in default** — and on startup grovefs auto-loads a `KEY=VALUE` file into any `GROVEFS_*` variable not already set, searching `$GROVEFS_CONFIG` (explicit path) first, then `~/.config/grovefs/grovefs.conf`:

```bash
# ~/.config/grovefs/grovefs.conf (chmod 600 if it carries a secret — same contract as
# putting one in an environment variable)
GROVEFS_META_SERVER=192.168.1.10:50110
GROVEFS_DATA_SERVERS=192.168.1.10:50100
GROVEFS_TOKEN=s3cret

# before: grovefs --home /data/myfs mount --view alice -m /mnt/alice --meta-server … --data-servers … --token …
# after:  only "which view, where" is left on the line
grovefs mount --view alice -m /mnt/alice
```

Knobs resolvable from env/file: `GROVEFS_HOME` (`--home`), `GROVEFS_META_SERVER` / `GROVEFS_DATA_SERVERS` / `GROVEFS_TOKEN` (`mount --meta-server/--data-servers/--token`, and the servers' `--token` reads the same knob), `GROVEFS_META_DIR` (`meta-server --meta-dir`), `GROVEFS_API_KEY` / `GROVEFS_MCP_API_KEY` (the credential envs, also settable from the config file), `GROVEFS_STATS`. The syntax honors `#` comments, an optional `export ` prefix, and quoted values; non-`GROVEFS_*` keys and malformed lines print a `file:line` warning and are ignored. `--view`/`-m` are deliberately NOT file-servable — they differ per mount, and baking them into a default is the wrong shape. The detached-mount re-exec inherits the environment, so the shorthand survives daemonization. Regression: the `config` suite (28 assertions: the precedence chain, parser edge cases, and a real token-gated FUSE mount driven entirely from the file).

## About `--view` and per-view path capacity

- **`--view` is a view-namespace label, unrelated to Linux usernames**: any legal filename works (`alice`, `team-a`, `u42`, `research_group`). It is used only to (1) name `<home>/meta/<view>.db` and (2) tag the FUSE mount as `grovefs:<view>`. The binary does not call `getpwnam`, does not verify uid, does not require `useradd`. **Access authority comes from being able to reach `--home` / `blocks/` (or holding the server's `--token` / a live `--require-api-key` credential), not from the Linux user**. Under the same `--home`, distinct `--view` values map to distinct, mutually invisible views.
- **One user = one tree, unlimited paths inside**: once mounted you may `mkdir`/`touch` any number of files and directories at any depth under that mountpoint, exactly as in your own home. Per-user isolation applies to the whole tree, not to individual paths. `--manifest` is only a convenience for pre-seeding a skeleton, not a cap on path count or depth.

## Shared views & the permission model

GroveFS has exactly two authorization layers — **token = cluster admission, view name = tree choice**. `--view` is not an identity: any uid that mounts the same `--meta-server <addr>` with the same `--view alice` lands on alice's single authoritative tree and sees the other writers. That is intentional and makes "one view, many users" work out of the box.

```text
Client A (uid=1000)  mount --view alice ─┐
Client B (uid=2000)  mount --view alice ─┤ same --view = one shared tree; one client
Client C (uid=1000)  mount --view bob   ─┘ may mount many views at once (N:M)
        │
        ▼ TCP frame M_OPEN(view, token, uid, api_key)   (clients never open .db files or hold PG conn strings)
┌─ meta-server (single authoritative writer) — gates run in order, then routing
│  1) token gate    mismatch -> E_AUTH (cluster admission; the only hard gate; client-reported uid is advisory)
│  1') api-key gate  missing/invalid/revoked/wrong-scope -> E_AUTH; a key bound with --view is refused on any other view
│  1'') ro key gate   an --ro key is refused every write-intent op (E_NOTPERM; content writes EROFS at the data server)
│  2) view name -> engine cache   first M_OPEN opens the existing tree (never creates one); later mounts reuse it
└──┬──────────────────────────────────
   ▼ view=alice                          ▼ view=bob
  alice's authoritative tree (shared A+B, writes serialized)   bob's tree (C only, invisible to alice)
   ├─ SQLite (default):      one view = one DB file   <meta-dir>/meta/alice.db · bob.db
   └─ PostgreSQL (--meta-store):  one view = one schema    grovefs_v_616c696365 · grovefs_v_626f62

The unit of isolated storage is the *view*, not the client; content bytes live in the data-server's `<root>/blocks/<key>` (its `--data-dir`), referenced by data_ref inside each tree.
```

- **Many OS users sharing one view**: point every client at the same `mount --view alice --meta-server ... --data-servers ...` and both metadata (single SQLite/PG tree, writes serialized through the central engine) and content (single `blocks/`) converge in-place. If you need **differentiated rights per uid** inside that shared tree (e.g. an auditor mount that can list but never mutate), the recommended way is now a **view-bound / read-only API key** (`grovefs apikey create --for mount --view alice [--ro]` — enforced server-side, revocable, attributable); see “Locking the doors” below. The uid a mount presents is `geteuid()` by default and can be overridden with `mount --run-as-uid N` (handy for role-based mounts against a shared `meta-server`); it is recorded as the entry owner for lock arbitration and audit and is **advisory** — a TCP client can forge it, so it is never an access-control boundary. Real admission is the `--token` / `--require-api-key` gate, and “which tree, read-only or not” is hard-enforced server-side by a key's `--view`/`--ro` binding. A lock op (`flock`/`F_SETLK`, incl. lease renewal) writes the central `locks` table and counts as write-intent, so an `--ro` key is refused on it too (`E_NOTPERM`, surfacing as `EPERM` at the client — deliberately distinct from an HA standby's `E_ROFS` failover signal); a revoke is op-driven and so *does* reach live connections (re-checked about once per second under load).

- **Metadata backend is swappable: SQLite ↔ PostgreSQL** (this is the `meta-server`'s *authoritative* store only; the wire protocol clients see does not change):

  ```bash
  # default: embedded SQLite, one `<home>/meta/<view>.db` per view
  $Y meta-server --meta-dir /data/cluster --listen 0.0.0.0:50110

  # PostgreSQL: one `grovefs_v_<hex(view)>` schema per view, tx-guarded rename/unlink/lock
  $Y meta-server --meta-dir /data/cluster --listen 0.0.0.0:50110 \
       --meta-store postgres://user:pw@10.0.0.9:5432/grovefs
  ```

  Clients mount exactly the same way (`--meta-server host:port --view alice`). Notes that matter:
  - **PG is mutually exclusive with the in-tree HA flags**: `--meta-store postgres://…` combined with any of `--role standby` / `--peer` / `--witness` fails fast at startup — GroveFS-native HA ships snapshots via SQLite `VACUUM INTO`, which has no PG analogue. On PG, use PostgreSQL's own streaming / logical replication / PITR, which is strictly stronger than the file-snapshot mechanism anyway.
  - Tables are created idempotently on the view's first `M_OPEN` (`CREATE SCHEMA IF NOT EXISTS` + `CREATE TABLE IF NOT EXISTS`), so no pre-migration script is needed; the root row is pinned at `id=1` and `setval(pg_get_serial_sequence(...), MAX(id))` aligns the sequence, matching SQLite's `ROOT_INO` semantics exactly.

  Why Redis was *not* added: Redis's K/V+TTL model is not isomorphic to a view tree (rename/unlink/hardlink/refcount GC want transactions, lock arbitration wants row-level state). Emulating relational semantics on Redis would rebuild a worse PG at the application layer. **GroveFS ships PG, not Redis.**

## Per-client presentation: same data, different shapes (`--layout`)

View isolation answers "different views = different trees". `--layout` answers the other half: **one stored tree, customized display per client** — show only part of it, or reorganize entries into paths the client prefers. One rule per line (`#` comments, absolute paths; source paths address the stored tree):

| Rule | Effect | Typical use |
| --- | --- | --- |
| `hide <src>` | hide the whole subtree at its source spot | mask a private dir |
| `only <src>` | whitelist: show just these subtrees (ancestor dirs pass through) | expose only /docs |
| `map <src> <dst>` | relocate: the source spot vacates, the entry shows at `<dst>` (scaffold dirs synthesized as needed) | surface /data/raw as /team/raw |
| `link <src> <dst>` | alias: both spots show it, same inode (no byte copy, no extra refcount) | shortcut /proj/mylib |

Semantics under test: real entries keep their **real inodes** — reads/writes/locks/striping/GC pass through to the data plane; `rm` through the lens deletes the source entry; `hide`/`only` still filter *inside* relocated subtrees by source path; creating inside a purely synthesized scaffold dir gives `EPERM`, a plain create whose name collides with a lens-shown entry gives `EEXIST`; symlink target text is not rewritten. In `only` mode, writing into a pass-through ancestor outside the whitelist is `EPERM` as well.

```bash
$ cat .demo/lens.txt                     # client C's lens
hide /photos/private
map /data/raw /team/raw
$ grovefs --home .demo/home mount --view alice -m .demo/mnt --layout .demo/lens.txt &
layout overlay .demo/lens.txt: 1 hide, 0 only, 1 reloc rule(s)
mounting view 'alice' at .demo/mnt (base .demo/home)

$ ls .demo/mnt                                   # this client sees: data docs photos team
data  docs  photos  team
$ ls .demo/mnt/photos                             # private is gone
public
$ echo "hello via lens" > .demo/mnt/team/raw/note.txt   # write-through, no copy
$ ls .demo/mnt2/data/raw                          # a plain (unlensed) mount sees it at once
big.bin  note.txt
$ mkdir .demo/mnt/team                            # 'team' is lens scaffolding — name taken
mkdir: cannot create directory '.demo/mnt/team': File exists
```

Rules are parsed once at mount time (an invalid file fails fast rather than silently showing everything). The lens is a **per-mount-instance** presentation layer — nothing is persisted, no server state changes — so it composes with local `MetaEngine` or remote `RemoteMeta`, with `--ro`, and with a read-only view-bound API key. Against a shared `meta-server`, invalidations for entries living under synthesized dirs are broadcast by *source* path and may miss lens coordinates; add `--no-cache` (zero attribute/entry cache) in that setup.

## Server-side derived views: one copy of data, a shape per `--view` name (`grovefs view`)

`--layout` is configured per client. When shapes should be **managed centrally** — so that a team shares one display contract under a name — use derived views: `grovefs view define` stores the rules in the meta root (`<meta-dir>/meta/views/<name>.layout`, first line `base <physical view>`, remaining lines are the same four verbs as `--layout`), and clients simply `mount --view <name>` with **no local rules file**. Derived (virtual) views mount **read-only, always** — the shape is a centrally governed distribution, every write belongs to the original (base) view. One definition renders the same shape on every machine (a server-side `LayoutMeta` singleton per derived name, so virtual inodes agree across hosts). A local (`--home`) mount resolves the same `<home>/meta/views/<view>.layout` automatically when no `--layout` file is passed.

```bash
$ cat .demo5/docsonly.rules
# show only the docs subtree
only /docs
$ grovefs view --meta-dir .demo5/home define --name docsonly --base team --rules .demo5/docsonly.rules
defined derived view 'docsonly' over base 'team': 0 hide, 1 only, 0 reloc rule(s)
note: mounts of 'docsonly' are read-only (virtual shape) — write to base view 'team'
$ grovefs view --meta-dir .demo5/home define --name team --base team --rules .demo5/docsonly.rules   # name == base
error: derived view name must differ from its base
$ grovefs view --meta-dir .demo5/home list
view 'docsonly' -> base 'team': 0 hide, 1 only, 0 reloc rule(s)

$ grovefs --home .demo5/home mount --view docsonly -m .demo5/mnt &    # mount by name, no --layout
derived view 'docsonly' over base 'team': 0 hide, 1 only, 0 reloc rule(s)
note: derived (virtual) views mount read-only — write to base view 'team'
mounting view 'docsonly' at .demo5/mnt (base .demo5/home, read-only)
$ ls .demo5/mnt                              # the derived-name mount: just docs
docs
$ ls .demo5/mnt_full                         # the base name mounts the same, full tree (writable)
docs  photos
$ echo "new entry" > .demo5/mnt_full/docs/2026/b.md   # a write on the base tree…
$ ls .demo5/mnt/docs/2026                    # …shows in the derived view after the short TTL (immediate via a shared meta-server: pushes are double-broadcast)
a.md  b.md
$ echo "from lens" > .demo5/mnt/docs/2026/c.md        # writes through a virtual view are refused, always
bash: .demo5/mnt/docs/2026/c.md: Read-only file system
$ grovefs view --meta-dir .demo5/home rm --name docsonly
removed derived view 'docsonly' (underlying data untouched)
```

Guards and semantics: `define` refuses `name == base`, refuses a base that is itself a derived view (no nested lenses) and refuses to shadow an existing physical `<name>.db`; invalid rules fail fast (never degrading into "show everything"). **Virtual views are read-only by hard contract**: writes belong to the original (base) view only. The `meta-server` ships a derived-name flag in the `M_OPEN` ack, so the client lands a kernel read-only mount immediately (every mutation — and `flock` write locks — give `EROFS` with zero round-trips); the server additionally keeps an **independent authoritative per-op gate** on derived names (answering `E_NOTPERM`, deliberately *not* `E_ROFS`, which clients read as an HA failover signal), so old or crafted clients cannot bypass it; a local (`--home`) mount of a derived name enforces the same RO at mount time. **Edits hot-apply, no remount**: the `meta-server` checks the definition file's mtime before dispatching every op, so a replaced definition reaches established mounts within the client's cache TTL (≤2s) — and the read-only contract survives the reshape; a `view rm` deliberately does **not** hot-apply (live mounts keep the shape they opened with until remount — nobody has the tree yanked out from under them). Behind a shared `meta-server`, a derived view's subscribers register in **both its own bucket and the base name's bucket** and every write is broadcast to both, so base writes land in derived mounts even without `--no-cache` (verified by section 9 of the `layout` e2e: base writes visible in the derived mount, derived mounts refuse writes/deletes with `EROFS` and leave the base tree untouched while the base name stays writable on the same server, a re-define reshapes a live mount, and local derived mounts are read-only too). `grovefs view rm` deletes only the definition, never data. `grovefs view` operates on server-local files — run it where the `meta-server` lives (or share the same `--meta-dir`); under HA, `meta/views/` is control-plane config and must be synced to the standby by hand. **Base views are a first-class act**: `grovefs view create --view NAME` (equivalently `init --view NAME`) stands up a physical tree — it is the *only* kind of view whose deletes can free bytes, and only after the cross-view `refs` ledger (§Key features) confirms no other view still holds the object; derived views and aliases never own physical deletion. `grovefs refs --meta-dir D rebuild` repairs the ledger after an out-of-band `rm meta/x.db`.

Hot reload on one established connection (no remount anywhere in this transcript):

```bash
$ ls .demo3/mnt                             # docsonly = only /docs, mounted
docs
$ grovefs view --meta-dir .demo3/home define --name docsonly --base team --rules .demo3/broader.rules
view 'docsonly' already defined — replacing it (live meta-server mounts hot-reload it within their cache TTL)
defined derived view 'docsonly' over base 'team': 0 hide, 2 only, 0 reloc rule(s)
$ sleep 3
$ ls .demo3/mnt                             # same mount, shape widened by itself
docs  photos
```

## Pure aliases: name governance (`grovefs view alias`)

Derived views govern *what a view looks like*; **pure aliases** govern *what it is called* — the name is the alias, the mapping is the entry. The definition shares `<meta-dir>/meta/views/<name>.layout`, first line `alias <real view>`, no rule lines. The mirror image of a derived view: an **alias mount shows the FULL tree and is WRITABLE** (it simply is the real view under a public name), while the **real name stops being a mount entry point** — `meta-server` and local (`--home`) mounts alike refuse a governed real name, so public names rotate freely and the storage identity stays unexposed (guessing the real name no longer opens a door):

```bash
$ grovefs view --meta-dir $BASE alias --name teamwork --real team
defined pure alias 'teamwork' -> real view 'team'
note: 'team' is no longer a mount entry point while this definition exists (mounts by its real name are refused, locally and via meta-server); alias mounts show the FULL tree and are WRITABLE
$ grovefs mount --view teamwork -m /mnt/work --meta-server 10.0.0.5:50241 --data-servers 10.0.0.5:50240   # full tree, writable
$ grovefs mount --view team -m /mnt/t --meta-server 10.0.0.5:50241 --data-servers 10.0.0.5:50240          # the governed real name: refused
error: view 'team' is governed by an alias — mount the alias name instead (`grovefs view list` shows it)
$ grovefs --home $BASE mount --view team -m /mnt/t                                        # local mount: same contract
error: view 'team' is governed by an alias — mount the alias name instead (`grovefs view list` shows it)
```

When a public name leaks, rotate it in **one gapless command** with `--retire` (write the new alias before removing the old, so the real name stays governed throughout — there is no window where it becomes mountable by itself):

```bash
$ grovefs view --meta-dir $BASE alias --name teamwork2 --real team --retire teamwork   # old public name leaked, swap to teamwork2
rotated public name for real view 'team': 'teamwork' -> 'teamwork2' (gapless — 'team' stayed governed throughout, 'teamwork' no longer resolves)
note: legitimate users just remount under 'teamwork2'
$ grovefs mount --view teamwork -m /mnt/leak --meta-server ... --data-servers ...                     # old leaked name remounts as an empty view (never the real tree)
```

Guards and semantics: `alias` rejects `name == real`, shadowing a physical `<name>.db`, derived-view targets, and alias chains (resolution is single-hop by design); one real view may carry at most one public name (`--retire` rotates within one command — write-before-remove, so it is never blocked by this invariant — and refuses a `--retire` pointing at a derived view or a non-existent alias, so a shape definition can never be dropped by accident). `grovefs import` refuses an alias target — an alias is not a storage identity, and writing a separate `<alias>.db` would create a shadow tree nothing reads. `view rm` on an alias reopens the real name for NEW mounts (established connections are untouched); rotation/rm only affect NEW mounts, and a retired name resolves to a fresh empty namespace that never reaches the real tree. Note the honest boundary: hiding/rotating the real name is name governance (leak hygiene), not a security mechanism — public names must be distributed to be used and rotation cannot revoke an existing token-holder's access, so real isolation stays token + api-key. (Verified by sections 10/11 of the `layout` e2e suite — 26 alias assertions; see USAGE §3.4 for full transcripts.)

## Customizing data listings for agents: the MCP interface (`grovefs mcp`)

View governance is also a set of **MCP (Model Context Protocol) tools**: `grovefs mcp` speaks newline-delimited JSON-RPC 2.0 on stdio (the `initialize` / `ping` / `tools/list` / `tools/call` core of the 2025-06-18 protocol revision), so any MCP client — Claude Desktop, an IDE agent, any agent runtime — spawns it as a subprocess and lets the agent **customize a data listing's shape programmatically**: list views, inspect the real directory listing a view shows (`list_view_tree`, which renders a derived view through its own lens and can preview an unpublished `base`+`rules` lens without committing), read back a single definition (`get_view`), dry-run a whole candidate definition against every governance guard without writing (`validate_definition`), publish a read-only derived lens from inline rule text (the four `--layout` verbs `hide`/`only`/`map`/`link` travel as text lines — the agent never touches a file), rotate public names, remove definitions. Every transcript below is captured from a real `mcp`-suite run:

```bash
$ grovefs mcp --meta-dir /data/xy/grovefs/.demomcp/home    # spawned by the MCP client; stdin/stdout are the channel
# agent-side tools/call, tool result text shown verbatim:
[list_views]   {"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_views","arguments":{}}}
{
  "db_path": "/data/xy/grovefs/.demomcp/home",
  "defined": [],
  "physical": [
    "team"
  ]
}
[preview_rules] "rules": "bog /x"                        # preview before publishing: a bad verb is caught first
bad layout rules: line 1: unknown layout verb 'bog'
[define_derived_view] {"name":"release","base":"team","rules":"# public release: no private photos\nhide /photos/private"}
defined derived view 'release' over base 'team': 1 hide, 0 only, 0 reloc rule(s)
note: mounts of 'release' are read-only (virtual shape) — write to base view 'team'
# user side mounts the agent-defined name directly (no --layout, no local file):
$ grovefs --home $HOME_DIR mount --view release -m .demomcp/mnt &
derived view 'release' over base 'team': 1 hide, 0 only, 0 reloc rule(s)
note: derived (virtual) views mount read-only — write to base view 'team'
$ ls .demomcp/mnt
docs  photos
[list_view_tree] {"view":"team"}                       # discover the real paths first (reads local metadata, mounts nothing, touches no data bytes): rules written against these, not guesses
{"kind":"physical","rendered_from":"team","dir_count":4,"file_count":3,"truncated":false,"entries":[
  {"path":"/docs","kind":"dir"},{"path":"/docs/2026","kind":"dir"},{"path":"/docs/2026/b.md","kind":"file"},
  {"path":"/docs/a.md","kind":"file"},{"path":"/photos","kind":"dir"},{"path":"/photos/private","kind":"dir"},
  {"path":"/photos/private/x.jpg","kind":"file"}]}
[list_view_tree] {"view":"release"}                    # rendered THROUGH the published lens: display paths /docs /docs/2026 /docs/2026/b.md /docs/a.md /photos (/photos/private pruned — exactly what the mount shows)
[list_view_tree] {"base":"team","rules":"only /docs"}  # preview an unpublished lens without writing any .layout: only the /docs subtree renders
[validate_definition] {"name":"release","base":"team","rules":"hide /photos/private"}  # pre-publish dry run: walk the whole name+base+rules through every governance guard, write nothing, show the verdict
{"accepted":true,"dry_run":true,"kind":"derived","hide":1,"only":0,"reloc":0,"would_replace_existing_definition":false,"base_view_db_present":true,"note":""}
[get_view] {"name":"release"}                          # read a single lens's raw rules back for read-modify-write iteration
{"kind":"derived","base":"team","hide":1,"only":0,"reloc":0,"definition":"base team\nhide /photos/private","mount":"read-only (virtual shape; writes go to the base view)"}
[list_views] again                    # structured JSON: name/kind/real_view/rule counts
view 'release' -> base 'team': 1 hide, 0 only, 0 reloc rule(s) (read-only)   # CLI cross-check: byte-identical wording
[define_derived_view] {"name":"team","base":"team",...}                        # name == base
isError → derived view name must differ from its base               # the CLI's refusal string, verbatim
```

Semantics and boundaries: ten tools — eight governance ops (`list_views` / `define_derived_view` / `alias_view` / `fork_view` / `graft_view` / `remove_view` / `preview_rules` / `validate_definition`) plus two read-only introspection tools (`list_view_tree` / `get_view`) — and the `grovefs view` subcommands share **one library implementation** (`grovefs::mcp`): validations, refusal strings and file mutations are byte-identical, one contract with two entrances — an agent cannot do anything the CLI cannot, nor bypass a guard (derived mounts stay read-only, aliases keep refusing the governed real name, `--retire` stays write-before-remove, `fork_view` copies namespace rows only — never bytes — into an independent per-view metadata db whose structure is writable while content stays `EROFS`, and `remove_view` on a fork drops only that tree and rebuilds the shared ledger; hot-reload likewise belongs to the `meta-server` entrance — a local mount resolves its definition once at mount time). `list_view_tree`/`get_view` are **metadata-only introspection**: they open a view's tree read-only through whichever store the server fronts (SQLite `meta/<view>.db`, a PG `grovefs_v_<hex>` schema, or an `fdb://` subspace) and walk it through the same `LayoutMeta` lens a mount uses (no mount, no object bytes), so an agent writes rules against real display paths and previews a shape before committing it; a tree that store cannot reach is refused by name rather than half-walked, and the `size` in a rendered entry is the metadata field rather than a byte count (dirs 4096, files 0, symlinks their target length) — stat through a mount for real bytes. `validate_definition` is a pre-publish dry run: it walks a whole (name, base, rules) through the full `define_derived_view` guard set — invalid names, name==base, shadowing a physical db, replacing a pure alias, bad rule text — returning the exact byte-identical refusal a real publish would produce, or an `accepted` JSON verdict, and it writes NOTHING. Governance failures come back INSIDE the result (`isError: true` + the CLI text) per MCP convention; only protocol-level faults use JSON-RPC errors. Notifications (no id) are never answered; a malformed frame gets `-32700` and the stream recovers. **Control plane only**: it never mounts and never writes data. By default it trusts whoever can spawn the process (the same OS-level boundary as a local mount); cross-machine auth remains the server's token / API key. When it fronts many distinct callers, `grovefs mcp --require-api-key` opens an opt-in API-key gate (next subsection) — the same credential family, extended by scope to the mount/data doors as well. Wire-up: register `grovefs mcp --meta-dir D` as a stdio server in the client config (a per-call `db_path` argument overrides the default). E2E: the `mcp` suite, 65 assertions (handshake/discovery/inline-rule reshapes/path discovery + shape preview/pre-publish dry-run/alias rotation/fork lifecycle/CLI-identical wording/protocol error paths) and the `apikey` suite, 33 assertions; see USAGE §3.5. The agent-side operating manual (in English, for international deployments) ships with the repository under `skills/grovefs-mcp/` (`SKILL.md` + `reference.md` + `examples.md`, plus the dependency-free stdio client `scripts/mcp_call.py` and `scripts/smoke.sh`, a connection self-test that runs the whole workflow against a throwaway store).

### Locking the doors: API-key auth, scopes, view binding / read-only, and call audit (`grovefs apikey`)

One server often faces MANY distinct callers — an agent runtime fronts a `grovefs mcp`, several client machines mount the same `meta-server` over the LAN — and that needs revocable credentials plus per-caller attribution. `grovefs apikey` mints keys (**stored as SHA-256 only** — the plaintext prints once at `create`, so a leaked `api_keys.db` yields nothing usable) and tags each with a **scope**: `--for mcp` (opens the governance interface only), `--for mount` (opens a `--require-api-key` meta/data server only), `--for any` (the **default**; every key minted before scopes existed is this one, so older deployments need no migration). Both entrances share ONE registry (`<meta-dir>/meta/api_keys.db`); the scope keeps them isolated — a mount credential cannot be replayed against the governance API, an agent key cannot open the data plane. A mount key can additionally carry **two access-control dimensions**: `--view V` binds the credential to ONE physical view — `M_OPEN`ing any other view is refused at the handshake with `E_AUTH` (a system-level key without `--view` opens any view); `--ro` marks it read-only — the server refuses every write-intent op (`E_NOTPERM` on the meta plane, `EROFS` on content writes at the data server) and the `M_OPEN` ack tells the client to mount read-only, so read-only is enforced rather than trusted. **Governance gate**: `grovefs mcp --require-api-key` gates every method that reads or writes governance state — `tools/list`, `tools/call`, `resources/list`, `resources/templates/list`, `resources/read`, `completion/complete` (`initialize` / `ping` / `prompts/*` / `logging/*` always stay open so a client can handshake and show its key). **Mount gate**: `meta-server --require-api-key` gates `M_OPEN` and `data-server --require-api-key` gates `OP_OPEN` (`--meta-dir` points the data server at the metadata root, defaulting to its first `--data-dir`); the client presents `mount --api-key <ID:SECRET>` (or exports `GROVEFS_API_KEY` so the secret never lands in argv). **Audit gate**: every `tools/call`, every `resources/read` and every mount connection — refusals included — is appended to one table (timestamp, key, plane (`method` = meta|data|the MCP method), tool, success, detail) that `grovefs apikey calls` reads back. Revoking only marks the row `revoked` (past calls keep their attribution); a revoked key immediately fails authentication **and closes mounts that are already up**. Storage is its own local SQLite file (`meta/api_keys.db`), control-plane state. Transcripts below are from a real run:

```
$ grovefs apikey --meta-dir <home> create --label laptop-alice --for mount   # mint + print the plaintext <id>:<secret> once
apikey created — shown ONCE (only its SHA-256 is stored; save it now):
  id:    aac1f51a0d5641ddaa8dbb83dc1cf646
  label: laptop-alice
  scope: mount
  view:  <any>
  rw:    rw (read-write)
  key:   aac1f51a0d5641ddaa8dbb83dc1cf646:a5d5450a7cf4...(64 hex secret)

$ grovefs apikey --meta-dir <home> create --label reader --for mount --view alice --ro   # view-bound + read-only
apikey created — shown ONCE (only its SHA-256 is stored; save it now):
  id:    7c0e...
  label: reader
  scope: mount
  view:  alice
  rw:    ro (read-only)
  key:   7c0e...:…            # opens view alice only; every write is refused server-side

# using a bound/read-only key where it doesn't belong is refused outright:
$ grovefs mount --view bob -m /mnt/b --meta-server ... --data-servers ... --api-key "$READER_KEY"
error: permission denied: this api key is bound to view 'alice', not 'bob'   # E_AUTH at M_OPEN
$ mkdir /mnt/a/should_fail                                                          # alice's read-only key
bash: /mnt/a/should_fail: Read-only file system                                     # kernel RO from the handshake flag; the server also refuses every write

$ grovefs apikey --meta-dir <home> create --label x --for both              # a typo never silently widens a privilege
error: unknown api key scope 'both' (expected any|mcp|mount|admin)

$ grovefs meta-server --meta-dir <home> --listen 0.0.0.0:50110 --require-api-key
meta-server: api-key auth REQUIRED (present `mount --api-key`, minted by `grovefs apikey create --for mount|any`); a revoke closes live mounts on their next op

# refusal reasons travel byte-for-byte with the handshake (not just an error code):
$ grovefs mount --view alice -m /mnt/a --meta-server ... --data-servers ...                       # no key at all
error: no api key presented: this server requires one (pass `mount --api-key ID:SECRET` or export GROVEFS_API_KEY; mint with `grovefs apikey create --for mount`)
$ grovefs mount --view alice -m /mnt/a --meta-server ... --data-servers ... --api-key "$MCP_KEY"  # an agent key mounting the view
error: authentication failed: this api key is scoped to 'mcp', not 'mount'

$ grovefs apikey --meta-dir <home> calls --limit 6        # mount connections and governance calls, one table
last 6 call(s), newest first:
ts   key_id   method   tool   ok   detail
...  aac1f51a0d5641ddaa8dbb83dc1cf646  meta  M_OPEN   yes  view='alice' uid=1005
...  aac1f51a0d5641ddaa8dbb83dc1cf646  data  OP_OPEN  yes
...  -                                 meta  M_OPEN   no   view='alice': authentication failed: this api key is scoped to 'mcp', not 'mount'

$ grovefs apikey --meta-dir <home> revoke --id aac1f51a0d5641ddaa8dbb83dc1cf646   # the row stays for historical attribution
apikey: revoked id='aac1f51a0d5641ddaa8dbb83dc1cf646' (label 'laptop-alice'); it can no longer authenticate, past calls keep attribution
  a LIVE mount presenting this key is refused on its next op (within the 1s re-check window) and unmounts itself;
  a `grovefs mcp` session in flight keeps its identity until it reconnects.

# closing a live mount, measured (no operator action needed):
$ printf 'late' > /mnt/a/new.txt
bash: /mnt/a/new.txt: Permission denied                     # EACCES, carrying the server's reason
$ cat /mnt/a/grovefs-mount.log
  grovefs: access refused by the server (authentication failed: this api key has been revoked) — unmounting this view
  received termination signal, unmounting cleanly...
  unmounted view 'alice' at /mnt/a                          # afterwards /mnt/a is an empty dir, not a stale endpoint

# the governance gate's shape: an initialize without apiKey is answered, the next tools/list is refused
$ grovefs mcp --meta-dir <home> --require-api-key
{"jsonrpc":"2.0","id":1,"result":{"serverInfo":{"name":"grovefs"},...}}   # initialize answered (the gate holds only the state-touching methods)
{"jsonrpc":"2.0","id":2,"error":{"code":-32001,"message":"authentication required: present a valid apiKey in the initialize params (or export GROVEFS_MCP_API_KEY for the server)"}}
# with "apiKey":"<id>:<secret>" the session authenticates and the tool surface opens:
[tools/call define_derived_view {"name":"release","base":"team","rules":"hide /photos/private"}]
defined derived view 'release' over base 'team': 1 hide, 0 only, 0 reloc rule(s)

$ grovefs apikey --meta-dir <home> list                            # id/label/scope/view/rw/created/last_used/revoked — never the secret
```

A client that cannot customize its handshake is covered by exporting `GROVEFS_MCP_API_KEY='<id>:<secret>'`, which pre-authenticates the whole governance server at launch (`GROVEFS_API_KEY` is the mount-side counterpart). Honest boundary: the key authenticates **the caller at the door** — it lets (or stops) a caller issuing governance writes, lets or stops it opening a view tree, and makes each admission attributable — without changing governance semantics; its `--view`/`--ro` dimensions are enforced server-side. Live-connection re-checking is **op-driven** (the server only re-verifies when traffic arrives), so the revocation boundary is “the first server round-trip after the revoke”: reads fully answered by the kernel cache need no round-trip and are therefore not interrupted (the next round-trip is). `--token` and `--require-api-key` can both be set — then both gates must pass. With `--require-api-key` off the server audits calls but gates nothing. E2E: the `apikey` suite, 33 assertions (create/list/hash-not-on-disk/refuse-without-key/refuse-wrong-secret/admit-with-valid-key/call-attribution/revoke-fails-closed-but-stays-listed/prefix-revoke/env-pre-auth/default-ungated-regression/set-ro in-place ro↔rw flip with authentication kept) and the `mountkey` suite, 76 assertions (scope minting + typo refusal/refusal reasons in the protocol/admission and env fallback (secret absent from argv)/connection audit down to key and view/scope isolation both ways/revocation closing a LIVE mount with no stale endpoint/other keys unaffected/token+key double gate/independent data-plane gate/failover carrying the credential/default ungated path unchanged/a view-bound key refused across views with its refusal audited/an `--ro` key's writes refused server-side/`set-ro` hot-flipping rw↔ro on a live mount with no reconnect); see USAGE §3.5.

## Importing existing data: zero-copy passthrough (`grovefs import`)

When the server already holds a pile of data under some directory and clients should get it through a view, there is no need to “stream it in” first: `grovefs import` registers an **existing local directory tree** into a view with the bytes staying exactly where they are —

```bash
$ tree .demo4/legacy
.demo4/legacy
|-- docs
|   `-- quarter.txt
`-- photos
    `-- 2026
        `-- y.jpg

3 directories, 2 files

$ grovefs --home .demo4/home import --view legacy --source .demo4/legacy --dry-run
dry run: view 'legacy' under /legacy would mirror 3 dir(s) and 2 file(s) (symlinks/special files and names already in the view are skipped on the real run)
$ grovefs --home .demo4/home import --view legacy --source .demo4/legacy
created BASE view 'legacy' (target was absent); imported 2 object(s)
imported into view 'legacy' under /legacy: 3 dir(s), 2 object(s) (2 hardlinked zero-copy, 0 copied), 0 skipped, 0 failed

$ grovefs mount --view legacy -m .demo4/mnt &    # a client mount now sees the tree
$ ls .demo4/mnt/legacy
docs  photos
$ stat -c '%y' .demo4/mnt/legacy/docs/quarter.txt   # the file's own clock, not import time
2021-05-06 07:08:00.000000000 +0000

$ echo "edited via the mount" > .demo4/mnt/legacy/docs/quarter.txt
$ cat .demo4/legacy/docs/quarter.txt           # the ORIGINAL source path, same inode
edited via the mount

$ rm .demo4/mnt/legacy/photos/2026/y.jpg       # delete a name inside the view
$ ls .demo4/legacy/photos/2026/                # the user's dataset is untouched
y.jpg
$ find .demo4/home/blocks -type f -links 2 | head -1   # the object was just a second link
```

Semantics (34 assertions in the `import` suite): **zero copies on one filesystem** — object and source share the inode (nothing shows up in `df`), with an honest streamed-copy fallback across devices; **live passthrough both ways** — an edit through the mount or an external rewrite of the source file shows up on the other side within the client’s ≤2s TTL; **GC only unlinks** — removing the name in the view deletes only the `blocks/` path, never the source; **idempotent and incremental** — re-running registers just the new names; mtime/atime/mode/owner preserved (`--dry-run` does not even create the view DB). Imported entries are then first-class: reads/writes, locks, snapshots and invalidation push all apply, and derived views/lenses can reshape them into virtual directories with `map`/`link` — that virtual shape then mounts **read-only** (writes go to the imported base view, visible through the shape via the double-broadcast pushes), and `grovefs import` refuses a derived target name outright, since it would write a shadow `<view>.db` no mount ever reads. Like `grovefs view`, run it on the machine holding the store (the `--home`, or the meta/data servers’ root).

## Production topology: central storage + LAN mounts

```bash
# central node
$Y data-server --listen 0.0.0.0:50100 --data-dir /data/cluster
$Y meta-server --meta-dir /data/cluster --listen 0.0.0.0:50110

# each client (a remote mount needs no --home: nothing authoritative lives on the client; no local writes at all)
$Y mount --view alice \
     --data-servers 10.0.0.5:50100 --meta-server 10.0.0.5:50110 -m /mnt/alice
```

| Option | Effect |
| --- | --- |
| `--data-servers host:port` | File IO shipped over the network to `data-server` (no NFS) |
| `--meta-server host:port` | View served by the central `meta-server` → same user across machines shares one tree |
| `--data-servers a,b,c` | Comma-separated list enables deterministic cross-server striping |
| `--meta-server primary,standby` | Failover list; falls over automatically when the primary dies |

## Consistency

| Mode | Command | Behavior |
| --- | --- | --- |
| **Real-time invalidation push** (default, recommended) | just `--meta-server` | After a client's write succeeds, the center broadcasts invalidation (`inval_entry`+`inval_inode`) to that user's other subscribers; peers see it immediately, no polling |
| `--no-cache` strong-coherence fallback | add `--no-cache` | Disables kernel attr/dentry cache and reads straight through (`FOPEN_DIRECT_IO`). Trade-off: no client caching, more round-trips |

Invalidations must be sent in pairs (`inval_entry` + `inval_inode`) or peers may hold stale attributes.

## Distributed advisory locks

`flock` / `fcntl` are arbitrated through a central `locks` table (SQLite), effective across processes and machines. Reclaim paths:

1. **Holding process crashes** — the kernel closes its fd, triggering FUSE `release` (carrying the matching `lock_owner`); the lock row is deleted **immediately**, far below the lease.
2. **Whole mount daemon crashes** — no `release`, no renewal; the lock expires within a **short lease (default 15s)**.

Live holders are kept alive by a **renewal heartbeat** (every lease/3), so they **never time out**.

> **Scaling boundary**: a blocking lock wait polls synchronously inside the mount process (100ms cadence, capped at 10 minutes) and occupies one FUSE worker thread; with the default `--threads 4`, ~4 concurrent waiters on the same mount start queuing other requests — raise `--threads` for lock-heavy workloads; event-driven wake-up is on the roadmap.

> **Do not judge liveness by pid**: in `( flock -x 200; sleep N )` the `setlk` pid is a transient helper (the lock lives on the inherited open-file-description), so a pid-alive check would wrongly kill live locks; `flush` fires on every close with a mismatched owner and is unusable for reclaim. Only `release` (last-close) is the authoritative signal that a holder terminated.

## High availability

```bash
# primary
$Y meta-server --meta-dir /data/nodeA --listen 10.0.0.5:50110 --role primary --peer 10.0.0.6:50111
# standby
$Y meta-server --meta-dir /data/nodeB --listen 10.0.0.6:50111 --role standby --peer 10.0.0.5:50110
# primary with RPO=0 synchronous replication
$Y meta-server --meta-dir /data/nodeA --listen 10.0.0.5:50110 --role primary --peer 10.0.0.6:50111 --sync-repl
# true fencing via a majority of independent arbiters (comma-separated list, odd N)
$Y meta-server --meta-dir /data/nodeA --listen 10.0.0.5:50110 --role primary --peer 10.0.0.6:50111 --witness 10.0.0.7:50100,10.0.0.8:50100,10.0.0.9:50100
```

- The two nodes must use **separate `--meta-dir`s** (each keeps its own copy; replication ships whole snapshots).
- `--sync-repl` (opt-in): each write blocks until the standby durably installs the snapshot and ACKs, dropping **RPO from ~500ms to 0**; on ACK timeout it honestly degrades to async with a warn (never falsely reports 0).
- `--witness` (opt-in, **fail-safe fencing**): one or more `data-server`s reachable by both halves (a comma-separated list; an odd count `N` is recommended, and `N=1` is simply a single witness) act as independent arbiters. Authority needs a strict majority `quorum_size(N) = N/2 + 1` to grant: the meta side fans out to the whole list concurrently and applies majority logic (`witness_quorum`). Because two majorities always intersect, only the first claimer can raise a shared arbiter (the later sees `prev == epoch`, a renew, not an advance), so a same-value race yields exactly one primary. Each arbiter keeps a monotonic fence token; `OP_FENCE` folds your epoch via `fetch_max` and returns the **value held before the fold (`prev`)**. A claim wins iff `prev < your epoch`; `prev >= your epoch` means you are fenced — returning `prev` (not the post-fold max) is exactly what resolves same-value collisions so a recovering arbiter never ends up with two primaries. A live primary renews a bounded lease at a majority every tick (self-demotes if a majority reports `prev > mine`); each arbiter refuses overlapping leases — at most one live lease at a time — so a majority live lease is itself proof of exclusive authority, and a write is admitted from a local lease check (one fan-out per renewal, not per write). A would-be promoter may seize a lease only after the incumbent's expires (it refuses if `prev >= candidate`, a tie or peer-ahead, or the incumbent lease is still live). **Fail-safe**: if a configured witness set cannot form a lease quorum beyond a 3s grace, a node gives up write authority (primary self-demotes, standby refuses to promote → writes freeze) rather than risk split-brain. **Availability gain over a lone witness**: losing a *minority* of arbiters neither freezes writes (a quorum is still reachable) nor resurrects a stale primary (a quorum still holds the monotonic token even if a minority of `.fence_token` stores are lost); only losing a *majority* freezes. Each arbiter persists its token at `<root>/.fence_token`, so a restart does not zero it. Beyond the meta-plane proof, the DATA plane is fenced too: the client stamps its live epoch onto every write-intent content frame (create/write/truncate/remove/fsync) and the witness data-server refuses one whose epoch lags its token (`EROFS`), since a `write()` otherwise lands bytes on the backend before the debounced meta touch — a fenced frame re-pulls the epoch from the authority and retries once. The data-server furthermore linearizes the epoch check and the backend write against a promotion (a claim takes an exclusive lock that drains in-flight writes, and the witness co-locates with storage), so a stale write can never land concurrently with the new primary's — content torn-writes are closed on the arbiter, not just bounded (this content gate is effective only on data-servers inside the `--witness` list, so deploy the witness co-located with the storage clients actually write to; a content store outside the set keeps a zero token, its data-plane gate stays inert, and only the meta-plane fence protects it).

## Auth / read-only / performance

**Token auth** covers both the data plane (`OP_OPEN`) and the metadata plane (`M_OPEN`); a mismatch fails at connect with `PermissionDenied`. Empty token (default) = LAN-trusted, no auth — trusted networks only.

**A revocable per-caller credential** guards the same two doors, but audits and kicks one caller at a time: a token is one shared secret — every host holding it can mount, nobody sees *who* mounted, and ejecting a caller means rotating the token and breaking everyone. Mint an API key instead (`grovefs apikey create --for mount`, opens only the mount/data doors):

```bash
$Y apikey --meta-dir /data/cluster create --label client-a --for mount    # plaintext prints once; only SHA-256 is stored
$Y meta-server --meta-dir /data/cluster --listen 0.0.0.0:50110 --require-api-key
$Y data-server --listen 0.0.0.0:50100 --data-dir /data/cluster --require-api-key --meta-dir /data/cluster
$Y mount --view alice --data-servers ... --meta-server ... --api-key '<id>:<secret>' -m /mnt/alice   # or export GROVEFS_API_KEY to keep it out of argv
$Y apikey --meta-dir /data/cluster calls --limit 20                       # every connection (refusals too) attributed to its key
$Y apikey --meta-dir /data/cluster set-ro --id <id> --rw                    # flip an --ro key rw↔ro in place; a live mount follows within ≤1s, no reconnect
$Y apikey --meta-dir /data/cluster revoke --id <id>                       # closes that key's LIVE mounts (seconds)
```

The two substantive differences from a token: ① refusal reasons travel byte-for-byte back with the handshake (missing key / unknown id / wrong secret / wrong scope / revoked each print their own text), and the server **re-checks live connections once a second** — after a revoke that mount is interrupted on its first server round-trip, prints the server's own wording and unmounts itself cleanly (no `Transport endpoint is not connected` left behind), while mounts on other keys keep running; ② mount connections and governance calls share one audit table. `--token` and `--require-api-key` can both be set (then both gates must pass); with the flag off behaviour is exactly as ungated (auditing still happens). See USAGE §3.5.1/§6.

**Read-only**: `mount ... --ro` → every mutating op returns `EROFS` (kernel `MS_RDONLY` + filesystem-side guard).

**Performance knobs**: `--threads N` (default 4, per-thread `/dev/fuse` fd), chunked parallel IO (reads ≥1MiB, writes ≥4MiB), large-file striping (≥16MiB), `touch_mtime` debouncing (one write per inode per epoch second, which is the store's whole resolution; a fresh file starts with its creation second already stamped, and the kernel's close-time `SETATTR` echo is dropped when its mtime falls in the stored second and its size already matches the object, so create+write+close pays no extra metadata write at all), creates that reserve only the name (the backend object is opened by the first `write`/`truncate`, so a `touch`-style create never opens a file at all — 0.177ms/op single-threaded, and 3.3k files/s at T=1 / 12.2–12.6k at T=8 with one parent dir per thread; an unwritten file simply reads back as 0 bytes), a warm read pool per view engine on both backends (SQLite: the mount sizes it to `--threads`, default 4, embedded/meta-server 1, `GROVEFS_SQLITE_READ_POOL` overrides 0–16; PG: `PG_READ_POOL_SIZE` = 4; drained slots open lazily and are returned on drop, and each PG read connection caches its own prepared statements, ≤16) that unblocks concurrent metadata reads while writes stay serialized, an opt-in async-commit durability tier for PostgreSQL writers (`GROVEFS_PG_SYNC_COMMIT=off`, default on: +29% at N=4, at the cost of losing the last commits on a crash), creation replies that carry the new row so a create costs two metadata round trips instead of three, SQL-paged directory listings (`readdir_page`, `LIMIT/OFFSET` filled into the reply buffer in geometric chunks — 32→1024 for READDIRPLUS, 128→1024 for plain READDIR), and `FUSE_READDIRPLUS` negotiation (on by default wherever the provider pages in its own store — SQLite/PG local, and remote against a capable `meta-server` via `M_READDIRP`; `--no-readdirplus` to fall back to plain READDIR + per-entry LOOKUP).

## Deployment & operations (hands-on)

The full step-by-step guide lives in **[USAGE.md](USAGE.md) §11–§14** (mirrored in [docs.html](docs.html), re-sequenced into a four-tier zero-to-expert manual with new beginner-prerequisites, internals & extension-development, and performance-tuning methodology sections, plus an auto/light/dark reading-theme toggle): §11 walks through environment preflight → build & install → topology planning → shared token → systemd units (two central services + a templated client mount unit) → firewall → an 8-step acceptance checklist → HA / PostgreSQL variants; §12 covers routine inspection (log-keyword → action table), backup/restore (single-node / SQLite+HA / PG), rolling upgrades, a troubleshooting table and post-change regression; §13 is a **fixed-IP worked example** (storage server 192.168.1.10 with data in /data on the SQLite backend, one read-write client at /mnt/da and one read-only client at /mnt/db) explaining every flag hands-on; §14 answers “custom data directories, bulk import of existing data (zero-copy `grovefs import` first, streamed rsync through a mount as the alternative; hardlink aliasing, view splitting, and an explicit not-implemented list), and per-client display layouts”. The shortest production path in six steps:

```bash
# 1) build & install the same binary on every node
cargo build --release && sudo install -m755 target/release/grovefs /usr/local/bin/grovefs
# 2) center: shared token + the two services (use systemd in production; units in USAGE §11.4)
TOK=$(openssl rand -hex 16)
grovefs data-server --listen 0.0.0.0:50100 --data-dir /data/cluster --token $TOK &
grovefs meta-server --meta-dir /data/cluster --listen 0.0.0.0:50110 --token $TOK &
# 3) open 50100/50110 to the internal subnet only
# 4) create the view explicitly (a mount never provisions), then mount it
grovefs view --meta-dir /data/cluster create --view alice
grovefs mount --view alice --data-servers 10.0.0.5:50100 --meta-server 10.0.0.5:50110 --token $TOK -m /mnt/alice
# 5) accept: A-write/B-read instantly visible across two clients + cross-host flock + findmnt shows grovefs:alice
# 6) operate: watch journalctl -u grove-meta-server for degrade/freeze keywords; a cold copy of blocks/ + meta/*.db is a complete backup
```

## Testing

```bash
export LC_ALL=C LANG=C            # no PKG_CONFIG needed: no crate queries pkg-config (see Build)
cargo test --release
bash .build-bin/run_all.sh                 # 34 stages (33 gated + bench: build/unit + 31 suites)
SKIP_BENCH=1 bash .build-bin/run_all.sh    # skip throughput bench
```

## Honest limitations

These are explicit, honest boundaries, not hidden assumptions:

- **Transport**: custom TCP frames today, not gRPC. gRPC (tonic/prost) was gate-evaluated as **GO** but full integration is **deliberately deferred**; the current protocol is correct and tested.
- **HA RPO**: ~500ms async by default (0 with `--sync-repl`).
- **Fencing**: `--witness` is implemented and **fail-safe**: a `data-server` list (comma-separated, odd `N`) arbitrates by **majority** (`quorum_size = N/2+1`); a primary holds a non-overlapping lease at a majority (renewed every tick), so a majority live lease is itself proof of exclusive authority and a meta write is admitted from a local lease check (one fan-out per renewal, not per write) AND gates content frames by token at the witness data-servers, bounding the double-write window to the lease safety margin (converging to one primary). The irreducible boundary: a strict two-node pair **without arbiters** cannot prevent split-brain and leaves the window unbounded (CAP/consensus). The **availability gain of a quorum**: with `--witness`, losing a *minority* of arbiters neither freezes writes nor resurrects a stale primary (a quorum still holds the monotonic token); only losing a *majority* fails safe and freezes (`N=1` reverts to a lone witness, which becomes a single availability dependency).
- **Striping**: no throughput gain on a single saturated link (loopback/one NIC); the value is aggregate capacity across nodes — real multi-machine links must be re-benchmarked before claiming gains.
- **Presentation overlay (`--layout`)**: it is a **display customization, not a security boundary** — mounting the same view without the lens still shows the full tree; use separate views + token/API key for hard isolation. Creating/renaming into a purely synthesized dir gives `EPERM`, a name colliding with a lens-shown entry gives `EEXIST`, and `only` mode refuses writes outside the whitelist; symlink target text is not rewritten; with a shared `meta-server`, invalidations for entries under synthesized dirs may miss lens coordinates (use `--no-cache`); rules relocate entries within one view only — cross-view references are not supported.
- **Server-side derived views (`grovefs view`)**: mounts of a derived name are **read-only by hard contract** — the shape is centrally governed, distributed but never modified; every write belongs to the base view. Enforcement is three-layered (kernel RO negotiated at `M_OPEN` → `EROFS` at the client; an authoritative per-op gate on the meta-server answering `E_NOTPERM`; the same check at local `--home` mount time). If you need a *writable* custom shape, use the client-side `mount --layout` lens (§ Per-client presentation overlay), which writes through to the real entries. Otherwise still **not a security boundary** — mounting the base name directly still shows the full tree (token/API key gate access, not shapes); definition **edits hot-apply** (the server checks the file's mtime per op; live mounts switch shape within their ≤2s cache TTL, no remount), while `view rm` does not hot-apply (established mounts keep their shape until remount); during a shape swap, paths/fds on old virtual inodes may resolve to ENOENT — real entries keep stable inodes; no nesting (the base must be a physical view), no `name == base`, no shadowing a physical db; `meta/views/` (SQLite backend) is control-plane config that an HA standby must sync by hand (under PG the control plane sits in `grovefs_control` and replicates with the trees); hot reload is `meta-server`-only — a local (`--home`) mount still resolves its definition once at mount time.
- **Server-side pure aliases (`grovefs view alias`)**: the mirror image of derived views — an alias mount shows the full tree and is WRITABLE, while a governed real name is NOT mountable by its own name (refused locally and by the `meta-server`). Resolution is single-hop, re-scanned per `M_OPEN` (mounts are rare); alias chains, derived targets and one-real-name-with-two-public-aliases are rejected at the command; `import` refuses alias targets; removing an alias affects only NEW mounts (established connections keep flowing, like `view rm`); `--retire` rotates a leaked public name in one gapless step (write-before-remove so the real name is never momentarily mountable; the retired name resolves to a fresh empty namespace that never reaches the real tree) — but rotation, like `rm`, only affects NEW mounts and cannot revoke an existing token-holder, so it is leak hygiene, not a security boundary. One honest wording note: refusals travel **in the protocol** — the `M_OPEN` error frame carries the server's reason text, so a remote alias refusal prints exactly the local wording above (ditto token mismatch, missing/invalid/revoked/wrong-scope api key; against an OLD server the payload is empty and the client falls back to its generic text). And the honest boundary: name hiding is governance, not security — the public name must be distributed anyway; token + api-key admission remain the hard gates.
- **Zero-copy import (`grovefs import`)**: hard links require source and `blocks/` to share a filesystem (cross-device falls back to an independent copy, reported honestly); if an external tool later updates the source file via **rename/replace-write** (the default of vim, rsync without `--inplace`, …) the imported inode keeps the bytes from the last passthrough write — re-run `import` for new content (existing names are skipped; use a different `--prefix` to force a re-registration); symlinks/special files are deliberately skipped (not representable); imported objects are single files and do not participate in the ≥16MiB cross-server striping (striping remains a property of fresh writes through a mount); in a `meta-server` deployment import writes the authoritative DB file directly, so run it on the storage host and expect live mounts to converge by TTL (≤2s) rather than push; the PostgreSQL backend is not supported (its authoritative tree lives in Postgres — stream data in through a mounted path); keep `--source` on the store’s filesystem and **around** — deleting the source only drops that alias name (data survives in `blocks/`), but after external renames the `blocks/` link is the only remaining path to the bytes.
- **Scope of the metadata read pool and directory paging**: both live in **two storage engines** — SQLite `MetaEngine` and PostgreSQL `PgEngine` (each a 4-connection read pool + SQL `LIMIT/OFFSET` paging). Local mounts (`--home` SQLite or PG) get paging and READDIRPLUS directly; `meta-server` handlers use their backend's read pool, and the `M_READDIRP` frame carries `offset/limit` so a remote client fetches one page per kernel buffer and negotiates `FUSE_READDIRPLUS` — the same 5001-entry directory over `meta-server`+`data-server` measures **2.29s** (4.22s paging-only, listings byte-identical). What still falls through to one full listing per buffer with no READDIRPLUS: **derived/lens views** (`LayoutMeta` — its hidden/mapped shape can't be one SQL page) and mounts against a **server too old to advertise the paging cap** (a short `M_OPEN` ack with no second byte). Metadata *write* concurrency (multi-writer) is not supported **on the SQLite backend** (single writer connection); the PostgreSQL backend's opt-in SSI write pool (see “Common questions”) is the exception. The gate is `MetaProvider::supports_paged_readdir()`, pinned by the `store_level_paging_is_advertised_only_by_providers_that_page_in_sql` test.
- **Verification environment**: everything validated on 127.0.0.1 (simulated LAN); no real multi-machine or fio-level benchmark yet.

Where numbers are not measured, no benefit is claimed; wherever there is a limitation, it is stated in code comments and here.

## License

Apache License 2.0 (see the `LICENSE` file at the repository root).

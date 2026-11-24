# GroveFS

> **A user-view FUSE filesystem** — bytes live centrally on disk while each user mounts an **independent, private directory tree** (with full nesting); no NFS.

[中文](README.md)

---

## What it is

`GroveFS` is a **user-view FUSE filesystem**. It cleanly separates *who sees what* (the view) from *where bytes actually live* (the object):

- **View**: each user gets a private directory tree, stored as one SQLite DB `<home>/meta/<view>/<view>.db`. Users cannot see each other's files.
- **Object**: the real bytes of a file, stored as UUID-named object files under `blocks/`, auto-fanned-out over **256 bucket dirs** keyed by a hash of the object id (no single directory grows with the total, so backup/traversal tools stay usable at tens of millions of objects); the view's directory entries only hold object keys (pointers).
- **Store root `--home`**: the physical root holding `blocks/` (data objects) and `meta/<view>/<view>.db` (one tree per view). It is the store root of the **node-side verbs** (`view create`, `gc`, `import`, `apikey`, `du`) and of the servers themselves, resolved `--home` > `$GROVEFS_HOME` > `~/.grovefs`. **A `mount` never reads or writes it**: every mount resolves BOTH a metadata service and a data service and keeps nothing authoritative on the client's disk — not even a cache of them. The central `meta-server` names its authoritative metadata root with `--meta-dir`.

On top of that, metadata and data can each be served as a network service (`meta-server` / `data-server`) so **many client machines mount and share one authoritative view over the LAN** — a central-storage + in-rack LAN-access topology, and **no NFS**.

## Key features

- **Per-user view isolation** with full custom nesting (`mkdir`/`mknod`/`rename`/`rmdir`).
- **Pluggable storage/metadata backends** via traits: local, remote, and cross-server striped (`ShardedBackend`). The authoritative metadata store can additionally be swapped to **PostgreSQL** (`meta-server --meta-store postgres://…`): one PG schema per view, transactions around rename/unlink/lock; under PG the in-tree HA (snapshot shipping) is intentionally off — use PG's own streaming replication / PITR instead.
- **View-bound / read-only API keys (the recommended access control)**: `grovefs apikey create --for mount --view V [--ro]` mints a credential that carries *which tree it may open* and *read-write or read-only*. A key bound with `--view V` can only `M_OPEN` view V (opening any other view is refused at the handshake with `E_AUTH`); an `--ro` key is refused **server-side on every write-intent op** (`E_NOTPERM` on the meta plane, `EROFS` on content writes at the data server), and the `M_OPEN` ack tells the client to mount read-only, so “read-only” never depends on the client remembering `-o ro`. A key is a real, revocable door (unlike a self-reported uid, which is advisory and forgeable). A system-level key (no `--view`) still opens any view.
- **Deletion safety for objects shared across views (the `refs` ledger)**: one `data_ref` names exactly ONE underlying object, yet several physical views can hold names for it at once (`grovefs import` of the same source file into two views resolves to the same key; derived views/aliases share their base view's bytes). A view's own reference count cannot see other views, and trusting it would delete an object another view still references. So the bookkeeping lives in a shared ledger `<base>/meta/_control/refs.db` (PG: table `grovefs_refs.holders`), which every view ATTACHes so the holder count and the node change commit **atomically in one IMMEDIATE transaction**. A blob is physically removed only when “the last name for that key *in this view* is unlinked **and** no other view still holds it”. Out-of-band view-db removals (`rm meta/x/x.db`, a fresh `--home`) can only make the ledger *over*-count — bytes are kept, never lost; `grovefs refs --meta-dir D rebuild` recomputes holders from the trees that actually exist. **Only BASE views (physical trees created explicitly via `grovefs view create`) ever free bytes on delete**; derived/alias mounts are read-only and their removal never GCs. Removing a base view is an explicit act: `grovefs view rm --name V --base` deletes its `meta/V/V.db` (+WAL/SHM), rebuilds the ledger, and frees every blob no other view holds (bytes resolve under `--data-dir`, default `--home`; a governed name, a live `grovefs:<V>` mount, or a `--meta-store postgres://…` deployment is refused — USAGE §3.4.3). A FORK holds real names for shared blobs, so a delete inside a fork frees an object only when it is the last name across every view (base included); removing the fork tree triggers `refs rebuild` to drop its holds. A key with no ledger row is simply private to one view, so an empty ledger needs no initialization step. Orphan pages that no surviving tree names and the ledger holds no row for (an out-of-band `rm meta/x/x.db`, or a crash between object creation and the node insert) are audited and reclaimed offline with `grovefs gc --meta-dir D [--data-dir R] [--apply]`: the scan is the default; `--apply` only deletes pages older than `--older-than` (default 24h), never a key the ledger still holds, and refuses while the store is mounted or a local grovefs process uses it (a PostgreSQL deployment is refused outright).
- **Per-client presentation overlay (`mount --layout rules.txt`)**: one authoritative tree, one copy of bytes, a different *displayed* shape per mount — `hide` trims a subtree, `only` whitelists, `map` relocates an entry to another path (its source spot vacates), `link` adds an alias visible in both places at once (same inode). The lens is pure `MetaProvider`-layer path translation: real entries keep their real inodes, so data IO, locks, GC and invalidation push go straight through; nothing is copied and other mounts are unaffected.
- **Server-side derived views (`grovefs view define` + `mount --view <name>`)**: when shapes should be managed **centrally**, `grovefs view define --name docsonly --base team --rules F` stores the rules in the meta root (`<meta-dir>/meta/views/<name>.layout`, first line `base <physical view>`) and clients simply `mount --view docsonly` — **no local rules file needed**. Derived (virtual) views mount **READ-ONLY, always**: the shape is centrally governed — distribute it, never modify it — so every write belongs to the original (base) view; a mount of a derived name negotiates the read-only flag at `M_OPEN` and lands on a kernel read-only mount (writes give `EROFS`), with an independent authoritative per-op gate on the `meta-server`. One definition renders the same shape for every machine (virtual inodes agree across hosts), **edited definitions hot-apply** (the server checks the definition's mtime per op; live mounts reshape within their ≤2s cache TTL, no remount). A mount that passes no `--layout` learns the shape from the server: `meta-server` resolves `<meta-dir>/meta/views/<view>.layout` and reports it in the `M_OPEN` ack, so a client never holds a rules file.
- **Server-side pure aliases (`grovefs view alias`)**: governance of *what a view is called* rather than *what it looks like* — a public name maps 1:1 onto a physical view (the definition is a single `alias <real view>` line, no rules), so an alias mount shows the **FULL tree and stays WRITABLE**, while the **real name stops being a mount entry point** (the `meta-server` refuses a mount of a governed real name). Public names can rotate freely without exposing the storage identity — guessing the real name no longer opens a door, and a leaked public name is retired in **one gapless command** (`alias --retire`: write-before-remove so the real name stays unmountable throughout). Aliases share `meta/views/` and `list`/`rm` with derived views; alias chains and derived targets are rejected; `grovefs import` refuses an alias target (an alias is not a storage identity).
- **Server-side fork views (`grovefs view fork`)**: the middle ground between a base view and a read-only lens — **organizable, not uploadable**. `grovefs view fork --name lab --base team` COPIES the base view's directory entries into an independent metadata tree (`meta/lab/lab.db`, inodes and hard links preserved); no content byte is copied and the objects stay shared through the cross-view ledger, so the definition is a single `fork <base view>` line. A fork mount has **structure WRITABLE** (`mkdir`/`rename`/`unlink`/`symlink`/`link`/`chmod` all pass) and **content writes refused with `EROFS`** (`write`/`truncate` — uploading bytes belongs to the base view); a delete inside the fork removes only the fork's own names and leaves the base tree untouched, and a shared object is reclaimed only once the LAST view holding a name for it is gone. The content-read-only fact travels to the client in the third flag byte of the `M_OPEN` ack, which is what lets the mount refuse content writes in FUSE. A fork shares the definition slot with derived views and aliases (`view list`/`view rm`), and `view rm` on a fork drops its own tree and rebuilds the ledger. The base must be a **physical view** (a name that carries a definition cannot be a base) with the same semantics on both backends: SQLite copies the rows of one `.db`, PG copies across schemas with `INSERT … SELECT` inside one transaction (`forksrv` proves both arms). `alias` refuses a fork target (a fork is a tree identity, not a public name — mint its credential with `apikey create --view lab`). Forks support **curation**: `--empty` builds the root and copies nothing, then `view graft` brings in the entries selected from another view — one plan line per entry (`<src>` lands under `--dest` as-is, `<src>TAB<dst>` also renames it), `--map -` reads the plan from stdin; the whole plan commits as ONE transaction (all-or-nothing) with zero byte copies, a colliding name is refused by default and skipped with `--skip-existing`.- **Zero-copy import of existing data (`grovefs import`)**: when the server already holds a pile of directory data (say `/data/legacy`), `grovefs import --view V --source DIR` registers the whole tree into a view without moving bytes — on the same filesystem each object is a **hard link** of the source file (zero extra bytes; both paths read/write one inode, live in both directions), degrading to a streamed copy across devices. Structure/timestamps/mode/owner are preserved, re-running is incremental, and deleting a name in the view only unlinks the `blocks/` path — **the user's source data can never be destroyed by the view**. Composes with derived views/lenses for virtual-directory shapes (`map` the imported paths anywhere; the resulting derived mount is **read-only** — write through the imported base view). `grovefs import` also refuses a derived target name, which would produce a shadow `<view>.db` nobody can read.
- **Custom binary framing over TCP** (not NFS, not gRPC) with a connection pool.
- **Real-time cross-client invalidation push** (default): after a write, the center broadcasts `inval_entry`+`inval_inode` so peers see changes immediately, no polling; `--no-cache` gives a zero-caching fallback.
- **Distributed advisory locks**: `flock` / `fcntl F_SETLK/F_GETLK` arbitrated centrally, effective across processes and machines; crash reclaim via FUSE `release`, plus a short lease (default 15s) and renewal heartbeats.
- **High availability**: primary/standby `meta-server` with heartbeat/epoch promotion and automatic demotion; `--sync-repl` drops write RPO to 0; `--witness` takes a comma-separated list of shared `data-server` arbiters (odd `N`) and arbitrates by **majority** (`quorum_size = N/2+1`), **fail-safe**, holding a non-overlapping lease at a majority (renewed every tick; each arbiter refuses overlapping leases, so a majority live lease is itself proof of exclusive authority and a write is admitted from a local lease check) and stamping content frames so the arbiters can fence a stale writer, bounding the double-write window to the lease safety margin; losing a minority of arbiters still allows writes (`N=1` = a lone witness).
- **Token authentication** (shared cluster passphrase; both planes), **revocable per-caller API-key gates** (`meta-server`/`data-server --require-api-key` + `mount --api-key`; a key can be bound to ONE view with `--view` and forced read-only with `--ro`, both enforced server-side), **data-plane admission binding via a Data-Access Capability** (`meta-server`/`data-server --data-access-key`, a shared HMAC key: a genuine meta mints a capability at `M_OPEN` and the `data-server` verifies it at `OP_OPEN`, so a rogue meta-server cannot arm a client against the honest data-server; NOT a per-view ACL, opt-in and off by default; USAGE §6.1.1), **read-only mounts** (`--ro`), **multi-threaded `fuser 0.18`** (`clone_fd`), **chunked parallel IO**, and **cross-server striping**.
- **Per-mount content bandwidth shaping (`--bw-read` / `--bw-write`)**: a client-side GCRA slot schedule paces the bytes actually delivered — reads before the reply, writes after persistence — so the long-run rate is exactly the cap (bytes are paced from the first op; a single op larger than one second of budget may start up to 1 s early) and nothing is dropped or truncated, and write-side delay is natural backpressure. `0`/omitted = unlimited (one `rate == 0` branch on the hot path); `--stats`' `bytes read`/`bytes written` counters verify achieved throughput. Honest boundary: self-discipline, not isolation — a raw data-plane client that bypasses FUSE, or N mounts of one view, each get their own budget, and orphaned objects are keyed by UUID and shared across views, so they cannot be attributed to a view server-side.
- **Agent-facing MCP interface (`grovefs mcp`)**: the view governance above (`define`/`alias`/`list`/`rm` of derived views and pure aliases) is also exposed as **MCP (Model Context Protocol) tools** over stdio, so an AI agent can customize a data listing's shape programmatically — rules travel as **inline text** through `tools/call` and the agent never touches a file. Two read-only introspection tools close the discovery loop: `list_view_tree` renders the real listing a view shows (a derived view through its own lens, or an unpublished `base`+`rules` previewed without committing) and `get_view` reads back a single definition, so an agent writes rules against actual paths instead of guessing; both open the LOCAL SQLite metadata read-only, mount nothing, and touch no object bytes; a `validate_definition` dry run additionally checks a whole candidate (name+base+rules) against every governance guard — writing nothing — and returns the exact refusal a real publish would give. Shares the exact validation and error wording with the CLI (one contract, two entrances) and is control-plane only: it mounts nothing and writes no data. Authentication is opt-in — `grovefs mcp --require-api-key` gates the state-touching methods behind a `grovefs apikey` credential (hashed at rest, scoped `mcp`/`any`) and logs every call; the same `--require-api-key` also fronts `meta-server`/`data-server` for mounts (scope `mount`/`any`), with revocation closing live mounts. An `admin`-scoped ACCOUNT key (`--owner`) opens the remote governance port (`meta-server --control-listen HOST:PORT`) instead, so any machine can run `view`/`apikey`/`refs` governance against that store with a revocable credential instead of a database URI — SQLite included (USAGE §3.5.2). See the “Customizing data listings for agents: the MCP interface” section and USAGE §3.5.
- **The management console (`grovefs web`)**: an **independent subcommand** — a small HTTP gateway in front of the remote governance port (`meta-server --control-listen`), so HTTP never enters `meta-server` itself. It serves an embedded single-page app (overview / views / keys / accounts & grants / audit, `include_str!`-ed at build time — the page reaches for no CDN and ships no node_modules, so it opens on an air-gapped host; the sources live in `console/` (Vue 3 + TypeScript + Vite) and the built bundle is committed under `src/webui/`, which is why a plain `cargo build` still needs no node toolchain and only front-end work runs `npm ci && npm run build` — see `console/README.md`), turns a browser session into an **account**, and forwards each click as one governance op. Those forwards ride a **resident control connection**: one socket pinned to the admin credential, so an op skips the TCP connect, the server thread and the two queries of an authentication (measured with both arms interleaved on one host: serial p50 −19…−43% and server CPU/op −10…−50%, throughput +18%/+25% at P=16/32 with P=32 p50 83ms→26ms, while the P≤8 deltas sit inside the run-to-run spread). **What is pinned is the credential, not the identity** — every op is still attributed and judged by the account in its own request, and not one audit row is saved (`GROVEFS_GATEWAY_CTL_PIN=0` puts the gateway back on one connection per request). Accounts are created and **delegated a base view**: after `web grant add --account bob --base main`, bob governs that tree outright — its `fork`s, aliases and lenses, and the keys bound to any of those names, all inherited automatically (a `lens`/`alias`/`fork` name resolves outward to its base in ≤ 4 hops server-side), so a grant names a place rather than a leaf. The rule “who may touch which tree” still lives in exactly ONE place, server-side: the gateway only translates and **subtracts** (a hardcoded 32-op route list; `gc.scan`/`gc.apply`/`refs.rebuild`/`view.rm_base`/`fork.graft` are never routed), and accounts, grants and password digests (PBKDF2-HMAC-SHA256, 210k rounds) live on the **server** (SQLite: `<meta-dir>/meta/_control/web_accounts.db`; PG: tables in the control schema) — sign-ins are verified by the process that owns the store, which is why the gateway can afford to hold no password database at all. The FIRST administrator can only be placed by `meta-server --admin-user/--admin-password-file` while the account table is empty (a governance entrance that could mint its own admin would defeat the gate). See the “Management console” section and USAGE §3.5.3.
- **Parallel metadata reads + paged directory listings + `FUSE_READDIRPLUS`**: alongside its single writer connection, each view's `MetaEngine` keeps a pool of warm read connections (**1 warm by default, up to 8 retained for bursts**: `READ_POOL_SIZE=1` + `READ_POOL_RETAIN=8`; `GROVEFS_SQLITE_READ_POOL` raises the WARM FLOOR, the retention ceiling is the larger of the two, `0` closes the pool) that serves `lookup`/`getattr`/`readdir` and the nlink resolution, so metadata reads never queue behind the write lock (**SQLite writes still serialize on that one authoritative connection** — single-writer semantics untouched; PG spreads writes over its own bounded SERIALIZABLE write pool, see the PG honest boundary below). A mount process holds no engine, so this pool belongs to the server's per-view engines (and to any caller embedding the library in its own process); concurrent `meta-server` RPC handlers each use their own backend's pool and stop waiting on each other. Directory listing goes through `readdir_page(parent, offset, limit)`, which the SQLite engine pushes down as SQL `LIMIT/OFFSET` while the FUSE callbacks fill each reply buffer with geometrically growing chunks (32→1024 for READDIRPLUS, 128→1024 for plain READDIR), so one kernel request costs `O(buffer)` instead of re-scanning the whole directory; and `FUSE_DO_READDIRPLUS` (kernel >= 4.6) is negotiated by default, shipping attributes with the entries so `ls -l` stops degrading into N `LOOKUP`+`GETATTR` round-trips. On the client side the `M_READDIRP` frame pushes `offset/limit` down to the server, so a remote client fetches exactly one page per kernel buffer. Measured (a `meta-server` + `data-server` pair on this host's loopback, same mountpoint, each pass behind a fresh mount, empty kernel dentry cache): a 5001-entry directory `ls -l` **0.270–0.274s** (`--no-readdirplus`, paging only: **0.575–0.629s**, the two listings byte-identical once sorted); a 10,002-entry directory `ls -f` **0.116s** and `ls -l` **0.547s**; a 100,002-entry directory `ls -f` **0.925s**. Paging and PLUS apply to every provider class — SQLite/PG/treedb engines each push the page down inside their own store, and derived/lens views (`LayoutMeta`) override `readdir_page` (materializing the rendered shape per parent directory, then slicing it; materialization IS a cache, **so it must be flushed by invalidation pushes**), which is why they benefit too: the same 100,002-entry directory through a 100-rule lens measures `ls -f` **1.612s**, and **0.429s** re-read inside the TTL. `--no-readdirplus` stays as the A/B and compatibility knob; rerun `.build-bin/probe_readdir_cluster.sh`.
- **Mount bootstrap (`grovefs manager`, the ONLY entry point)**: replaces "type the two cluster coordinates on every client" with "declare them once, server-side" — `manager define --meta ... --data ...` records a deployment's AUTHORITATIVE topology, `meta-server`/`data-server --register-with` announces that a node is up, and a client runs `mount --manager H:P --roster-key-file F` to get a SIGNED roster, resolve those two values, and only then talk to the meta- and data-servers. **`mount` no longer accepts a hand-typed `--meta-server`/`--data-servers` (removed)**: failover and stripe semantics are unchanged, but those two values now come ONLY from the roster — the typed-coordinate path is closed. This is a REPLACEMENT, not an addition. It is a bootstrap service and never a data path: a manager restart or outage disturbs no live mount, it can only refuse a NEW one (fail-closed). Anti-forgery comes in two layers — a registration needs the JOIN key's HMAC and is accepted ONLY for an address the operator DECLARED (an undeclared address is refused even with a valid MAC, so a leaked join key cannot steer clients to an attacker's machine), and the answer is signed with the ROSTER key, which a client verifies or the mount fails rather than warns. A roster is always the DECLARED set, never a healthy subset (stripe members are not a candidate pool). See USAGE §4.2 and `docs/adr/0001-manager.md`.
- **Full regression**: `run_all.sh` — **43 stages (42 gated + bench), all GREEN** (`layout` presentation-overlay e2e incl. server-side derived views and read-only derived mounts, remote & local, and pure aliases — writable alias mounts, real-name mount refusal (remote & local), chain/derived-target guards, rm reopens the real name, and gapless name rotation via `--retire` (a leaked public name is swapped in one step — old name stops resolving, the real name stays unmountable throughout); 82 assertions + `bwlimit` per-mount content bandwidth-shaping e2e (17 assertions: a 24MiB read/write at 8MiB/s is held to a >=1.4s floor while an unlimited mount of the SAME view is materially faster, throttled bytes are intact, `GROVEFS_BW_READ` env fallback, `--bw-read 0` = unlimited, invalid rates refused at parse time, `--bw-write` stays inert on a read-only mount) + `import` zero-copy import e2e (35 assertions: hardlink passthrough / mtime fidelity / GC never touches the source / incremental re-run / composition with a derived shape, which mounts read-only) + `migrate` metadata-export/import e2e (one view's tree travels as host-neutral JSONL to a different store path — no absolute path/hostname/blocks path in the file; ids/inodes/hard links/mode/owner/symlink targets survive, the ledger is rebuilt, import is atomic so a failed `--replace` keeps the old tree, `--as-fork` restores fork semantics with a shared ledger, and several base views stay independent; 74 assertions) + `mcp` agent-governance e2e + `apikey` MCP-interface API-key auth & call-audit e2e (61 assertions: create/list/revoke/rotate in place (same id, fresh secret, old one refused, the stored copy replaced with the new one)/set-ro in-place ro↔rw flip (the flipped key still authenticates), hashed-at-rest with nothing on disk but an `sbx1` envelope and `apikey secret` reading the credential back byte-for-byte, no-key `-32001` refusal, good-key admit + attributed call log, `GROVEFS_MCP_API_KEY` env pre-auth, default no-gate server, `--note`/`--ttl` stored, listed and searchable, the key column carrying only a head+tail fingerprint, `update` editing the remark and the deadline, a past-deadline key refused at the gate and revived by a new `--ttl`) + `mountkey` revocable mount-credential e2e (76 assertions: scope minting + typo refusal, refusal reasons carried in the protocol, admission & env fallback with the secret absent from argv, connection audit down to key and view, scope isolation both ways, a revoke closing a LIVE mount with no stale endpoint, other keys unaffected, token+key double gate, an independent data-plane gate, failover carrying the credential, ungated default unchanged, a view-bound key refused across views with its refusal audited, an `--ro` key's writes refused server-side, `set-ro` flipping rw↔ro in place on a LIVE mount with no reconnect) + `config` command-shorthand e2e (auto-loaded `KEY=VALUE` file + env fallbacks: the flag > env > file > default chain for GROVEFS_HOME/META/DATA/TOKEN/DB_PATH, parser edge cases — comments, `export` prefix, quoted values, non-`GROVEFS_*` keys and malformed lines warned-and-ignored, an unreadable explicit `$GROVEFS_CONFIG` non-fatal — and a real token-gated FUSE mount whose coordinates come entirely from the file, plus the hard refusal when no source supplies a coordinate (exit 1, the refusal text verbatim, no mountpoint left behind, a mount that never touches `--home`); 37 assertions) + `gc` offline orphan-object audit/reclaim e2e (42 assertions: a clean store scans clean; an out-of-band `rm meta/<view>/<view>.db` leaves its objects reported as ledger-held and NOT deleted; after `refs rebuild` they surface as orphans, the default age gate spares a fresh object and `--older-than 0 --apply` unlinks it; loose files outside the bucket layout are reported but never deleted; `--apply` refuses while THIS store has live mounts yet a same-named view mounted from another store does not block; a `--data-dir` sharing no object with any tree is refused, with `--force` clearing only that guard; a PG store and an invalid duration are refused; `--json` field shapes) + `ctlplane` remote-governance e2e (95 assertions: the CONTROL banner; the first admin key minted on the server host; remote `view`/`apikey`/`refs` governance with nothing written on the operator machine and a second empty home proving the tree lives server-side; `any`/`mount` and garbage keys refused on the port; a remote revoke taking effect immediately; no-key and mixed-local-flag refusals; audit retention (`apikey purge` counting without deleting and deleting with its starting count, the two refusals of `--audit-retention` plus `never` admitted as the explicit keep-everything, and the `auditRetention` `store.info` reports (7d when the flag is silent)); `fork`/`graft`/`create --manifest`/`export`/`import` all SERVED over the wire, the last two as a dump that streams back in line-aligned chunks and an import the server installs itself; teardown guards still enforced remotely; the PG arm runs the same script) + `web` management-console e2e (186 assertions: the install-time owner seeded ONLY while the account table is empty, an unchanged restart re-reading the flag neither re-seeds nor locks itself out, a different `--admin-user` refusing to start at all; the gateway refusing to START — before binding a port — without a control address, without a credential, with a bad credential, or on a routable plain-HTTP bind without `--allow-insecure-http`; SPA + security headers + no anonymous reach; two wrong sign-ins answering byte-identically; CSRF, Origin and the op whitelist with the five verbs that stay CLI-only; the delegated account exercising its ACL **through the page path** — governing its base and its family (a fork appearing exactly once in `view.stats`), minting keys only for its own views, rotating its own keys' secrets through the page route while another account's key is refused, refused deployment-wide verbs; a withdrawn grant biting on the NEXT call; audit naming `account@ip`; export/import through the console's two routes; disable, the last-admin guard; the per-client-IP sign-in throttle answering 429 with a `retryAfter` even for the right password; the two connection shapes of pinning — the default arm keeping a live governance connection, the `GROVEFS_GATEWAY_CTL_PIN=0` arm keeping none, both answering the same op byte-for-byte identically) + `forksrv` two-backend remote fork/graft e2e (whole-tree copy with zero bytes moved, independent tree with structure writable and content EROFS, `--empty`+`graft` curation, the ledger ending at exactly zero) + `xfer` two-backend remote export/import e2e (the dump lands on the operator's disk, chunks cut on line boundaries, a 60 B and a 4 MiB window produce the same rows, `--as-fork`/`--replace`/guards, a line-for-line round trip and no ledger leak) + `gcremote` remote-gc e2e (the server reads its own trees and lists bytes from a data-server; `--apply` refuses while the SERVER has a live session and says a mount on another machine is invisible) + `pg` PostgreSQL backend e2e; the PG suite self-skips on hosts without a reachable PostgreSQL, the new `client` suite mounts the prebuilt `grovefs-mac` of the sibling `grovefs-client` repo against the meta-server/data-server built here — write/read round-trips through BOTH control-plane tiers, the unknown-view refusal and the token gate, and roster bootstrap through `manager serve` (a mount typed with no coordinate at all resolving both addresses from a signed roster; the five refusals — a typed coordinate alongside `--manager`, no `--roster-key-file`, a roster that does not verify under it, a key file whose shape is wrong before the manager is even asked, an unreachable manager named by address — and a roster declaring several data members, refused by the client yet mounted by the kernel `grovefs mount` of the very same roster) — and self-skips when that checkout, its binary or the `fusermount` helper is absent, so GREEN is preserved either way).

## Architecture

Three layers, all trait-decoupled:

```
┌─ FUSE mount process  (fs_impl.rs: GroveFS, &self, thread-safe)
│    ├── MetaProvider trait   view-tree access
│    │     ├── local:  MetaEngine  → <home>/meta/<view>/<view>.db
│    │     ├── remote: RemoteMeta  → meta-server (authoritative)
│    │     └── overlay: LayoutMeta → mount --layout (per-client presentation, decorates either)
│    │           ├── derived view: server-side LayoutMeta singleton → mount --view <name> (rules defined once via grovefs view define, stored in meta/views/)
│    │           ├── fork view: `fork <base>` → independent meta/<name>/<name>.db (structure writable, content EROFS, bytes shared)
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
| `grovefs view fork --name X --base B [--empty]` | independent metadata tree `meta/X/X.db` (`--empty`: root row only) | its own organizable tree | structure writable, content `EROFS`, bytes shared |
| `grovefs view graft --name X --map P [--src S] [--dest D]` | a plan of `<src>[TAB<dst>]` lines | entries placed in the fork | one transaction, zero bytes copied |
| `grovefs view alias --name P --real R` | a single `alias <R>` line | public name 1:1 governance | full tree writable, real name refuses mounts |

The customized shape **does not have to resemble the base tree**: `<src>` is pinned to the base, `<dst>` is yours to invent; add `only <new-root>` to make the custom tree the whole view. The one thing a lens cannot do is invent content — every rendered entry is some base entry re-pathed, mirrored or pruned, plus synthesized scaffolding; bytes are never copied. Hot-reload: mounts through `meta-server` reshape within ≤2s (the server re-checks the definition's mtime per op); a bare local `--home` mount resolves once at mount time and needs a remount.

> Details: README "Per-client presentation / Server-side derived views / Pure aliases" (fork: USAGE §3.4.1), USAGE §3.1–§3.4.1; the agent workflow in README "Customizing data listings for agents" and `skills/grovefs-mcp/`.

### 2. Where view metadata lives: one engine per view, control plane follows the backend

```text
<meta-dir>/meta/
├── <view>/                one authoritative tree per physical view, in its OWN directory
│   │                      (`ls meta/` is a listing of trees)
│   └── <view>.db          the tree (SQLite WAL): node table
│                        (id/ino, parent_id, name, is_dir, data_ref, link_target,
│                        mode/uid/gid, mtime/atime/ctime, UNIQUE(parent_id,name)
│                        + ino/parent/data_ref indexes) + ctrl + locks (POSIX lock leases)
├── _control/              the control plane — NOT trees (the name is reserved, so it can never
│   │                      be a view name and is never enumerated as one)
│   ├── refs.db            cross-view holder ledger for shared objects (GC's sole "last name" authority;
│   │                      derived/alias/fork share the base's bytes)
│   ├── api_keys.db        API key SHA-256 digests + a wrapped copy of each credential + call
│   │                      audit (the SQLite control plane; under PG it lives in
│   │                      `grovefs_control.api_keys` instead)
│   └── store.key          opens the wrapped credential copy (0600, created atomically
│                          on first use)
├── views/*.layout         derived/alias/fork definitions (derived = first line `base <B>` + rule
│                        lines; alias = a single `alias <R>` line; fork = a single `fork <B>` line)
└── .xfer/                 the server-side export/import spool (dot-prefixed, so never a tree)
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

**The v2 layout and migrating an old store**: every physical view tree gets its own directory (`meta/<view>/<view>.db`) and the control plane moves to `meta/_control/` — **the directory name is the type** (dot-prefixed and `_`-prefixed directories are never enumerated as views). The old flat store (`team.db`/`refs.db` lying flat under `meta/`) is read by no command: `grovefs layout --meta-dir D show` reports the shape, `grovefs layout --meta-dir D migrate` migrates it offline to v2 (`--dry-run` first for the plan; it requires no live mount on that storage and no grovefs process on this machine).

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
Governance plane (grovefs mcp): the same key registry (SQLite: meta/_control/api_keys.db; PG:
grovefs_control.api_keys), scope must include `mcp`; every tools/call and
resources/read is audited; revocation freezes live mounts within ≤1s (op-driven re-check), while
historical calls keep their attribution.
```

Honest boundary: display governance (derived/alias/fork) is **not a security boundary** — it only reshapes the presented name tree; what is visible is readable and readable is writable back (a fork's content excepted). The real doors are the token and revocable keys. Authentication consults only the stored SHA-256 digest, kept next to a reversible ciphertext copy of the credential that needs the store key to open (`apikey secret --id X` reads it back); the control plane (keys, audit, view definitions) **follows the metadata backend** — SQLite keeps `meta/_control/api_keys.db` + `views/*.layout`, PG keeps everything in the `grovefs_control` schema; one system of record, never split.

> Details: README "Shared views & the permission model" + the API-key subsection + the "Management console" section, USAGE §6 (incl. §6.1 view-binding / read-only key measurements), §3.5.1 (ten-step key drill), §3.5.3 (accounts + base-view delegation).

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
**No.** The "many machines share one tree" capability comes from **`meta-server` + the same `--view`**, independent of whether the backend is SQLite or PG. The default embedded SQLite (`rusqlite bundled`, compiled into the binary, zero external deps) was built for exactly this topology — deploy `data-server` + `meta-server` on a central node and have clients `mount --view alice --manager <center>:50055 --roster-key-file /etc/grovefs/roster.key` (the coordinates come from the signed roster); they share one authoritative tree with real-time cross-client visibility. This whole path is proven on a pure-SQLite backend by the `metasrv` / `freshness` / `locks` suites — **you do not need to install PostgreSQL first**. The two backends are an **either/or** choice, not a layered deployment: one authoritative tree lives on exactly one backend — stay on SQLite and you never install PG at all; switch to PG and the server stops creating/reading any `<meta-dir>/meta/<view>/<view>.db` (the only ever-SQLite exceptions are the control-plane `meta/_control/` files and derived-view `.layout` definitions, which are independent of backend choice).

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

> **Honest boundary**: the write model **differs** between the two engines — the default SQLite backend keeps `meta-server` a **single writer** per view (one authoritative connection + mutex, writes serialized); `--meta-store postgres://…` widens writes to a bounded **SERIALIZABLE write pool** (`PG_WRITE_POOL_SIZE = 8`, the HopsFS route with the transactional substrate handed to PostgreSQL's own SSI) — concurrent writes on disjoint `(parent_id, name)` rows commit in parallel, read-write overlaps abort (`SQLSTATE 40001/40P01`) and are **transparently retried** (200µs–25ms backoff, ≤12 attempts, never leaked to POSIX callers), proven by the `pg_ssi` (5 Rust tests) and `pg_parallel` (bash, 3 mounts × 45 concurrent creates) suites with auto-SKIP on PG-less hosts; governance invariants I1–I4 hold unchanged under any SSI-accepted schedule. Parallelism stays **within one PG cluster** — cross-cluster metadata partitioning is not done (hand that to PG's own replication/sharding). Metadata *read* parallelism spans **both** engines — SQLite `MetaEngine` and PostgreSQL `PgEngine` each keep their own read pool (SQLite: 1 warm read connection per view engine with bursts retained up to 8 — `GROVEFS_SQLITE_READ_POOL` raises the warm floor and `0` closes the pool; PG: `PG_READ_POOL_SIZE` = 4) and page listings with SQL `LIMIT/OFFSET`. For default deployments, keep the zero-dependency SQLite; PG's incremental value is **write parallelism + durability / replication / operational maturity**.

## Build

**Prerequisites** (Linux): Rust (edition 2021) only. **No `pkg-config`, no libfuse development package** — the FUSE client is the pure-Rust `fuser` (its `libfuse*` features are not enabled); a full `PKG_CONFIG=/nonexistent cargo build --release` succeeds and the resulting binary has no fuse library in `DT_NEEDED` (only the five glibc libs). The metadata engine uses `rusqlite` with the `bundled` feature — SQLite is compiled from source, **no system dependency**. Mounting needs the kernel's `/dev/fuse`, plus the distro's setuid helper `fusermount`/`fusermount3` **when unprivileged** (with `CAP_SYS_ADMIN` it goes straight to `mount(2)`) — a userspace tool, not a development package.

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
| **Node-side store root** — the directory holding `blocks/` (data) and `meta/` (views); only a machine running the servers or the node-side verbs (`view create`, `gc`, `import`, `apikey`, `du`) names it. **A mount never reads or writes it** | `--home <dir>` (the servers name their own roots with `--meta-dir` / `--data-dir`) |
| **Server metadata root** — the host directory holding the authoritative view DBs, view definitions and API keys for `meta-server` | `--meta-dir <dir>` |
| **Data landing root** — where `data-server` writes bytes (defaults to the metadata root) | `--data-dir <dir>` |
| **Existing directory to take over** — data already on disk that you want to turn into a view | `--source <dir>` |
| **Mount point** — the local directory you use to reach the tree | `-m <dir>` (`--mountpoint`) |
| **Mount bootstrap endpoint** — the single manager address a client points at; both meta / data coordinates (host:port) are resolved from its signed roster, never typed | `--manager <host:port>` + `--roster-key-file <path>` (`[--deployment NAME]`) |
| **View name** — an arbitrary label for one logical tree (unrelated to Linux usernames) | `--view <name>` |
| **Cluster admission secret** — the shared token a server demands (data face and meta face can differ) | server `--token <str>` + client `mount --token <str>` |
| **Revocable per-caller credential** — the server admits a live API key instead (`grovefs apikey create --for mount`); listable, auditable per caller, and a revoke closes **already-established** mounts | server `--require-api-key` (+ `--meta-dir D` on `data-server`) + client `mount --api-key <ID:SECRET>` (or env `GROVEFS_API_KEY`) |
| **Mount as a given uid** — lock arbitration and audit record this uid as the entry owner (advisory) | `--run-as-uid <uid>` |
| **Read-only / zero-cache strong consistency** | `--ro` / `--no-cache` |
| **Directory-listing fast path escape hatch** — turn off READDIRPLUS (attributes shipped with each entry), falling back to plain READDIR + per-entry `LOOKUP`; A/B benchmarks and compatibility debugging only (mounts whose provider cannot page in its own store never negotiate READDIRPLUS, so this flag is a no-op there) | `--no-readdirplus` |

## Quick start (minimal local cluster)

A view is ALWAYS served by two processes: `meta-server` owns the tree, `data-server` owns the bytes — and a mount learns both their addresses ONLY from a manager-signed roster, never by typing an IP:PORT. The smallest cluster runs the manager plus those two services on this host's loopback over one store root `BASE` — `meta/` holding one tree per view, `blocks/` holding content objects. Move them to two machines and the commands are word-for-word identical, only the addresses differ. There is no local single-disk fallback to retreat to, and coordinates are manager-only — why the removal rather than a demotion is `docs/adr/0001-manager.md` and `docs/adr/0002-no-local-mount.md`:

```bash
Y=./target/release/grovefs
BASE=/data/myfs

# 1) declare the topology and start the manager (first serve mints join.key / roster.key under $BASE/manager/)
$Y --home $BASE manager define --meta 127.0.0.1:50053 --data 127.0.0.1:50100
$Y --home $BASE manager serve --listen 127.0.0.1:50055 &

# 2) the two services register with the manager (one store root = the minimal cluster)
$Y meta-server --meta-dir $BASE --listen 127.0.0.1:50053 --advertise-addr 127.0.0.1:50053 --register-with 127.0.0.1:50055 --join-key-file $BASE/manager/join.key &
$Y data-server --data-dir $BASE --listen 127.0.0.1:50100 --advertise-addr 127.0.0.1:50100 --member-id 0 --register-with 127.0.0.1:50055 --join-key-file $BASE/manager/join.key &

# 3) create the two views explicitly (the only entry point that builds a view)
$Y view --meta-dir $BASE create --view alice
$Y view --meta-dir $BASE create --view bob

# 4) mount: both coordinates come ONLY from the manager's signed roster (a mount
#    detaches by default — it returns once the mountpoint is live; --foreground stays attached)
$Y mount --view alice --mountpoint /mnt/alice --manager 127.0.0.1:50055 --roster-key-file $BASE/manager/roster.key
$Y mount --view bob   --mountpoint /mnt/bob   --manager 127.0.0.1:50055 --roster-key-file $BASE/manager/roster.key

# 5) read and write it like an ordinary directory, custom hierarchy included
mkdir -p /mnt/alice/projects/2026
echo "report" > /mnt/alice/projects/2026/report.txt
ls /mnt/bob                               # bob cannot see alice's files

# 6) unmount
$Y umount --mountpoint /mnt/alice
```

Verbatim transcript of the same sequence (store root and mountpoints on throwaway directories):

```text
created BASE view 'alice' at /tmp/qseCnd/meta/alice/alice.db
  deletes in a base view free the underlying object once no other view references it;
  reshape it with `view define` or publish it under a name with `view alias`.
$ grovefs view --meta-dir /tmp/qseCnd list
no alias/fork/lens definitions in /tmp/qseCnd (local files) — the 2 base view(s) below each map to their own tree
base view 'alice' -> own tree meta/alice/alice.db (read-write; deletes free a shared object only when no view holds it)
base view 'bob' -> own tree meta/bob/bob.db (read-write; deletes free a shared object only when no view holds it)
view keys: `grovefs apikey --meta-dir /tmp/qseCnd list [--view NAME]` (add --view to see only that view's keys)
$ grovefs mount --view alice --mountpoint /tmp/qs_mnt_443443/alice --manager 127.0.0.1:50055 --roster-key-file /tmp/qseCnd/manager/roster.key
manager 127.0.0.1:50055: deployment 'default' epoch 1 resolved meta [127.0.0.1:50053] data [127.0.0.1:50100] (signature verified, valid for 3599s more)
using remote metadata service at 127.0.0.1:50053
using remote data service at 127.0.0.1:50100
mounting view 'alice' at /tmp/qs_mnt_443443/alice (fully remote: no local store)
grovefs FUSE caps granted: ASYNC_READ=true BIG_WRITES=true PARALLEL_DIROPS=true POSIX_LOCKS=true FLOCK=true WRITEBACK=true ATOMIC_O_TRUNC=true AUTO_INVAL_DATA=true READDIRPLUS=true MAX_PAGES=true; max_write req 4 MiB
grovefs: mounted view 'alice' at /tmp/qs_mnt_443443/alice (background pid 443480)
$ ls /tmp/qs_mnt_443443/alice/projects/2026
report.txt
$ cat /tmp/qs_mnt_443443/alice/projects/2026/report.txt
report
$ find /tmp/qseCnd -path '*/blocks/*' -type f   # the bytes live in the data-server's root
/tmp/qseCnd/blocks/2a/abebde35-b88d-4f1f-96ff-ac856af3795b
$ grovefs umount --mountpoint /tmp/qs_mnt_443443/alice
unmounted /tmp/qs_mnt_443443/alice
```

Coordinates are never typed: every mount runs `mount --manager H:P --roster-key-file F` and resolves the pair above from the manager's signed roster (the resolved lists are exactly the meta- and data-server addresses it then dials, so failover and stripe semantics are unchanged). See "Mount bootstrap" and `docs/adr/0001-manager.md`.

Pre-seed a hierarchy with a manifest: `view --meta-dir $BASE create --view alice --manifest seed.txt` (one line per entry, `mkdir <path>` or `touch <path>`, `#` comments; names already present in the manifest are skipped, so applying one seed twice does not plant it twice). A `touch` object lands in the `blocks/` ADJACENT to that meta root — the same root in the minimal cluster; when the `data-server` runs on a different `--data-dir`, create the view first and write the files through a mount. `create` refuses a name that already exists rather than silently reusing it:

```text
error: view 'alice' already exists — remove it first (`grovefs view rm --name alice --base`) or choose a different name
```

## Shrinking long commands: environment variables & the auto-loaded config file

Repeat-heavy flags don't have to be retyped. Every knob resolves through one chain — **explicit flag > live environment variable > config file > built-in default** — and on startup grovefs auto-loads a `KEY=VALUE` file into any `GROVEFS_*` variable not already set, searching `$GROVEFS_CONFIG` (explicit path) first, then `~/.config/grovefs/grovefs.conf`:

```bash
# ~/.config/grovefs/grovefs.conf (chmod 600 if it carries a secret — same contract as
# putting one in an environment variable)
GROVEFS_MANAGER=192.168.1.10:50055
GROVEFS_ROSTER_KEY_FILE=/etc/grovefs/roster.key
GROVEFS_TOKEN=s3cret

# before: grovefs mount --view alice -m /mnt/alice --manager … --roster-key-file … --token …
# after:  only "which view, where" is left on the line
grovefs mount --view alice -m /mnt/alice
```

Knobs resolvable from env/file: `GROVEFS_HOME` (`--home`), `GROVEFS_MANAGER` / `GROVEFS_ROSTER_KEY_FILE` / `GROVEFS_DEPLOYMENT` (the mount bootstrap `mount --manager/--roster-key-file/--deployment`, now the ONLY source of a mount's coordinates), `GROVEFS_TOKEN` (`mount --token`, and the servers' `--token` reads the same knob), `GROVEFS_META_DIR` (`meta-server --meta-dir`), `GROVEFS_API_KEY` / `GROVEFS_MCP_API_KEY` (the credential envs, also settable from the config file), `GROVEFS_STATS`. (`GROVEFS_META_SERVER` / `GROVEFS_DATA_SERVERS` no longer drive `mount` — they are kept only on the ops commands that speak to a service address directly, such as `du` and `gc --control-server`.) The syntax honors `#` comments, an optional `export ` prefix, and quoted values; non-`GROVEFS_*` keys and malformed lines print a `file:line` warning and are ignored. `--view`/`-m` are deliberately NOT file-servable — they differ per mount, and baking them into a default is the wrong shape. The detached-mount re-exec inherits the environment, so the shorthand survives daemonization. Regression: the `config` suite (37 assertions: the precedence chain, parser edge cases, a real token-gated FUSE mount driven entirely from the file, the hard refusal when no source supplies the bootstrap at all — neither `--manager` nor `GROVEFS_MANAGER` — with exit code, the sentence itself and nothing attached — and the two claims that replaced the removed local path: the banner reads "fully remote: no local store" verbatim, and a mount does not create so much as a directory under the --home it is handed).

## About `--view` and per-view path capacity

- **`--view` is a view-namespace label, unrelated to Linux usernames**: any legal filename works (`alice`, `team-a`, `u42`, `research_group`). It is used only to (1) address `<meta-dir>/meta/<view>/<view>.db` on the server and (2) tag the FUSE mount as `grovefs:<view>`. The binary does not call `getpwnam`, does not verify uid, does not require `useradd`. **Access authority comes from server-side admission (`--token`, or a revocable `--require-api-key` + `mount --api-key`), not from the Linux user** — and being able to reach the two services is not the same as being able to read any tree. Under the same pair of services, distinct `--view` values map to distinct, mutually invisible views.
- **One user = one tree, unlimited paths inside**: once mounted you may `mkdir`/`touch` any number of files and directories at any depth under that mountpoint, exactly as in your own home. Per-user isolation applies to the whole tree, not to individual paths. `--manifest` is only a convenience for pre-seeding a skeleton, not a cap on path count or depth.

## Shared views & the permission model

GroveFS has exactly two authorization layers — **token = cluster admission, view name = tree choice**. `--view` is not an identity: any uid that resolves the same pair of service coordinates through one manager and mounts the same `--view alice` lands on alice's single authoritative tree and sees the other writers. That is intentional and makes "one view, many users" work out of the box.

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
   ├─ SQLite (default):      one view = one tree directory   <meta-dir>/meta/alice/alice.db · bob/bob.db
   └─ PostgreSQL (--meta-store):  one view = one schema    grovefs_v_616c696365 · grovefs_v_626f62

The unit of isolated storage is the *view*, not the client; content bytes live in the data-server's `<root>/blocks/<key>` (its `--data-dir`), referenced by data_ref inside each tree.
```

- **Many OS users sharing one view**: point every client at the same `mount --view alice --manager ... --roster-key-file ...` and both metadata (single SQLite/PG tree, writes serialized through the central engine) and content (single `blocks/`) converge in-place. If you need **differentiated rights per uid** inside that shared tree (e.g. an auditor mount that can list but never mutate), the recommended way is now a **view-bound / read-only API key** (`grovefs apikey create --for mount --view alice [--ro]` — enforced server-side, revocable, attributable); see “Locking the doors” below. The uid a mount presents is `geteuid()` by default and can be overridden with `mount --run-as-uid N` (handy for role-based mounts against a shared `meta-server`); it is recorded as the entry owner for lock arbitration and audit and is **advisory** — a TCP client can forge it, so it is never an access-control boundary. Real admission is the `--token` / `--require-api-key` gate, and “which tree, read-only or not” is hard-enforced server-side by a key's `--view`/`--ro` binding. A lock op (`flock`/`F_SETLK`, incl. lease renewal) writes the central `locks` table and counts as write-intent, so an `--ro` key is refused on it too (`E_NOTPERM`, surfacing as `EPERM` at the client — deliberately distinct from an HA standby's `E_ROFS` failover signal); a revoke is op-driven and so *does* reach live connections (re-checked about once per second under load).

- **Metadata backend is swappable: SQLite ↔ PostgreSQL** (this is the `meta-server`'s *authoritative* store only; the wire protocol clients see does not change):

  ```bash
  # default: embedded SQLite, one `<home>/meta/<view>/<view>.db` per view
  $Y meta-server --meta-dir /data/cluster --listen 0.0.0.0:50110

  # PostgreSQL: one `grovefs_v_<hex(view)>` schema per view, tx-guarded rename/unlink/lock
  $Y meta-server --meta-store postgres://user:pw@10.0.0.9:5432/grovefs --listen 0.0.0.0:50110   # under PG --meta-dir is optional: the server writes no authoritative data locally
  ```

  Clients mount exactly the same way (`--manager host:port --view alice`, coordinates resolved from the roster). Notes that matter:
  - **PG is mutually exclusive with the in-tree HA flags**: `--meta-store postgres://…` combined with any of `--role standby` / `--peer` / `--witness` fails fast at startup — GroveFS-native HA ships snapshots via SQLite `VACUUM INTO`, which has no PG analogue. On PG, use PostgreSQL's own streaming / logical replication / PITR, which is strictly stronger than the file-snapshot mechanism anyway.
  - Tables are created idempotently on the view's first `M_OPEN` (`CREATE SCHEMA IF NOT EXISTS` + `CREATE TABLE IF NOT EXISTS`), so no pre-migration script is needed; the root row is pinned at `id=1` and `setval(pg_get_serial_sequence(...), MAX(id))` aligns the sequence, matching SQLite's `ROOT_INO` semantics exactly.
  - **Write parallelism with an elastic pool**: PG relaxes the single writer to a bounded SERIALIZABLE write pool — the 8 write + 4 read connections are a **warm floor** (pre-filled at engine open, never reclaimed); `--pg-write-pool-max`/`--pg-read-pool-max` set the burst retention ceiling (default = floor) and `--pg-pool-idle-secs` (default 60s, `0` disables) reclaims above-floor connections after silence; a single view overrides all five with `grovefs view pool`. SSI conflicts (`40001/40P01`) are retried transparently, never leaked to the POSIX caller. Cross-node multi-authority is deliberately not done (USAGE §6.2).
  - **Multiple PG instances**: `--meta-store file:/path/instances.conf` (one `name = postgres://…` per line, first entry = control plane) plus a placement table puts each view on exactly one instance; `view create --pg-instance` pins (the console's new-base-view form offers the same choice as a dropdown), `view move` relocates an existing view (snapshot → build → flip → retire, `--keep-source` leaves a rollback point). `refs rebuild` recomputes per instance and sums; server-side `gc` scans the cross-instance union; fork/graft refuse cross-instance pairs; each instance needs its own data-server. Capacity = Σ per-instance `(max_connections − ops headroom) / (write floor + read floor)`; HA stays per-instance (PG streaming replication / PITR). Details: USAGE §6.2, [docs.html §24](docs/docs.html#pg).

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
$ cat lens.txt                     # client C's lens
hide /photos/private
map /data/raw /team/raw
$ grovefs mount --view alice -m mnt --manager 127.0.0.1:16520 --roster-key-file ~/roster.key --layout lens.txt
manager 127.0.0.1:16520: deployment 'default' epoch 1 resolved meta [127.0.0.1:16521] data [127.0.0.1:16522] (signature verified, valid for 3599s more)
using remote metadata service at 127.0.0.1:16521
layout overlay /tmp/lensWFuU/lens.txt: 1 hide, 0 only, 1 reloc rule(s)
using remote data service at 127.0.0.1:16522
mounting view 'alice' at /tmp/lensWFuU/mnt (fully remote: no local store)

$ ls mnt                                           # through the lens: one synthesized name, team
data  docs  photos  team
$ ls mnt/photos                                    # private is gone
public
$ echo "hello via lens" > mnt/team/raw/note.txt    # write-through, no copy
$ ls mnt2/data/raw                                 # a plain (unlensed) mount sees it at once, at the SOURCE path
big.bin
note.txt
$ ls -i mnt/team/raw/note.txt mnt2/data/raw/note.txt   # one and the same inode
9 /tmp/lensWFuU/mnt/team/raw/note.txt
9 /tmp/lensWFuU/mnt2/data/raw/note.txt
$ mkdir mnt/team                                   # 'team' is lens scaffolding — name taken
mkdir: cannot create directory '/tmp/lensWFuU/mnt/team': File exists
$ rm mnt/team/raw/big.bin                          # what the lens deletes IS the source entry
$ ls mnt2/data/raw
note.txt
```

Rules are parsed once at mount time (an invalid file fails fast rather than silently showing everything). The lens is a **per-mount-instance** presentation layer — nothing is persisted, no server state changes — so it composes with the one metadata path a mount has (`RemoteMeta`), with `--ro`, and with a read-only view-bound API key. Against a shared `meta-server`, invalidations for entries living under synthesized dirs are broadcast by *source* path and may miss lens coordinates; add `--no-cache` (zero attribute/entry cache) in that setup.

## Server-side derived views: one copy of data, a shape per `--view` name (`grovefs view`)

`--layout` is configured per client. When shapes should be **managed centrally** — so that a team shares one display contract under a name — use derived views: `grovefs view define` stores the rules in the meta root (`<meta-dir>/meta/views/<name>.layout`, first line `base <physical view>`, remaining lines the same four verbs as `--layout`; on the SQLite backend this is the file, while `view … --meta-store postgres://…` writes a row into PG's `grovefs_control.view_defs`), and clients simply `mount --view <name>` with **no local rules file**. Derived (virtual) views mount **read-only, always** — the shape is a centrally governed distribution, every write belongs to the original (base) view. One definition renders the same shape on every machine (a server-side `LayoutMeta` singleton per derived name, so virtual inodes agree across hosts); access control stays with view-bound / read-only API keys (shape and permission are orthogonal). A mount that passes no `--layout` gets the shape from the SERVER — `meta-server` resolves `<meta-dir>/meta/views/<view>.layout` and reports it in the `M_OPEN` ack, so a client never needs a rules file.

```bash
$ cat .demo5/docsonly.rules
# show only the docs subtree
only /docs
$ grovefs view --meta-dir $BASE define --name docsonly --base team --rules docsonly.rules
defined derived view 'docsonly' over base 'team': 0 hide, 1 only, 0 reloc rule(s)
note: mounts of 'docsonly' are read-only (virtual shape) — write to base view 'team'
$ grovefs view --meta-dir $BASE define --name team --base team --rules docsonly.rules   # name == base
error: derived view name must differ from its base
$ grovefs view --meta-dir $BASE list
view 'docsonly' -> base 'team': 0 hide, 1 only, 0 reloc rule(s) (read-only)
base view 'team' -> own tree meta/team/team.db (read-write; deletes free a shared object only when no view holds it)

$ grovefs mount --view docsonly -m mnt --manager 127.0.0.1:16510 --roster-key-file ~/roster.key   # mount by name, no --layout
manager 127.0.0.1:16510: deployment 'default' epoch 1 resolved meta [127.0.0.1:16511] data [127.0.0.1:16512] (signature verified, valid for 3599s more)
using remote metadata service at 127.0.0.1:16511
derived view 'docsonly' over base 'team': read-only lens
note: derived (virtual) views mount read-only — write to base view 'team'
using remote data service at 127.0.0.1:16512
mounting view 'docsonly' at /tmp/vdemo0j0V/mnt (fully remote: no local store, read-only)
$ ls mnt                                       # the derived-name mount: just docs
docs
$ ls mnt_full                                  # the base name mounts the same tree, complete and writable
docs
photos
$ echo "new entry" > mnt_full/docs/2026/b.md   # a write on the base tree…
$ ls mnt/docs/2026                             # …shows in the derived view after the short TTL (immediate via a shared meta-server: pushes are double-broadcast)
a.md
b.md
$ echo "from lens" > mnt/docs/2026/c.md        # writes through a virtual view are refused, always
bash: /tmp/vdemo0j0V/mnt/docs/2026/c.md: Read-only file system
$ grovefs view --meta-dir $BASE rm --name docsonly
removed view definition 'docsonly' (underlying data untouched)
```

Guards and semantics: `define` refuses `name == base`, refuses a base that is itself a derived view (no nested lenses) and refuses to shadow an existing physical `<name>.db`; invalid rules fail fast (never degrading into "show everything"). **Virtual views are read-only by hard contract**: writes belong to the original (base) view only. The `meta-server` ships a derived-name flag in the `M_OPEN` ack, so the client lands a kernel read-only mount immediately (every mutation — and `flock` write locks — give `EROFS` with zero round-trips); the server additionally keeps an **independent authoritative per-op gate** on derived names (answering `E_NOTPERM`, deliberately *not* `E_ROFS`, which clients read as an HA failover signal), so old or crafted clients cannot bypass it — the verdict lives on the side that holds the store. **Edits hot-apply, no remount**: the `meta-server` checks the definition file's mtime before dispatching every op, so a replaced definition reaches established mounts within the client's cache TTL (≤2s) — and the read-only contract survives the reshape; a `view rm` deliberately does **not** hot-apply (live mounts keep the shape they opened with until remount — nobody has the tree yanked out from under them). Behind a shared `meta-server`, a derived view's subscribers register in **both its own bucket and the base name's bucket** and every write is broadcast to both, so base writes land in derived mounts even without `--no-cache` (verified by section 9 of the `layout` e2e: base writes visible in the derived mount, derived mounts refuse writes/deletes with `EROFS` and leave the base tree untouched while the base name stays writable on the same server, a re-define reshapes a live mount). `grovefs view rm` deletes only the definition, never data. `grovefs view` operates on server-local files — run it where the `meta-server` lives (or share the same `--meta-dir`); under HA, `meta/views/` is control-plane config and must be synced to the standby by hand. **Base views are a first-class act**: `grovefs view --meta-dir D create --view NAME` stands up a physical tree — it is the *only* kind of view whose deletes can free bytes, and only after the cross-view `refs` ledger (§Key features) confirms no other view still holds the object; derived views and aliases never own physical deletion. `grovefs refs --meta-dir D rebuild` repairs the ledger after an out-of-band `rm meta/x/x.db`.

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

Derived views govern *what a view looks like*; **pure aliases** govern *what it is called* — the name is the alias, the mapping is the entry. The definition shares `<meta-dir>/meta/views/<name>.layout`, first line `alias <real view>`, no rule lines. The mirror image of a derived view: an **alias mount shows the FULL tree and is WRITABLE** (it simply is the real view under a public name), while the **real name stops being a mount entry point** — the `meta-server` refuses a mount of a governed real name, so public names rotate freely and the storage identity stays unexposed (guessing the real name no longer opens a door):

```bash
$ grovefs view --meta-dir $BASE alias --name teamwork --real team
defined pure alias 'teamwork' -> real view 'team'
note: 'team' is no longer a mount entry point while this definition exists (the meta-server refuses a mount of the real name); alias mounts show the FULL tree and are WRITABLE
$ grovefs mount --view teamwork -m /mnt/work --manager 10.0.0.5:50055 --roster-key-file ~/roster.key   # full tree, writable
$ grovefs mount --view team -m /mnt/t --manager 10.0.0.5:50055 --roster-key-file ~/roster.key          # the governed real name: refused
manager 10.0.0.5:50055: deployment 'default' epoch 1 resolved meta [10.0.0.5:50241] data [10.0.0.5:50240] (signature verified, valid for 3599s more)
using remote metadata service at 10.0.0.5:50241
error: view 'team' is governed by an alias — mount the alias name instead (`grovefs view list` shows it)
error: grovefs: background mount exited (exit status: 1) before mounting
```

When a public name leaks, rotate it in **one gapless command** with `--retire` (write the new alias before removing the old, so the real name stays governed throughout — there is no window where it becomes mountable by itself):

```bash
$ grovefs view --meta-dir $BASE alias --name teamwork2 --real team --retire teamwork   # old public name leaked, swap to teamwork2
rotated public name for real view 'team': 'teamwork' -> 'teamwork2' (gapless — 'team' stayed governed throughout, 'teamwork' no longer resolves)
note: legitimate users just remount under 'teamwork2'
$ grovefs mount --view teamwork -m /mnt/leak --manager 127.0.0.1:16510 --roster-key-file ~/roster.key   # the retired public name remounts nowhere near the real tree
error: view 'teamwork' does not exist on this meta-server (sqlite): no tree and no definition — views are created explicitly, so unknown names are refused instead of becoming an empty tree; create it first: `grovefs view --meta-dir /tmp/retiretvmc/home create --view teamwork` (or seed a tree with `grovefs view import`)
error: grovefs: background mount exited (exit status: 1) before mounting
```

Guards and semantics: `alias` rejects `name == real`, shadowing a physical `<name>.db`, derived-view targets, and alias chains (resolution is single-hop by design); one real view may carry at most one public name (`--retire` rotates within one command — write-before-remove, so it is never blocked by this invariant — and refuses a `--retire` pointing at a derived view or a non-existent alias, so a shape definition can never be dropped by accident). `grovefs import` refuses an alias target — an alias is not a storage identity, and writing a separate `<alias>.db` would create a shadow tree nothing reads. `view rm` on an alias reopens the real name for NEW mounts (established connections are untouched); rotation/rm only affect NEW mounts, and a retired name resolves to a fresh empty namespace that never reaches the real tree. Note the honest boundary: hiding/rotating the real name is name governance (leak hygiene), not a security mechanism — public names must be distributed to be used and rotation cannot revoke an existing token-holder's access, so real isolation stays token + api-key. (Verified by sections 10/11 of the `layout` e2e suite — 29 alias assertions; see USAGE §3.4 for full transcripts.)

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
$ grovefs mount --view release -m .demomcp/mnt --manager 127.0.0.1:50055 --roster-key-file ~/roster.key &
derived view 'release' over base 'team': read-only lens
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

Semantics and boundaries: ten tools — eight governance ops (`list_views` / `define_derived_view` / `alias_view` / `fork_view` / `graft_view` / `remove_view` / `preview_rules` / `validate_definition`) plus two read-only introspection tools (`list_view_tree` / `get_view`) — and the `grovefs view` subcommands share **one library implementation** (`grovefs::mcp`): validations, refusal strings and file mutations are byte-identical, one contract with two entrances — an agent cannot do anything the CLI cannot, nor bypass a guard (derived mounts stay read-only, aliases keep refusing the governed real name, `--retire` stays write-before-remove, `fork_view` copies namespace rows only — never bytes — into an independent per-view metadata db whose structure is writable while content stays `EROFS`, and `remove_view` on a fork drops only that tree and rebuilds the shared ledger; hot-reload likewise belongs to the `meta-server` entrance — a mount resolves its definition once at `M_OPEN`). `list_view_tree`/`get_view` are **metadata-only introspection**: they open a view's tree read-only through whichever store the server fronts (SQLite `meta/<view>/<view>.db`, a PG `grovefs_v_<hex>` schema) and walk it through the same `LayoutMeta` lens a mount uses (no mount, no object bytes), so an agent writes rules against real display paths and previews a shape before committing it; a tree that store cannot reach is refused by name rather than half-walked, and the `size` in a rendered entry is the metadata field rather than a byte count (dirs 4096, files 0, symlinks their target length) — stat through a mount for real bytes. `validate_definition` is a pre-publish dry run: it walks a whole (name, base, rules) through the full `define_derived_view` guard set — invalid names, name==base, shadowing a physical db, replacing a pure alias, bad rule text — returning the exact byte-identical refusal a real publish would produce, or an `accepted` JSON verdict, and it writes NOTHING. Governance failures come back INSIDE the result (`isError: true` + the CLI text) per MCP convention; only protocol-level faults use JSON-RPC errors. Notifications (no id) are never answered; a malformed frame gets `-32700` and the stream recovers. **Control plane only**: it never mounts and never writes data. By default it trusts whoever can spawn the process (the same OS-level boundary as reading and writing `--meta-dir` directly); auth across machines remains the server's token / API key. When it fronts many distinct callers, `grovefs mcp --require-api-key` opens an opt-in API-key gate (next subsection) — the same credential family, extended by scope to the mount/data doors as well. Wire-up: register `grovefs mcp --meta-dir D` as a stdio server in the client config (a per-call `db_path` argument overrides the default). E2E: the `mcp` suite, 77 assertions (handshake/discovery/inline-rule reshapes/path discovery + shape preview/pre-publish dry-run/alias rotation/fork lifecycle/CLI-identical wording/protocol error paths) and the `apikey` suite, 61 assertions; see USAGE §3.5. The agent-side operating manual (in English, for international deployments) ships with the repository under `skills/grovefs-mcp/` (`SKILL.md` + `reference.md` + `examples.md`, plus the dependency-free stdio client `scripts/mcp_call.py` and `scripts/smoke.sh`, a connection self-test that runs the whole workflow against a throwaway store).

### Locking the doors: API-key auth, scopes, view binding / read-only, and call audit (`grovefs apikey`)

One server often faces MANY distinct callers — an agent runtime fronts a `grovefs mcp`, several client machines mount the same `meta-server` over the LAN — and that needs revocable credentials plus per-caller attribution. `grovefs apikey` mints keys (**authentication consults only the stored SHA-256 digest** — the plaintext is echoed by `create`/`rotate`, and the store additionally keeps a reversible ciphertext copy of it that needs the store key to open, so a leaked `api_keys.db` still yields nothing usable) and tags each with a **scope**: `--for mcp` (opens the governance interface only), `--for mount` (opens a `--require-api-key` meta/data server only), `--for any` (the **default**; a row whose scope is absent or unparseable authenticates as this tier). Both entrances share ONE registry (`<meta-dir>/meta/_control/api_keys.db`); the scope keeps them isolated — a mount credential cannot be replayed against the governance API, an agent key cannot open the data plane. A mount key can additionally carry **two access-control dimensions**: `--view V` binds the credential to ONE physical view — `M_OPEN`ing any other view is refused at the handshake with `E_AUTH` (a system-level key without `--view` opens any view); `--ro` marks it read-only — the server refuses every write-intent op (`E_NOTPERM` on the meta plane, `EROFS` on content writes at the data server) and the `M_OPEN` ack tells the client to mount read-only, so read-only is enforced rather than trusted. **Governance gate**: `grovefs mcp --require-api-key` gates every method that reads or writes governance state — `tools/list`, `tools/call`, `resources/list`, `resources/templates/list`, `resources/read`, `completion/complete` (`initialize` / `ping` / `prompts/*` / `logging/*` always stay open so a client can handshake and show its key). **Mount gate**: `meta-server --require-api-key` gates `M_OPEN` and `data-server --require-api-key` gates `OP_OPEN` (`--meta-dir` points the data server at the metadata root, defaulting to its first `--data-dir`); the client presents `mount --api-key <ID:SECRET>` (or exports `GROVEFS_API_KEY` so the secret never lands in argv). **Audit gate**: every `tools/call`, every `resources/read` and every mount connection — refusals included — is appended to one table (timestamp, key, plane (`method` = meta|data|the MCP method), tool, success, detail) that `grovefs apikey calls` reads back. That table **does not grow towards forever** — one row per governance call, per MCP tool call and per mount handshake, so a deployment living on the console accumulates it whether anyone reads it or not: give the server `meta-server --audit-retention 90d` (a background sweep every 1/24 of the window, run once at start before it naps; needs `--control-listen`, and silence means 7d unless you say `never`), or run the same cut by hand with `grovefs apikey purge --before 90d [--dry-run]`. purge is a SUPERUSER verb and deliberately **not a console button** — the account whose actions are recorded here should not be the account allowed to trim them. Revoking only marks the row `revoked` (past calls keep their attribution); a revoked key immediately fails authentication **and closes mounts that are already up**. Storage is its own local SQLite file (`meta/_control/api_keys.db`), control-plane state. What a row keeps is the SHA-256 digest (the only thing authentication consults) plus a `sbx1.<nonce>.<ct>.<mac>` envelope of the full `<id>:<secret>` — **the plaintext never enters the store in readable form** — and `apikey secret --id X` hands back exactly what `create` echoed. Opening that envelope needs wrap material: env `GROVEFS_STORE_KEY` first, else `<meta-dir>/meta/_control/store.key` (0600, created atomically on first use); with neither available the row stores no copy (`secret` answers `never stored`) and the store **never falls back to writing plaintext**. A read-back finishes by comparing the digest, so what it returns is always a string the authentication path accepts. Every row also keeps **three pieces of bookkeeping**: a `--note` remark (≤500 chars, over the limit is refused rather than truncated), a `--ttl` deadline the server turns into `expires_at` (a past-deadline key fails authentication exactly like a revoked one; `apikey update --ttl` is the documented way back), and `created_by`. The `key` column `list` prints is the stored `secret_hint` — **first four and last four hex digits** — enough to recognise a credential on screen while a leaked database still yields nothing usable; “I need the full plaintext” is answered by `apikey secret` reading that copy back — through a `--control-server` or the console only for the row's own owner (or a superuser), and such a read lands in `apikey calls`, while a CLI read straight off the local store is **not** audited (whoever can open that file already owns it). For a row the store holds no copy of, `rotate` minting a fresh secret for the same id remains the only way back. Transcripts below are from a real run:

```
$ grovefs apikey --meta-dir <home> create --label laptop-alice --for mount   # mint <id>:<secret> and echo it (a wrapped copy lands in the store too)
apikey created (a recoverable copy is now stored — `grovefs apikey secret --id 3fed9a02db704c3d` reads it back):
  id:    3fed9a02db704c3d
  owner: <none>
  label: laptop-alice
  scope: mount
  view:  <any>
  rw:    rw (read-write)
  expires: never (set one with --ttl, or `apikey update --ttl`)
  key:   3fed9a02db704c3d:9f126bb5f3414955b17089ed89dee714

$ grovefs apikey --meta-dir <home> secret --id 3fed9a02db704c3d   # read the same credential back later
apikey secret — the stored copy of this credential:
  id:    3fed9a02db704c3d
  owner: <none>
  label: laptop-alice
  scope: mount
  view:  <any>
  key:   3fed9a02db704c3d:9f126bb5f3414955b17089ed89dee714

  authentication still runs on the stored digest; this copy is for
  handover.
  a direct read of the local store is NOT audited — whoever can
  open that file already owns it; a read through a meta-server
  (`--control-server`) is, and shows in `apikey calls`.

$ grovefs apikey --meta-dir <home> create --label reader --for mount --view alice --ro   # view-bound + read-only
apikey created (a recoverable copy is now stored — `grovefs apikey secret --id a1d7131e05f74ce8` reads it back):
  id:    a1d7131e05f74ce8
  owner: <none>
  label: reader
  scope: mount
  view:  alice
  rw:    ro (read-only)
  expires: never (set one with --ttl, or `apikey update --ttl`)
  key:   a1d7131e05f74ce8:e6555ce683554fdd92ad877ad3b63a06   # opens view alice only; every write is refused server-side

# using a bound/read-only key where it doesn't belong is refused outright:
$ grovefs mount --view bob -m /mnt/b --manager ... --roster-key-file ~/roster.key --api-key "$READER_KEY"
error: permission denied: this api key is bound to view 'alice', not 'bob'   # E_AUTH at M_OPEN
$ mkdir /mnt/a/should_fail                                                          # alice's read-only key
bash: /mnt/a/should_fail: Read-only file system                                     # kernel RO from the handshake flag; the server also refuses every write

$ grovefs apikey --meta-dir <home> create --label x --for both              # a typo never silently widens a privilege
error: unknown api key scope 'both' (expected any|mcp|mount|admin)

$ grovefs meta-server --meta-dir <home> --listen 0.0.0.0:50110 --require-api-key
meta-server: api-key auth REQUIRED (present `mount --api-key`, minted by `grovefs apikey create --for mount|any`); a revoke closes live mounts on their next op

# refusal reasons travel byte-for-byte with the handshake (not just an error code):
$ grovefs mount --view alice -m /mnt/a --manager ... --roster-key-file ~/roster.key                       # no key at all
error: no api key presented: this server requires one (pass `mount --api-key ID:SECRET` or export GROVEFS_API_KEY; mint with `grovefs apikey create --for mount`)
$ grovefs mount --view alice -m /mnt/a --manager ... --roster-key-file ~/roster.key --api-key "$MCP_KEY"  # an agent key mounting the view
error: authentication failed: this api key is scoped to 'mcp', not 'mount'

$ grovefs apikey --meta-dir <home> calls --limit 6        # mount connections and governance calls, one table
last 6 call(s), newest first:
ts   key_id   method   tool   ok   detail
...  aac1f51a0d5641dd  meta  M_OPEN   yes  view='alice' uid=1005
...  aac1f51a0d5641dd  data  OP_OPEN  yes
...  -                                 meta  M_OPEN   no   view='alice': authentication failed: this api key is scoped to 'mcp', not 'mount'

$ grovefs apikey --meta-dir <home> revoke --id aac1f51a0d5641dd   # the row stays for historical attribution
apikey: revoked id='aac1f51a0d5641dd' (label 'laptop-alice'); it can no longer authenticate, past calls keep attribution
  a LIVE mount presenting this key is refused on its next op (within the 1s re-check window) and unmounts itself;
  a `grovefs mcp` session in flight keeps its identity until it reconnects.

$ grovefs apikey --meta-dir <home> rotate --id aac1f51a0d5641dd   # a lost secret costs an identity nothing: same id, fresh secret
apikey rotated (the new credential's copy is stored — `grovefs apikey secret --id aac1f51a0d5641dd` reads it back):
  id:    aac1f51a0d5641dd
  owner: <none>
  label: laptop-alice
  scope: mount
  view:  <any>
  key:   aac1f51a0d5641dd:<a new 32-hex secret>

  the id and every binding are unchanged — only the secret was replaced;
  the OLD secret fails its next handshake, and a live mount established on it
  stays open until it reconnects — `apikey revoke` is the kill-now verb.

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

$ grovefs apikey --meta-dir <home> create --label nightly --for mount --note "staging box, hand over end of March" --ttl 30d   # the bookkeeping goes in with the mint
apikey created (a recoverable copy is now stored — `grovefs apikey secret --id 3dfd73c7b4684edc` reads it back):
  id:    3dfd73c7b4684edc
  owner: <none>
  label: nightly
  scope: mount
  view:  <any>
  rw:    rw (read-write)
  note:  staging box, hand over end of March
  expires: 2026-12-18T14:52:10.242021918+00:00      # the SERVER compares this at each handshake; it is not a client-side hint
  key:   3dfd73c7b4684edc:0609b45f71ce44bf9d0f2fca8d44a89e

$ grovefs apikey --meta-dir <home> list                    # the key column is the stored fingerprint (head4…tail4), never the secret
#   id                 key          owner            label            scope   view         rw   created                   last_used                 expires / revoked / note
1   3dfd73c7b4684edc   0609…a89e    -                nightly          mount   <any>        rw   2026-11-18T14:52:10.242127448+00:00  -   expires 2026-12-18T14:52:10.242021918+00:00 | note: staging box, hand over end of March
2   a1d7131e05f74ce8   e655…3a06    -                reader           mount   alice        ro   2026-11-18T14:52:10.223378367+00:00  -   never
3   3fed9a02db704c3d   9f12…e714    -                laptop-alice     mount   <any>        rw   2026-11-18T14:52:10.190396303+00:00  -   never

$ grovefs apikey --meta-dir <home> update --id 3dfd --note "handed to ops" --ttl never   # only the two bookkeeping fields move; id, bindings, owner and audit stay
apikey updated: id='3dfd73c7b4684edc' (label 'nightly')
  note:    handed to ops
  expires: never

$ grovefs apikey --meta-dir <home> update --id 3dfd --ttl "3 weeks"                      # a misspelt duration is refused, not quietly read as "never"
error: bad ttl '3 weeks': expected a count and a unit, like 45m, 24h or 7d
```

A client that cannot customize its handshake is covered by exporting `GROVEFS_MCP_API_KEY='<id>:<secret>'`, which pre-authenticates the whole governance server at launch (`GROVEFS_API_KEY` is the mount-side counterpart). Honest boundary: the key authenticates **the caller at the door** — it lets (or stops) a caller issuing governance writes, lets or stops it opening a view tree, and makes each admission attributable — without changing governance semantics; its `--view`/`--ro` dimensions are enforced server-side. Live-connection re-checking is **op-driven** (the server only re-verifies when traffic arrives), so the revocation boundary is “the first server round-trip after the revoke”: reads fully answered by the kernel cache need no round-trip and are therefore not interrupted (the next round-trip is). `--token` and `--require-api-key` can both be set — then both gates must pass. With `--require-api-key` off the server audits calls but gates nothing. On the stored copy, one honest limit worth stating plainly: it lives in the SAME database as the digest, and by default its store key lives next to that database (`<meta-dir>/meta/_control/store.key`), so a single cold copy of `meta/` takes both — the copy is an operational convenience (handover, nobody has to remember a plaintext), **not** a new security boundary; keeping the material outside the store means exporting `GROVEFS_STORE_KEY`, which takes precedence over the file. Lose the material and those rows stay `unopenable`, recoverable only by `rotate`. E2E: the `apikey` suite, 61 assertions (create/list/only the `sbx1` envelope on disk/refuse-without-key/refuse-wrong-secret/admit-with-valid-key/call-attribution/revoke-fails-closed-but-stays-listed/prefix-revoke/env-pre-auth/default-ungated-regression/set-ro in-place ro↔rw flip with authentication kept/short 16+32-hex format/rotate keeping the id while old secret refuses, new admits, digest plus wrapped copy on disk, revoked refuses/list --label·--scope filters/`--note`+`--ttl` stored and echoed/the key column showing only the fingerprint while no plaintext sits on disk in the clear/`apikey secret` reading the stored copy back byte-for-byte with the mint, the copy following `rotate`, an unknown id refused/`list --note` substring filter/`update` editing the remark and the deadline (`--ttl never` clears it, no flag is refused)/an expired row marked `EXPIRED` and refused at the gate with `-32001`/the same key revived by a new `--ttl`/`update` refusing a revoked id) and the `mountkey` suite, 76 assertions (scope minting + typo refusal/refusal reasons in the protocol/admission and env fallback (secret absent from argv)/connection audit down to key and view/scope isolation both ways/revocation closing a LIVE mount with no stale endpoint/other keys unaffected/token+key double gate/independent data-plane gate/failover carrying the credential/default ungated path unchanged/a view-bound key refused across views with its refusal audited/an `--ro` key's writes refused server-side/`set-ro` hot-flipping rw↔ro on a live mount with no reconnect); see USAGE §3.5.

## Management console: views, keys and base-view delegation from a browser (`grovefs web`)

The remote governance port lets any machine administer the same metadata, but its entrance is still a terminal: verbs to remember, a key to carry, refusal text to read. The three high-frequency jobs — look at the views, hand out a key, give someone a place to manage — are cheaper clicked; and the ownership “**this account runs this base view, including the forks, aliases and keys that hang off it**” is something a shared admin key cannot express. `grovefs web` is the HTTP gateway in front of that port — an **independent subcommand** (HTTP never enters `meta-server`) that **adds no governance logic of its own**:

```bash
# Install time: only the process holding the store may place the first admin
# (applied solely while the account table is empty; the password has a FILE or STDIN,
#  never a --admin-password — argv is readable by every local process)
$ grovefs meta-server --meta-dir .webdoc/srv --listen 127.0.0.1:15698 --control-listen 127.0.0.1:15699 \
    --admin-user rootops --admin-password-file ./root.pw
meta-server: web accounts — created the first web account 'rootops' (role admin) from the install-time flags — sign in with it, then delegate base views
meta-server: web console sign-ins are verified HERE (`grovefs web --control-server 127.0.0.1:15699`); accounts and their base-view delegation live in meta/_control/web_accounts.db

# The gateway stores nothing: it needs the governance address + one `--for admin` account key
$ grovefs web --control-server 127.0.0.1:15699 --api-key-file ./control.key --listen 127.0.0.1:15687
grovefs web: management console on http://127.0.0.1:15687/
  control plane : 127.0.0.1:15699 (backend sqlite)
  sign-in       : accounts of that server's web store; the first owner comes from its
                    --admin-user flag
  sessions      : 8h idle window, 4 worker thread(s)

# Create an account, then give it a place (the same three actions live on the page's Accounts tab)
$ grovefs web --control-server 127.0.0.1:15699 --api-key-file ./control.key account create --account bob --password-file ./bob.pw
web account created: bob
  role:  user
  bases: none — it can sign in and govern nothing yet
  note:  delegate a base view before this account can manage anything

$ grovefs web --control-server 127.0.0.1:15699 --api-key-file ./control.key grant add --account bob --base main
bob now governs 'main' and everything derived from it, as granted by rootops.
That covers its forks, aliases and lenses, and the keys bound to any of those names.
```

A grant names a **base view**, not a leaf: every fork, alias and lens grown under that tree afterwards — and the keys bound to those names — falls inside the delegation automatically, which is the difference between handing someone a place and handing someone a name. Signing in as `bob` returns his own reach (`{ "account": "bob", "role": "user", "bases": ["main"] }`): he lists his views, defines lenses and aliases, forks his base, reads `view.stats` (whose `family` contains his fork), and mints `--for mount`/`--for mcp` keys **for his own views** with `set-ro` and revoke. Anything outside it comes back as the **server's own sentence**, not text the gateway invented:

```json
{ "ok": false, "error": "account 'bob' is not an administrator, and 'view.create' is not per-view governance: a new base view picks a storage home (PG instance / data root), which is a deployment decision — an admin must run it (or grant you the base view it acts on)" }
{ "ok": false, "error": "'gc.scan' is not available from the console: it stays an operator CLI command, where its dry-run report and confirmations are part of calling it" }
```

Honest boundary: the gateway holds one `admin`-scoped key, and whoever can read its configuration can already govern the deployment — hence the `127.0.0.1` default bind (a routable bind is refused before the port opens unless `--allow-insecure-http` is passed explicitly, with the reverse-proxy recipe in the error text), and hence “**a sign-in creates no capability, it only bounds and attributes one**”: a session may act only as its account, and every call is audited as `account@ip`. Sessions are a `gfs_sid` cookie (`HttpOnly; SameSite=Strict`) plus an `X-GroveFS-CSRF` header on writes, with `Origin` required to match `Host`; five failed sign-ins from one client IP inside 15 minutes start answering 429 (30s base, doubling per extra failure, **even when the password is right**), and “no such account” and “wrong password” answer byte-identically. The route list is hardcoded to 32 ops — `gc.scan`/`gc.apply`/`refs.rebuild`/`view.rm_base`/`fork.graft` are not routable. `grant rm` needs no session expiry: the server re-reads the principal per request, so the withdrawn delegation bites on the **next call**. `account rm` deletes the account and its delegations but deliberately leaves **its api keys alive** (deleting a person must not silently stop a running production mount; the reply counts how many are pending). E2E: the `web` suite, 186 assertions (including rotating one's own key's secret through the page route `apikey.rotate`, id preserved, and a cross-account rotation refused; `apikey.update` editing the remark and the deadline; an expired row the server marks `expired:true` and revives through a new `--ttl`; the bookkeeping fields (`note`/`created_by`/`secret_hint`) reaching the HTTP reply, with the stored fingerprint proven equal to head+tail of the secret echoed at creation; `apikey.secret` reading back its owner's own copy through the page route — byte-identical to what `rotate` answered, adding no plaintext to disk, and refused across accounts; `store.info` carrying the `pgInstances` key (null on a SQLite store, the instance names on a registry); and a count assertion that the route whitelist is exactly 32 ops); full command list, per-page capability table, every refusal verbatim, and the systemd + TLS-reverse-proxy recipes are in USAGE §3.5.3 / §11.4 and `docs/deploy.html` §2.4.

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

$ grovefs data-server --listen 127.0.0.1:50100 --data-dir .demo4/home &
data-server listening on 127.0.0.1:50100 (host lc, 1 data root(s): .demo4/home)
$ grovefs meta-server --meta-dir .demo4/home --listen 127.0.0.1:50053 &
meta-server listening on 127.0.0.1:50053 (base .demo4/home, role Primary)

$ grovefs mount --view legacy -m .demo4/mnt --manager 127.0.0.1:50055 --roster-key-file ~/roster.key &   # a client mount now sees the tree
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

Semantics (35 assertions in the `import` suite): **zero copies on one filesystem** — object and source share the inode (nothing shows up in `df`), with an honest streamed-copy fallback across devices; **live passthrough both ways** — an edit through the mount or an external rewrite of the source file shows up on the other side within the client’s ≤2s TTL; **GC only unlinks** — removing the name in the view deletes only the `blocks/` path, never the source; **idempotent and incremental** — re-running registers just the new names; mtime/atime/mode/owner preserved (`--dry-run` does not even create the view DB). Imported entries are then first-class: reads/writes, locks, snapshots and invalidation push all apply, and derived views/lenses can reshape them into virtual directories with `map`/`link` — that virtual shape then mounts **read-only** (writes go to the imported base view, visible through the shape via the double-broadcast pushes), and `grovefs import` refuses a derived target name outright, since it would write a shadow `<view>.db` no mount ever reads. Like `grovefs view`, run it on the machine holding the store (the `--home`, or the meta/data servers’ root).

## Production topology: central storage + LAN mounts

```bash
# central node: declare the topology, start the bootstrap service, register both services with it
$Y manager --state-dir /srv/mgr define --name prod --meta 10.0.0.5:50110 --data 10.0.0.5:50100
$Y manager --state-dir /srv/mgr serve --listen 0.0.0.0:50055
$Y data-server --listen 0.0.0.0:50100 --data-dir /data/cluster --member-id 0 --register-with 10.0.0.5:50055 --join-key-file /srv/mgr/manager/join.key
$Y meta-server --meta-dir /data/cluster --listen 0.0.0.0:50110 --register-with 10.0.0.5:50055 --join-key-file /srv/mgr/manager/join.key

# each client (a remote mount needs no --home: nothing authoritative lives on the client; both coordinates come only from the manager's signed roster, never typed)
$Y mount --view alice \
     --manager 10.0.0.5:50055 --deployment prod --roster-key-file /etc/grovefs/roster.key -m /mnt/alice
```

| Option | Effect |
| --- | --- |
| `--manager host:port` | The ONLY mount entry point: the client points at this one address and both service coordinates are resolved from its signed roster (the hand-typed `--meta-server`/`--data-servers` flags are removed) |
| `--roster-key-file PATH` | the roster VERIFICATION key (64 hex, the same key the manager signs with): an unverified roster fails the mount (fail-closed), never a warning followed by a connect |
| roster meta list | resolves to a FAILOVER list (the primary dies → the standby takes over), the same semantics the old `--meta-server primary,standby` had; the view is served by the central `meta-server` → the same view across machines shares one tree |
| roster data list | resolves to the COMPLETE stripe member set (a comma-separated list enables deterministic cross-server striping); members are not a candidate pool, a shortened list is refused |
| `--data-dir /d1,/d2` | data-server's comma-separated multi-root → intra-host automatic placement (whole objects hashed to one disk; add/remove/reorder roots remap every key and strand old objects — provision many roots up front, or merge disks with LVM/mdadm) |

### Mount bootstrap: declare the coordinates once, point clients at one address

Both coordinates above are PROVISIONING facts (constants an operator decides, not quantities that emerge at runtime), so they can be written in one place and distributed signed:

```bash
# central node: declare the topology (into <state-dir>/manager/deployments.json), then serve
$Y manager --state-dir /srv/mgr define --name prod --meta 10.0.0.5:50110 \
     --data 10.0.0.5:50100,10.0.0.6:50100
$Y manager --state-dir /srv/mgr serve --listen 0.0.0.0:50055

# nodes: register at startup (a node that cannot reach the manager serves anyway, retrying with backoff)
$Y data-server --listen 0.0.0.0:50100 --data-dir /data/cluster --member-id 0 \
     --register-with 10.0.0.9:50055 --advertise-addr 10.0.0.5:50100 \
     --join-key-file /etc/grovefs/join.key --deployment prod

# each client: one address
$Y mount --view alice --manager 10.0.0.9:50055 --deployment prod \
     --roster-key-file /etc/grovefs/roster.key -m /mnt/alice
```

| Aspect | Behavior |
| --- | --- |
| Roster content | the set the operator DECLARED, never "the ones alive right now"; `--require-registered` is a precheck GATE (it refuses the lookup outright) and does not shrink a roster |
| Two keys | the JOIN key authorizes a REGISTRATION (only servers hold it, a client never does); the ROSTER key signs the ANSWER (the manager signs, clients verify) |
| Anti-forgery | an undeclared address is refused even with a correct HMAC; `manager status` shows per declared address who last registered and with what member-id |
| Failure boundary | a manager outage cannot disturb an existing mount (the data plane never goes through it); a new mount that fails verification fails closed |
| Cost | one more port reachable from every node and client; per mount one extra TCP round-trip plus one local HMAC verify; zero added cost on the IO path |

Full commands, verbatim output, rejected alternatives and residual risks: USAGE §4.2 and `docs/adr/0001-manager.md`.

## Consistency

| Mode | Command | Behavior |
| --- | --- | --- |
| **Real-time invalidation push** (default, recommended) | remote mounts (meta resolved via a manager) | After a client's write succeeds, the center broadcasts invalidation (`inval_entry`+`inval_inode`) to that user's other subscribers; peers see it immediately, no polling |
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
$Y apikey --meta-dir /data/cluster create --label client-a --for mount    # echoes <id>:<secret>; the row keeps the digest plus a wrapped copy
$Y meta-server --meta-dir /data/cluster --listen 0.0.0.0:50110 --require-api-key
$Y data-server --listen 0.0.0.0:50100 --data-dir /data/cluster --require-api-key --meta-dir /data/cluster
$Y mount --view alice --manager ... --roster-key-file ~/roster.key --api-key '<id>:<secret>' -m /mnt/alice   # or export GROVEFS_API_KEY to keep it out of argv
$Y apikey --meta-dir /data/cluster calls --limit 20                       # every connection (refusals too) attributed to its key
$Y apikey --meta-dir /data/cluster set-ro --id <id> --rw                    # flip an --ro key rw↔ro in place; a live mount follows within ≤1s, no reconnect
$Y apikey --meta-dir /data/cluster revoke --id <id>                       # closes that key's LIVE mounts (seconds)
```

The two substantive differences from a token: ① refusal reasons travel byte-for-byte back with the handshake (missing key / unknown id / wrong secret / wrong scope / revoked each print their own text), and the server **re-checks live connections once a second** — after a revoke that mount is interrupted on its first server round-trip, prints the server's own wording and unmounts itself cleanly (no `Transport endpoint is not connected` left behind), while mounts on other keys keep running; ② mount connections and governance calls share one audit table. `--token` and `--require-api-key` can both be set (then both gates must pass); with the flag off behaviour is exactly as ungated (auditing still happens). See USAGE §3.5.1/§6.

**Read-only**: `mount ... --ro` → every mutating op returns `EROFS` (kernel `MS_RDONLY` + filesystem-side guard).

**Performance knobs** (every figure below is the CLUSTER tier — a `meta-server` + `data-server` pair on this host's loopback, rerunnable via the scripts named in the last clause): `--threads N` (default 4, per-thread `/dev/fuse` fd), chunked parallel IO (reads ≥1MiB, writes ≥4MiB), large-file striping (≥16MiB), `touch_mtime` debouncing (one write per inode per epoch second, which is the store's whole resolution; a fresh file starts with its creation second already stamped, and the kernel's close-time `SETATTR` echo is dropped when its mtime falls in the stored second and its size already matches the object, so create+write+close pays one create transaction and nothing else), **creates that reserve only the name** — 0.281ms/op (3,560 ops/s) for `touch`, `mkdir` 0.211ms (4,741 ops/s), `utime` 0.391ms (2,561 ops/s), with `meta.create` p50 127µs and `remeta.rpc` p50 86µs on the first: the `mkdir → utime` gap is round-trip COUNT (two RPCs vs four), and the backend object is opened by the first `write`/`truncate`, so a name-only create opens nothing and caches no fd (an unwritten file reads back as 0 bytes, `fsync` succeeds, `unlink` deletes just the name). Concurrency of that name-only create on one mount, one parent dir per thread: T=1 **3,201** / T=2 **6,329** / T=4 **8,683** / T=8 **4,846** / T=16 **4,640** files/s — gains through T=4 from overlapping commits, then the single writer connection and RPC thread queue cancel them; raising the server's warm READ floor to 8 does not change the shape, because the bottleneck is on the write side. Raw SQLite on the same volume commits at p50 46µs (WAL+NORMAL), so SQL is not the bulk of an op; `synchronous=OFF` (30µs) is deliberately NOT offered as a durability-lowering knob, and `FULL`/`journal_mode=DELETE` cost 16.7ms/20.5ms. **Deletion**: there is no client-side group commit (a mount holds no authority, so it cannot fold N requests into one transaction), and the win comes from commits overlapping on the SERVER — `rm` of 2000 small files single-threaded **3,013 files/s** (`meta.unlink` p50 101µs), and at `--threads 16`, one parent per thread, 400 files each: T=1 **2,082** / T=2 **4,270** / T=4 **5,062** / T=8 **5,120** / T=16 **5,767** files/s. Batch transactions remain an ENGINE-level API for callers embedding the library (`create_nodes_batch`/`unlink_nodes_batch`: 83µs/row at N=1, 38 at group 3, 25 at group 8; unlink 53.9/30.0/18.7/13.2/10.8 µs/row for batch 1/2/4/8/16). Scripts: `probe_sqlite_create_breakdown.sh`, `probe_wal_modes.sh`, `probe_unlink_cluster.sh`. Beyond that: a warm read pool per view engine on both backends (SQLite: 1 warm read connection with bursts retained up to 8, `GROVEFS_SQLITE_READ_POOL` raises the warm floor and `0` closes the pool; PG: `PG_READ_POOL_SIZE` = 4 read connections with `GROVEFS_PG_READ_POOL_MAX` setting the burst ceiling, each connection caching its own prepared statements ≤16; slots open lazily and return on drop), PG metadata reads measured at **0.358–0.360 ms/op** (≈2.79k/s, `.build-bin/probe_pg_read.sh`), an opt-in async-commit tier for PG writers (`GROVEFS_PG_SYNC_COMMIT=off`, default on: the one stable gain is **+18% at N=4**, inside run-to-run noise elsewhere, paid for with the last commits on a crash), creation replies that carry the new row so a create costs two metadata round trips instead of three, SQL-paged directory listings (`readdir_page`, `LIMIT/OFFSET` filled into the reply buffer in geometric chunks — 32→1024 for READDIRPLUS, 128→1024 for plain READDIR) and `FUSE_READDIRPLUS` negotiation (on by default wherever the provider pages inside its own store — SQLite/PG/treedb engines and `LayoutMeta` shapes all do, and a remote mount pushes its `offset/limit` down through `M_READDIRP`; `--no-readdirplus` falls back to plain READDIR plus per-entry LOOKUP and is the A/B knob). Measured on one 5001-entry directory through the cluster: `ls -l` **0.270–0.274s**, **0.575–0.629s** with paging only; rerun `.build-bin/probe_readdir_cluster.sh`.

## Backup and migration

Metadata is bound to NO hardware: a `node` row stores only the `data_ref` (the object key), and an object's physical location is a pure function of that key — `blocks/<FNV1a(key)%256 as two hex digits>/<key>` — with no absolute path, device number, hostname or host inode in it. GroveFS's own `ino` lives in the DB too, independent of whatever filesystem hosts it. So changing machine, OS or directory needs nothing special, as long as you carry BOTH halves — the metadata and the content bytes:

| What travels | Where it lives | Notes |
| --- | --- | --- |
| Content bytes | `blocks/` of the store root, i.e. under `data-server --data-dir` (in the minimal cluster both are the same root) | Plain files: `rsync -a -H` or a filesystem snapshot; the bucket layout derives from the key, so no reindexing after the copy |
| View metadata | `<meta-dir>/meta/<view>/<view>.db` | One independent SQLite tree per view; **never plain-copy it while there is write activity** (the WAL may not be merged) |
| Cross-view ledger | `<meta-dir>/meta/_control/refs.db` | Which views hold each object; recompute after an import or a cold copy with `grovefs refs --meta-dir D rebuild` |
| View definitions | `<meta-dir>/meta/views/*.layout` | Plain-text definitions of derived / alias / fork views, no host paths — ship them with the control plane (PG backend: `grovefs_control.view_defs`, travels with `pg_dump`) |
| API keys / audit | `<meta-dir>/meta/_control/api_keys.db` | Only if you use `apikey`; keys are stored as SHA-256 digests plus a reversible ciphertext copy of the secret (the copy needs `<meta-dir>/meta/_control/store.key` or `GROVEFS_STORE_KEY`; PG backend: `grovefs_control.api_keys`/`mcp_calls`, inside `pg_dump`, but the store key is not in the database) |

**Two ways to migrate:**

1. **Cold-copy the whole directory** (fastest, same-architecture move): stop every mount and both servers, `rsync -a -H` the store root (`blocks/` + `meta/`). A running SQLite file must not be plain-copied; if you cannot stop, take a consistent snapshot with `sqlite3 <db> ".backup /tmp/x.db"` (or `VACUUM INTO`).
2. **Export/import one view** (across machines, selective): `view export` writes one tree as host-neutral JSONL (a header plus one row per node — id, inode, names, timestamps, `data_ref`; **no content bytes**), `view import` rebuilds `<view>.db` in the target store root and recomputes the ledger. The import is atomic: a temporary tree is built beside the target and only `rename`d into place once everything validates, so a bad file or a failed `--replace` cannot damage the old tree.

```bash
# source machine: export one view (fine while mounted — it reads a snapshot)
grovefs view --meta-dir /data/src export --view team --out /tmp/team.jsonl
# carry the content bytes to the target (only blocks/; metadata is in the JSONL)
rsync -a -H /data/src/blocks/ /data/dst/blocks/
# target machine: import it as a new base view
grovefs view --meta-dir /data/dst import --view team --in /tmp/team.jsonl
# start the manager and declare the topology on that root, register both services with it (= the minimal cluster), then mount from a client through the roster
grovefs --home /data/dst manager define --meta 127.0.0.1:50053 --data 127.0.0.1:50100
grovefs --home /data/dst manager serve --listen 127.0.0.1:50055 &
grovefs meta-server --meta-dir /data/dst --listen 127.0.0.1:50053 --register-with 127.0.0.1:50055 --join-key-file /data/dst/manager/join.key &
grovefs data-server --data-dir /data/dst --listen 127.0.0.1:50100 --member-id 0 --register-with 127.0.0.1:50055 --join-key-file /data/dst/manager/join.key &
grovefs mount --view team -m /mnt/team --manager 127.0.0.1:50055 --roster-key-file /data/dst/manager/roster.key   # tree, hard links, modes, owners, symlinks as they were
```

`view import` guards: an existing target tree needs an explicit `--replace`; a target name that already holds a definition slot must be released with `view rm` first; a bad header format/version, a missing root, a dangling parent or a header count that disagrees with the body rejects the whole file. Add `--as-fork <base view>` to restore fork semantics (structure writable, content read-only): the import writes the `fork <base view>` definition and shares bytes through the ledger.

**Measured scale** (default IO, rerun with `bash .build-bin/perf_migrate_scale.sh`): migrating a view of 5,052 nodes holding 5,000 objects — `export` takes **0.07s** and writes 1,161,499 B of JSONL (1.10× the 1,056,768 B view DB, ≈230 B per node), the byte half lands with `rsync -a -H blocks/` in 0.1s, `import` into a store root at a different absolute path takes **0.53s** (ledger rebuild included); handing the new root to a `meta-server` + `data-server` pair and mounting, enumerating the 5,000 files costs **0.22s** with content byte-identical and ownership preserved (`664 uid=1005`). For contrast, registering the same batch of files into the source view (zero-copy import, per-object work) took 1.2s — **migration cost follows the size of the namespace, not the volume of data**, and the bytes travel separately via `rsync -a -H blocks/`.

**The only TOPOLOGY constraints worth knowing** (data placement, not hardware binding):

- **Multi-disk `--data-dir`**: an object goes to disk `FNV1a(key) % number of roots`, so the target must have the same root COUNT in the same ORDER. Collapsing to a single root also works (merge the `blocks/` trees; with one root every key lands there).
- **Cross-server striped objects** (`data_ref = s<N>:…`): chunk i lives on `servers[i % N]` — a LIST INDEX, not an address. The target still needs N data-servers, and the two failure modes differ: a member-count MISMATCH is refused outright (never a silently wrong chunk), while a reordered list is undetectable (the guard only compares N) — the server you handed over lacks the `{uuid}#{i}` it is now addressed for, and that batch of files fails to read. One operational corollary: **changing machine or IP is not a constraint at all** — replacing that one entry IN PLACE in `-o data=` is recognized automatically, no metadata change and no rebalance of already-written objects (the rotating `i % N` placement only decides where NEW objects go).
- **HA `epoch`** lives in each view's `ctrl` table; a whole-directory copy carries it over, so **the old primary must be stopped** after a migration (or reinitialize from a fresh store root), otherwise two primaries can both be online. `view export/import` and `view fork` deliberately do NOT copy `ctrl`: the new tree starts from a clean epoch.
- **The PostgreSQL backend** is not file-copyable: migrate the whole database with `pg_dump` / logical replication; move a single view with `view export/import` (both backend tiers — the PG tier connects straight to the resolved instance via `--meta-store`); relocate an existing view between instances with `view move` (metadata only, content bytes stay in their respective data domains; USAGE §6.2).
- `uid/gid` are numeric advisory metadata and survive by value across systems; symlink TARGET TEXT is user data, kept verbatim and never rewritten because the machine changed.
- **`st_ino` travels with the DB, `st_dev` does not**: `node.ino` is GroveFS's own number (stored in the DB, shared by hard links, copied as-is by `fork` and `export/import`), so it survives a machine or directory change; `st_dev` is allocated fresh per FUSE mount, so a backup/sync tool that dedupes on `(dev, ino)` must rebuild its mapping after a remount or a migrated store.
- **How to confirm nothing was lost after a copy**: object names are UUIDs (**not content hashes, and there is no dedup by content**) and the read path verifies nothing, so a missing file must be looked for deliberately — compare the number of keys referenced by the trees against the `matched` count in `grovefs gc --meta-dir D --data-dir R`; `matched < refs` means keys without objects (the classic symptom of a partial copy or a wrong `--data-dir` list). `rsync -a` preserves mtime (`gc --older-than` relies on it) and a store that ever used `import` must be copied with `-H`.

**Multiple base views**: create as many as you like (`grovefs view --meta-dir D create --view A`), each with its own `meta/A/A.db` — trees, byte references, permissions and deletion stay independent, and `view list` enumerates every bare base view. `view export/import` also works **per view**, so a migration can move one tree only; different base views can bind different API keys (`apikey create --view A`) without interfering.

## Pick a database and follow one track

GroveFS has two metadata backends: SQLite and PostgreSQL. Pick one and follow **one complete path** (deployment → usage → operations → acceptance) without jumping between sections:

- **SQLite (default)**: zero-dependency single binary, single writer, native primary/standby HA. Full path: [USAGE.en.md §15.2 / §15.17.1](USAGE.en.md), web edition [docs.html §6.9.1](docs/docs.html#case-backend-paths).
- **PostgreSQL**: parallel writes (8 write connections per view as a warm floor; burst ceiling and idle reclaim via `--pg-*-pool[-max]` / `--pg-pool-idle-secs`), HA / backup / PITR delegated to PG itself; scale past the connection ceiling with the multi-instance registry + placement (`--meta-store file:…` + `view move`). Full path: [USAGE.en.md §15.3–§15.4 / §15.17.2](USAGE.en.md), web edition [docs.html §6.9.2](docs/docs.html#case-backend-paths).

The two tiers differ in only **3 landing points**: native HA, zero-copy directory ingest `grovefs import`, and in-place `gc` are SQLite-only; every other command keeps the same name and arguments, just swapping the addressing knob `--meta-dir` for `--meta-store <URI>`, and the client mount command is byte-for-byte identical across both tiers. See [docs.html §4.1](docs/docs.html#prod) for the selection rationale and [docs.html §22.1](docs/docs.html#ref) for a per-command comparison.

## Deployment & operations (hands-on)

The full step-by-step guide lives in **[USAGE.en.md](USAGE.en.md) §11–§14** (mirrored in [docs.html](docs/docs.html), re-sequenced into a four-tier zero-to-expert manual with new beginner-prerequisites, internals & extension-development, and performance-tuning methodology sections, plus an auto/light/dark reading-theme toggle): §11 walks through environment preflight → build & install → topology planning → shared token → systemd units (two central services + a templated client mount unit) → firewall → an 8-step acceptance checklist → HA / PostgreSQL variants; §12 covers routine inspection (log-keyword → action table), backup/restore (SQLite / SQLite+HA / PG), rolling upgrades, a troubleshooting table and post-change regression; §13 is a **fixed-IP worked example** (storage server 192.168.1.10 with data in /data on the SQLite backend, one read-write client at /mnt/da and one read-only client at /mnt/db) explaining every flag hands-on; §14 answers “custom data directories, bulk import of existing data (zero-copy `grovefs import` first, streamed rsync through a mount as the alternative; hardlink aliasing, view splitting, and an explicit not-implemented list), and per-client display layouts”. The shortest production path in six steps (the service-start and mount commands live in Quick start and Production topology above — swap in the center's addresses and add `--token`; not repeated here):

1. **Build & install**: the same binary on every node — `cargo build --release && sudo install -m755 target/release/grovefs /usr/local/bin/grovefs`.
2. **Center: two services + a shared token**: `TOK=$(openssl rand -hex 16)`, both servers take `--token $TOK`; use systemd in production (unit templates in USAGE §11.4).
3. **Firewall**: open 50100/50110 to the internal subnet only (USAGE §11.6).
4. **Create the view explicitly, then mount**: a mount never provisions; the client presents the same `--token`.
5. **Accept**: A-write/B-read instantly visible across two clients + cross-host flock + `findmnt` shows grovefs:alice (USAGE §11.7).
6. **Operate**: watch `journalctl -u grove-meta-server` for degrade/freeze keywords; a cold copy of `blocks/` + the whole `meta/` (one directory per tree, plus `_control/` and `views/`) is a complete backup (USAGE §12).

## Testing

```bash
export LC_ALL=C LANG=C            # no PKG_CONFIG needed: no crate queries pkg-config (see Build)
cargo test --release
bash .build-bin/run_all.sh                 # 43 stages (42 gated + bench: build/unit + 40 suites)
SKIP_BENCH=1 bash .build-bin/run_all.sh    # skip throughput bench
```

## Command cheat sheet

> Flag names are literal — copy them as-is; § "Parameter meanings at a glance" above spells out what each knob actually points at. Commands that address the store take `--meta-dir D` on the SQLite tier and `--meta-store <URI>` on PostgreSQL.

| Goal | Command |
| --- | --- |
| Create a view | `grovefs view --meta-dir D create --view U [--manifest M]` (the ONLY entry point that builds a tree; on the PG tier swap `--meta-dir D` for `--meta-store <URI>`) |
| Mount (the only shape) | `grovefs mount --view U -m MP --manager H:P --roster-key-file F [--deployment N] [--token T] [--api-key ID:SECRET] [--ro] [--no-cache] [--threads N] [--run-as-uid N] [--layout F] [--no-readdirplus] [--stats] [--foreground\|--log-file F]` (detaches by default; coordinates come ONLY from the manager's signed roster — with neither `--manager`/`GROVEFS_MANAGER` nor `--roster-key-file` it refuses to mount (fail-closed); the hand-typed `--meta-server`/`--data-servers` are removed) |
| Mount bootstrap (one address) | `grovefs manager --state-dir D define --name N --meta M[,M2] --data A[,A2]` / `manager --state-dir D serve --listen H:P [--roster-ttl S] [--register-ttl S] [--require-registered]` / `manager status --server H:P [--deployment N] --join-key-file F`; nodes: `meta-server`/`data-server --register-with H:P --join-key-file F [--advertise-addr A] [--deployment N]`; clients: `mount --manager H:P --roster-key-file F [--deployment N]` (`--manager`/`--roster-key-file` are the ONLY source of a mount's coordinates; the old `--meta-server`/`--data-servers` are removed) |
| Layout rule file | one rule per line: `hide <src>` / `only <src>` / `map <src> <dst>` / `link <src> <dst>` (`#` comments, absolute paths) |
| Define a derived view | `grovefs view --meta-dir D define --name V --base B --rules F` (stored at `D/meta/views/V.layout`; a client gets the shape by `mount --view V`; virtual views mount read-only) |
| Define a pure alias | `grovefs view --meta-dir D alias --name A --real R` (public name 1:1 onto a physical view: `A` shows the full tree, writable; `R` stops being mountable by name) |
| Rotate a leaked public name | `grovefs view --meta-dir D alias --name A2 --real R --retire A` (write-before-remove, zero window: `A` stops resolving, `R` stays unmountable throughout, grants untouched) |
| Create a fork view | `grovefs view --meta-dir D fork --name L --base R` (copies directory entries into an independent `meta/L/L.db`, bytes shared; structure writable, content `EROFS`; `--empty` builds the root only, for curation) |
| Graft into a fork | `grovefs view --meta-dir D graft --name L [--src S] --map PLAN\|- [--dest P] [--skip-existing]` (one plan line per entry `<src>[TAB<dst>]`; the whole plan is one transaction, bytes shared; a directory source brings its subtree, the destination basename IS the rename) |
| Export/import one view's metadata | `grovefs view --meta-dir D export --view V --out F [--force]` (host-neutral JSONL, no bytes) / `grovefs view --meta-dir D import --view V --in F [--replace] [--as-fork B]` (atomic; the bytes travel as `blocks/`) |
| List keys per view | `grovefs apikey --meta-dir D list --view L` (only keys that may open `L`; a pure alias matches keys bound to the real name); search by label/owner/note/scope: `list --label SUB` / `--owner SUB` / `--note SUB` (case-insensitive substrings) / `--scope any\|mcp\|mount\|admin` (exact), all five combinable |
| List / delete view definitions | `grovefs view --meta-dir D list` (definition slots plus bare base views) / `grovefs view --meta-dir D rm --name V` (derived/alias drop only the definition; a fork also drops its own `meta/V/V.db` and rebuilds the ledger) |
| Agent interface (MCP) | `grovefs mcp --meta-dir D [--meta-store postgres://…]` (stdio JSON-RPC: governance `list_views` / `define_derived_view` / `alias_view` / `fork_view` / `graft_view` / `remove_view` / `preview_rules` / `validate_definition`, read-only introspection `list_view_tree` / `get_view`; rules travel as inline text, validation and wording identical to `grovefs view`, and the same governance is re-exposed through `resources/*` / `prompts/*` / `completion/complete` / `logging/setLevel`) |
| API keys and call audit | `grovefs apikey --meta-dir D create --label L [--for any\|mcp\|mount\|admin] [--owner ACCT] [--view V] [--ro] [--note TEXT] [--ttl DUR]` / `list [--view V] [--label S] [--owner S] [--scope S] [--note S]` / `revoke --id X` / `rotate --id X` (a fresh secret for the SAME id: binding, owner and audit history stay, the old secret is refused at the next handshake) / `set-ro --id X [--rw\|--ro]` (flip rw↔ro in place; a live mount follows within ≤1s, no remint, no reconnect) / `update --id X [--note TEXT] [--ttl DUR]` (only those two ledger fields, `--ttl never` clears the deadline; a revoked id refuses) / `secret --id X` (reads the stored reversible copy back as the full `<id>:<secret>`; only the owner or a superuser account, and through a server the read is audited) — authentication itself only ever uses the SHA-256 digest, a new key is 16 hex id + 32 hex secret, `list` shows the scope column and a head+tail fingerprint, note, deadline and minter; expiry is judged server-side at every handshake and refused like a revocation; gate the methods that touch state with `grovefs mcp --require-api-key`, or the mount door with `meta-server`/`data-server --require-api-key` (client `mount --api-key` / env `GROVEFS_API_KEY`); every call and every mount connection is audited (SQLite: `meta/_control/api_keys.db`; PG: `grovefs_control.mcp_calls`), and a revoke closes LIVE mounts; `GROVEFS_MCP_API_KEY` pre-authenticates a whole server |
| Import an existing directory tree | `grovefs import --view V --source DIR [--prefix P] [--data-dir R] [--dry-run]` (hardlinks on one filesystem, zero copy; re-running is incremental) |
| Serve the web console | `grovefs web --control-server H:P --api-key-file F [--listen 127.0.0.1:PORT] [--session-ttl N] [--allow-insecure-http\|--use-x-forwarded-for]` (an HTTP gateway in front of the governance port: overview / views / keys / accounts & grants / audit, one shared design language; it stores nothing itself) |
| Accounts and base-view grants | `grovefs web … account create --account A --password-file P\|--password-stdin [--role admin]` / `account passwd\|role\|disable\|enable\|rm` / `grant add\|rm --account A --base V` / `grant list` (a grant delegates a BASE view, so its forks, aliases, lenses and the keys bound to any of those names are inherited automatically; the first administrator is seeded by `meta-server --admin-user --admin-password-file` while the account table is empty) |
| Unmount | `grovefs umount -m MP` |
| Data service | `grovefs data-server --listen IP:PORT --data-dir DIR [--token T] [--require-api-key [--meta-dir D]]` |
| Metadata service (default SQLite) | `grovefs meta-server --meta-dir D --listen IP:PORT [--token T] [--require-api-key] [--role primary\|standby --peer ADDR [--sync-repl] [--witness DATA_ADDR]]` |
| Metadata service (PostgreSQL backend) | `grovefs meta-server --meta-store postgres://user:pw@host:5432/dbname --listen IP:PORT` (under PG `--meta-dir` is optional — the server writes no authoritative data locally; create each schema explicitly first with `grovefs view --meta-store <same URI> create --view V`; `view`/`apikey`/`mcp`/`data-server` join the same control plane through the same `--meta-store` URI or env `GROVEFS_META_STORE`) |

## Honest limitations

These are explicit, honest boundaries, not hidden assumptions:

- **Transport**: custom TCP frames today, not gRPC. gRPC (tonic/prost) was gate-evaluated as **GO** but full integration is **deliberately deferred**; the current protocol is correct and tested.
- **HA RPO**: ~500ms async by default (0 with `--sync-repl`).
- **Fencing**: `--witness` is implemented and **fail-safe**: a `data-server` list (comma-separated, odd `N`) arbitrates by **majority** (`quorum_size = N/2+1`); a primary holds a non-overlapping lease at a majority (renewed every tick), so a majority live lease is itself proof of exclusive authority and a meta write is admitted from a local lease check (one fan-out per renewal, not per write) AND gates content frames by token at the witness data-servers, bounding the double-write window to the lease safety margin (converging to one primary). The irreducible boundary: a strict two-node pair **without arbiters** cannot prevent split-brain and leaves the window unbounded (CAP/consensus). The **availability gain of a quorum**: with `--witness`, losing a *minority* of arbiters neither freezes writes nor resurrects a stale primary (a quorum still holds the monotonic token); only losing a *majority* fails safe and freezes (`N=1` reverts to a lone witness, which becomes a single availability dependency).
- **Striping**: no throughput gain on a single saturated link (loopback/one NIC); the value is aggregate capacity across nodes — real multi-machine links must be re-benchmarked before claiming gains.
- **Mount bootstrap (`grovefs manager`)**: the roster signature is an **HMAC (symmetric)** — every client that can verify can also sign, so "the client holds the roster key" is not the same statement as "the client cannot forge a roster". The payoff for forging is partly disarmed elsewhere: a fake DATA address, against a data-server with the Data-Access Capability (`--data-access-key`) enabled, buys a failed mount rather than read access, while a fake META address still harvests whatever credential the client presents at `M_OPEN`. Closing it means the asymmetric algorithm tag already reserved in the signature block (`algo=2`: a key change and one byte, not a format change). The other boundary: **a roster guarantees the member COUNT and declaration order, not that a given machine is the one that wrote the data** — replacing one data-server address with a different host that is equally inside the declared set makes striped objects read back empty (a member-count MISMATCH, by contrast, is refused explicitly with `EIO` by `backend::UnstripedGuard` on the single-address data path, not left to client discipline); swapping member hosts means swapping the whole set or migrating first. `--require-registered` is a precheck gate, not a health filter, and never shrinks a roster; multiple managers for hot redundancy are neither implemented nor tested. Rejected alternatives and the full residual-risk list live in `docs/adr/0001-manager.md`.
- **A mount has no local mode and no typed coordinates**: `mount` resolves BOTH a metadata service and a data service ONLY from one signed roster through `--manager` + `--roster-key-file` (env `GROVEFS_MANAGER` / `GROVEFS_ROSTER_KEY_FILE` satisfy the same requirement) — the hand-typed `--meta-server` / `--data-servers` flags are removed. Failing to supply the bootstrap is a hard refusal, not a fallback: one `--home` holding a view's tree AND its bytes would make every client an authoritative store, so two mounts of one view diverge and a laptop can end up holding the only copy. `--home` therefore stays the store root of the node-side verbs and of the servers, never of a mount.
- **Presentation overlay (`--layout`)**: it is a **display customization, not a security boundary** — mounting the same view without the lens still shows the full tree; use separate views + token/API key for hard isolation. Creating/renaming into a purely synthesized dir gives `EPERM`, a name colliding with a lens-shown entry gives `EEXIST`, and `only` mode refuses writes outside the whitelist; symlink target text is not rewritten; with a shared `meta-server`, invalidations for entries under synthesized dirs may miss lens coordinates (use `--no-cache`); rules relocate entries within one view only — cross-view references are not supported.
- **Server-side derived views (`grovefs view`)**: mounts of a derived name are **read-only by hard contract** — the shape is centrally governed, distributed but never modified; every write belongs to the base view. Enforcement is two-layered (kernel RO negotiated at `M_OPEN` → `EROFS` at the client; an authoritative per-op gate on the meta-server answering `E_NOTPERM`). If you need a *writable* custom shape, use the client-side `mount --layout` lens (§ Per-client presentation overlay), which writes through to the real entries. Otherwise still **not a security boundary** — mounting the base name directly still shows the full tree (token/API key gate access, not shapes); definition **edits hot-apply** (the server checks the file's mtime per op; live mounts switch shape within their ≤2s cache TTL, no remount), while `view rm` does not hot-apply (established mounts keep their shape until remount); during a shape swap, paths/fds on old virtual inodes may resolve to ENOENT — real entries keep stable inodes; no nesting (the base must be a physical view), no `name == base`, no shadowing a physical db; `meta/views/` (SQLite backend) is control-plane config that an HA standby must sync by hand (under PG the control plane sits in `grovefs_control` and replicates with the trees); a mount resolves its definition once, at `M_OPEN`.
- **Server-side pure aliases (`grovefs view alias`)**: the mirror image of derived views — an alias mount shows the full tree and is WRITABLE, while a governed real name is NOT mountable by its own name (the `meta-server` refuses it). Resolution is single-hop, re-scanned per `M_OPEN` (mounts are rare); alias chains, derived targets and one-real-name-with-two-public-aliases are rejected at the command; `import` refuses alias targets; removing an alias affects only NEW mounts (established connections keep flowing, like `view rm`); `--retire` rotates a leaked public name in one gapless step (write-before-remove so the real name is never momentarily mountable; the retired name resolves to a fresh empty namespace that never reaches the real tree) — but rotation, like `rm`, only affects NEW mounts and cannot revoke an existing token-holder, so it is leak hygiene, not a security boundary. One honest wording note: refusals travel **in the protocol** — the `M_OPEN` error frame carries the server's reason text, so a remote alias refusal prints exactly the local wording above (ditto token mismatch, missing/invalid/revoked/wrong-scope api key; against an OLD server the payload is empty and the client falls back to its generic text). And the honest boundary: name hiding is governance, not security — the public name must be distributed anyway; token + api-key admission remain the hard gates.
- **Zero-copy import (`grovefs import`)**: hard links require source and `blocks/` to share a filesystem (cross-device falls back to an independent copy, reported honestly); if an external tool later updates the source file via **rename/replace-write** (the default of vim, rsync without `--inplace`, …) the imported inode keeps the bytes from the last passthrough write — re-run `import` for new content (existing names are skipped; use a different `--prefix` to force a re-registration); symlinks/special files are deliberately skipped (not representable); imported objects are single files and do not participate in the ≥16MiB cross-server striping (striping remains a property of fresh writes through a mount); in a `meta-server` deployment import writes the authoritative DB file directly, so run it on the storage host and expect live mounts to converge by TTL (≤2s) rather than push; the PostgreSQL backend is not supported (its authoritative tree lives in Postgres — stream data in through a mounted path); keep `--source` on the store’s filesystem and **around** — deleting the source only drops that alias name (data survives in `blocks/`), but after external renames the `blocks/` link is the only remaining path to the bytes.
- **Scope of the metadata read pool and directory paging**: the read pool and the `LIMIT/OFFSET` paging both live in the **server-side storage engines** — SQLite `MetaEngine` (1 warm read connection, bursts retained up to 8), PG `PgEngine` (4 read connections, each caching its own prepared statements), the treedb engine (its own offset/limit for readdir). A mount process holds no engine, so what a client gets is the `M_READDIRP` push-down: it hands `offset/limit` to the server, the server runs that page on its own read pool, and the client takes one page per kernel buffer and negotiates `FUSE_READDIRPLUS`. Measured (loopback, same mountpoint, each pass behind a fresh mount, empty kernel dentry cache): 5001 entries `ls -l` **0.270–0.274s** (`--no-readdirplus`, paging only **0.575–0.629s**, listings byte-identical once sorted); 10,002 entries `ls -f` **0.116s**, `ls -l` **0.547s**; 100,002 entries `ls -f` **0.925s**, the same directory as a fork view **1.156s**, through a 100-rule lens **1.612s** (re-read inside the TTL **0.429s**). **Derived/lens views are inside the scope**: `LayoutMeta` overrides `readdir_page` (materializing the rendered shape per parent, then slicing), and since materialization is a cache it **must be flushed by invalidation pushes through `flush_listing`** or freshness is pushed back to the TTL order — the `disp_fresh` suite is the gate on exactly that contract. Whether a store can page is ADVERTISED by the `M_OPEN` ack capability bit; the client does not guess. Multi-authority metadata *write* parallelism does not exist: one deployment has one authority — SQLite serializes writes on a single authoritative connection, PG spreads them over a bounded SERIALIZABLE write pool while staying single-authority. The gate is `MetaProvider::supports_paged_readdir()`, pinned by the `store_level_paging_is_advertised_only_by_providers_that_page_in_sql` test; rerun `.build-bin/probe_readdir_cluster.sh`.
- **Verification environment**: everything validated on 127.0.0.1 (simulated LAN); no real multi-machine or fio-level benchmark yet.

Where numbers are not measured, no benefit is claimed; wherever there is a limitation, it is stated in code comments and here.

## License

Apache License 2.0 (see the `LICENSE` file at the repository root).

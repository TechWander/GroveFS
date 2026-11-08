# GroveFS MCP Reference (method surface / error codes / on-disk effects / CLI cross-reference)

Appendix to [SKILL.md](SKILL.md). Everything here exists in the implementation;
each item maps to `src/mcp.rs`.

## 1. Serving and handshake

```
grovefs mcp [--meta-dir <D>] [--home <H>] [--require-api-key]
```

- `--meta-dir`: the governance library root (contains `meta/` and `meta/views/`) —
  the same directory concept as `grovefs view` / `meta-server --meta-dir`; defaults
  by falling back to `--home`, then `$GROVEFS_HOME` / `~/.grovefs`.
- Every tool carries an optional `db_path` that **overrides the root per call**
  (one process serving several store roots).
- Framing: newline-delimited JSON-RPC 2.0, one frame per line, UTF-8, in on
  stdin / out on stdout.
- `initialize` `params`: `protocolVersion` (echoed when the server speaks it —
  `2025-06-18` / `2025-03-26` / `2024-11-05`; any other value is answered with
  `2025-06-18`), `capabilities`, `clientInfo`, plus the optional
  **`apiKey: "<ID>:<SECRET>"`** (the only open method that can carry a credential).
- A successful handshake answers:

```json
{"protocolVersion":"2025-06-18",
 "capabilities":{"tools":{"listChanged":false},"resources":{"subscribe":false,"listChanged":false},
                 "prompts":{"listChanged":false},"completions":{},"logging":{}},
 "serverInfo":{"name":"grovefs","title":"GroveFS view governance","version":"<crate version>"},
 "instructions":"…"}
```

`instructions` is the server's own operating manual, sectioned for skimming
(`## Discover` / `## Reshape` / `## Curate` / `## Validate before publishing` /
`## Golden order` / `## Beyond tools` / `## Mechanics`). Clients inject it into
context, so **this skill deliberately does not repeat its wording** — it only adds
implementation details and pitfalls. Each tool's own `description` is likewise
self-explanatory; `grovefs://guide` serves the same manual as a resource.

- The client then sends `notifications/initialized` (**no id, never answered**).

## 2. Method surface and the authentication gate

| Method | Under `--require-api-key` | Notes |
|---|---|---|
| `initialize` | open | the only method that can carry `apiKey`; success stamps the session's identity (later audit attribution) |
| `ping` | open | answers `{}` |
| `notifications/*`, `initialized` | open | received silently, **never answered** (don't wait for a reply) |
| `tools/list` | **gated** | 10 tools plus self-describing schemas, `title` + `annotations` (`readOnlyHint` on the five read-only tools), and an `outputSchema` on the four JSON-returning ones |
| `tools/call` | **gated** | governance failures stay inside the result (`isError`), never the JSON-RPC error layer; the JSON tools also return `structuredContent`; `arguments` may be an object or a string holding one |
| `resources/list` | **gated** | enumerates slot by slot (physical→tree; derived/alias/fork→definition+tree); `-32603` if the store cannot be read |
| `resources/templates/list` | **gated** | two templates (static) |
| `resources/read` | **gated** | malformed URI / unknown view → `-32002` (a bad URI is a protocol problem, not a governance refusal) |
| `completion/complete` | **gated** | role-aware: a prompt's `base`/`real` completes against physical trees, `rotate-alias`' `new`/`old` against aliases, a resource's `{name}` against every name; non-view-name arguments answer an empty list |
| `prompts/list` | open | two playbooks |
| `prompts/get` | open | missing/unknown prompt → `-32602` |
| `logging/setLevel` | open | only tunes this session's verbosity; never behind the gate |
| anything else | — | with an id → `-32601 method not found: <m>`; an unknown notification without an id stays silent |

Unauthenticated access to a gated method → `-32001` with
`authentication required: present a valid apiKey in the initialize params (or export GROVEFS_MCP_API_KEY for the server)`;
the audit log also records a failure per the audit rules
(`unauthorized: no valid api key`).

Credential **scope** (`--for mcp` / `any`) is what gates this plane. Two other
key attributes are enforced only on the mount plane and are NOT consulted by
`grovefs mcp`:

- `--ro` / `apikey set-ro`: an `--ro` key with scope `mcp` still calls
  `define_derived_view` / `alias_view` / `fork_view` / `graft_view` / `remove_view`
  successfully — verified. There is no read-only MCP credential.
- `--view V` binding: the MCP server never reads it. Bind a mount credential
  to a view; do not expect the same key to lock an agent to a view through
  this entrance.

Re-authentication is a one-shot: `initialize` stamps the session, and later
calls reuse it (unlike the FUSE path, which repolls the key store ~1 s). A
revocation therefore takes effect on the *next* `initialize`, not mid-session.

## 3. Error codes

| Code | Trigger | Typical text |
|---|---|---|
| `-32700` | malformed JSON frame | `parse error: …` (**the server keeps serving**; the stream is not dropped) |
| `-32601` | unknown method with an id | `method not found: foo/bar` |
| `-32602` | argument problems: `resources/read` without `uri`, `prompts/get` missing/wrong arguments, internal error in `completion/complete` | `unknown prompt '<x>'` / `rotate-alias needs arguments 'real', 'new' and 'old'` |
| `-32603` | internal error: `resources/list` cannot read the store | — |
| `-32001` | authentication gate | see above |
| `-32002` | resource URI unavailable | `unsupported resource uri '<uri>' (expected grovefs://view/<name>/(definition\|tree))` / `resource uri '<uri>' names no view` |
| (no code) | governance refusals, missing tool arguments, unknown tools | `isError:true` inside the result + the CLI's verbatim wording; a missing argument appends `(tool '<name>' requires: …)` and an unknown tool lists the surface plus a nearest-name suggestion |

## 4. Resources (byte-identical to tool results)

Templates:

```json
[{"uriTemplate":"grovefs://view/{name}/definition","name":"view-definition","mimeType":"application/json"},
 {"uriTemplate":"grovefs://view/{name}/tree{?path,depth,max}","name":"view-tree","mimeType":"application/json"}]
```

`resources/list` also enumerates two store-wide resources (first, so they are
the easiest to attach), plus every slot:

- `grovefs://guide` (`text/markdown`): the operating manual — the same text as
  `initialize.instructions`, plus the resource list.
- `grovefs://views` (`application/json`): the whole-store snapshot, byte-identical
  to `list_views`.
- `grovefs://view/<name>/definition`: the slot's **verbatim governance text**
  (raw derived rules, or the `base <x>` line of an alias/fork); a physical view
  answers its mount semantics. Equivalent to `get_view`.
- `grovefs://view/<name>/tree{?path,depth,max}`: the listing a mount actually
  sees (rendered through the real lens, synthetic directories included).
  Equivalent to `list_view_tree`, and the query parameters are that tool's
  arguments: `path` (display subtree), `depth` (default 6), `max` (default 800).
  A malformed number is refused (`'depth' must be a non-negative integer`) rather
  than silently ignored, and a bounded render still says `truncated: true`.

In both (resources and tools) `entries[].size` is a **metadata field, not a byte
count**: dirs `4096`, files `0`, symlinks their target's character length — the
walk reads metadata only and never asks the data plane for object lengths (the
tool description and `instructions` both say so).

The `resources/read` envelope is `{"contents":[{"uri","mimeType","text"}]}`; the
byte-identical JSON string lives inside `text` (so a client must peel
`contents[0].text` first).

## 5. Prompts (playbooks, instantiated into one user message)

| Prompt | Arguments | Content |
|---|---|---|
| `reshape-view` | ★`base`, `goal` (optional) | five steps "read real paths → draft rules → `validate_definition` dry run → `define_derived_view` publish → `list_view_tree` verify"; control plane throughout, no mounts |
| `rotate-alias` | ★`real` ★`new` ★`old` | one `alias_view {name:new, real, retire:old}` writes before removing, so the public name never has a gap; then `list_views` to confirm; old connections drain on their own |

## 6. Completion and logging

- `completion/complete`: `{"ref":{"type":"ref/prompt","name":"reshape-view"},
  "argument":{"name":"base","value":"re"}}` → view names from the served
  store matching the prefix (case-insensitive), as
  `{"completion":{"values":[…],"total":N,"hasMore":bool}}` (at most 100).
  The `ref` is honored: a prompt's `base`/`real` suggests physical trees only,
  `rotate-alias`' `new`/`old` suggests existing aliases only, and a resource URI's
  `{name}` suggests every mountable name. Without a recognized `ref` all names
  are suggested. It queries the store's **own backend** — local SQLite files, or
  PostgreSQL (`grovefs_control` + `grovefs_v_*`) when the server runs with
  `--meta-store postgres://…`; never a remote `meta-server`.
- `logging/setLevel`: `{"level":"warning"}`. Once a threshold is set, the server
  appends `notifications/message` frames (logger `"grove-mcp"`) **after the
  answered call**, but **only at or above the threshold**: success is `info` +
  `<method> <tool-or-uri> ok`, any refusal or failure is `warning` +
  `<method> <tool-or-uri> failed: <reason>` (e.g. `tools/call list_view_tree
  failed: no metadata tree for view 'ghost' …`) — with the threshold at
  `warning`, a successful call pushes nothing at all.
  Logged calls: `tools/call`, `tools/list`, `resources/list`, `resources/read`,
  `prompts/get`, `completion/complete`; `initialize`/`ping`/`prompts/list`/
  templates stay quiet. Notification frames are written **after** the response,
  so stdout remains a valid frame stream.
  Severity ordering: `debug 10 < info 20 < notice 30 < warning 40 < error 50 <
  critical 60 < alert 70 < emergency 80`; unknown names count as `info`.

## 7. What each writing tool leaves behind (usable for out-of-band verification)

SQLite backend (default) — effects on disk:

| Tool | On-disk effect |
|---|---|
| `define_derived_view` | overwrites `<db>/meta/views/<name>.layout` (first line `base <base>` + rule lines) |
| `alias_view` | writes `<db>/meta/views/<name>.layout` (`alias <real>`); with `retire`, **writes the new one before deleting the old** |
| `fork_view` | new tree `<db>/meta/<name>.db` (plus `-wal`/`-shm`) + `<db>/meta/views/<name>.layout`, and updates the cross-view ledger `refs.db` (with `seed:false` the tree is root-only) |
| `graft_view` | inserts rows into the DESTINATION's `<db>/meta/<name>.db` (ids/inos allocated above both existing id and ino spaces, hard links preserved), creates missing destination directories (0755), and books one ledger hold per new blob — one IMMEDIATE transaction, so a refused plan writes nothing. The destination may be a fork **or a bare base view**; only a derived lens or an alias is refused by role |
| `remove_view` | deletes the `.layout`; if the slot was a fork, also deletes its `meta/<name>.db*` and runs `rebuild_ledger` |
| `preview_rules` / `validate_definition` / the three read-only probes | **write nothing** |

Under `--meta-store postgres://…` nothing control-plane lands on disk: the
`.layout` writes above become rows in `grovefs_control.view_defs`
(verify with `SELECT name, definition FROM grovefs_control.view_defs`), and
`fork_view` and `graft_view` are refused (`fork is not implemented on the
PostgreSQL backend yet …` / `graft is not implemented on the PostgreSQL
backend yet …`).

A read-only probe never mints anything on PG either: `list_view_tree` on a
view whose `grovefs_v_<hex>` schema does not exist is refused with the same
`no metadata tree for view 'X' (… schema grovefs_v_…) — list_view_tree walks
this store's own backend; …` wording, and `information_schema.schemata` gains
no row. (Regression locked by `tests/mcp_probe_ro.rs`: an earlier build ran
`ensure_schema()` inside the probe and silently created a phantom schema that
then showed up in the next `list_views` as a physical view.)

## 8. CLI cross-reference (same library functions, same refusal texts)

| MCP tool | CLI equivalent |
|---|---|
| `list_views` | `grovefs view list` |
| `define_derived_view` | `grovefs view define --name N --base B --rules F` (rules come from a **file**, not inline text) |
| `alias_view` | `grovefs view alias --name N --real R [--retire OLD]` |
| `fork_view` | `grovefs view fork --name N --base B [--empty]` — `--empty` is the CLI spelling of `seed:false` |
| `graft_view` | `grovefs view graft --name N --map PLAN\|- [--src S] [--dest P] [--skip-existing]` — a plan file, or `-` to read the plan from stdin |
| `remove_view` | `grovefs view rm --name N` |

**Graft plan format** (one entry per line; verified against the current build):

| Line | Where it lands |
|---|---|
| `<src>` (no TAB) | the **same full relative path** in the destination — `src/docs/a.md` rebuilds `src/docs/` there, it does **not** become `/a.md` |
| `<src>TAB<dst>` | exactly `dst`; the TAB is the **only** rename mechanism |
| `--dest D` (CLI) / `dest` (tool) | prefixes **every** line, TAB lines included (`<src>TAB<dst>` → `D/<dst>`) |
| `skip_existing` | existing names are skipped in place; without it any conflict refuses the **whole** plan |
| `# …` | comment **only** when `#` is the first character of the line — a trailing `#` becomes part of the name |

A directory source brings its whole subtree; missing destination parents are created (0755) and counted in the reply's `entries` (so a 4-line plan can legitimately report `13 entries (8 directories)`). `src` defaults to the fork's recorded base but is **mandatory** when the destination is a base view.

Every row in this table is byte-identical between the two entrances (same
refusal text, same on-disk effect): the CLI is a thin parser over the same
`grovefs::mcp` functions, so neither entrance can do anything the other
cannot, and neither bypasses a guard.

**Four interface-only calls** (no same-named CLI subcommand; they are read-only
probe / dry-run library functions in `grovefs::mcp`):
`get_view`, `list_view_tree`, `preview_rules`, `validate_definition`. The
approximate CLI equivalents: read `<db>/meta/views/<name>.layout` directly (or
the `grovefs_control.view_defs` row under `--meta-store`); temporarily `mount
--layout FILE` to see a lens's shape; illegal rules fail fast
at `view define` — whereas the interface reaches the same verdict **without
touching disk**.

**CLI-only, not in MCP** (don't look for them here): `init` / `view create`
(building physical bases), `mount` / `umount`, `import` (zero-copy registration
of existing data), `view export` / `view import` (cross-machine namespace
transfer for one view), `apikey *` (minting keys, scopes, audit read-back,
revocation), `meta-server` / `data-server`, striping / HA / GC.
The interface = the same governance as `grovefs view`; **an agent can do nothing
the CLI cannot, and bypass no guard**.

> Naming caution: the CLI also has `grovefs view import` (importing one view's
> **metadata namespace** from JSONL into this store root, for cross-machine
> moves); it shares a name with the data-plane
> `grovefs import --view V --source DIR` (zero-copy registration of existing
> bytes) but means something else. Neither is in the MCP tool surface.

## 9. Common failures → what to do

| Symptom | Cause | Remedy |
|---|---|---|
| `-32001` right at startup | server ran with `--require-api-key`, client sent no `apiKey` in `initialize` | mint a key with `grovefs apikey create --label agent --for mcp` (plaintext printed once), put it in `initialize.params.apiKey` or export `GROVEFS_MCP_API_KEY` on the server |
| Auth fails: `invalid api key id` / `invalid api key secret` / `has been revoked` | plaintext transcribed wrong, revoked, or id:secret pasted incorrectly | match the id via `grovefs apikey list`; re-mint if needed |
| Auth fails though the key is valid | scope does not include the MCP plane | re-mint with `--for mcp` (or `--for any`); a `--for mount` key cannot open the governance interface |
| `no metadata tree for view 'X' … list_view_tree walks this store's own backend; a remote tree not reachable from here is not` | the view has no tree in the backend the MCP server is bound to (no local `meta/X.db`, and no `--meta-store` was given for the PG schema) | run the server against the right backend (`--meta-dir` on the store machine, or `--meta-store postgres://…`), or keep a local metadata copy (`view import`) |
| `no physical base tree … PostgreSQL-backed view cannot be forked` | fork needs a local SQLite bare base | first `grovefs --home H init/view create --view B`, or use a derived view instead |
| Wrote the `.layout` but the mount still shows the old shape | a bare local mount resolves its definition once at mount time | remount; mounts through `meta-server` reshape themselves within ≤2s |
| `list_view_tree` answers `truncated:true` | hit the `depth`(6)/`max`(800) bounds | drill down with `path` in segments, or raise `depth`/`max` |
| Hangs waiting for a reply after sending a notification | id-less frames are never answered by design | send notifications without waiting; only await replies for requests with an id |

# GroveFS MCP Reference (method surface / error codes / on-disk effects / CLI cross-reference)

Appendix to [SKILL.md](SKILL.md). Everything here exists in the implementation;
each item maps to `src/mcp.rs`.

## 1. Serving and handshake

```
grovefs mcp [--db-path <D>] [--home <H>] [--require-api-key]
```

- `--db-path`: the governance library root (contains `meta/` and `meta/views/`) —
  the same directory concept as `grovefs view` / `meta-server --db-path`; defaults
  by falling back to `--home`, then `$GROVEFS_HOME` / `~/.grovefs`.
- Every tool carries an optional `db_path` that **overrides the root per call**
  (one process serving several store roots).
- Framing: newline-delimited JSON-RPC 2.0, one frame per line, UTF-8, in on
  stdin / out on stdout.
- `initialize` `params`: `protocolVersion` (echoed back verbatim, not validated),
  `capabilities`, `clientInfo`, plus the optional
  **`apiKey: "<ID>:<SECRET>"`** (the only open method that can carry a credential).
- A successful handshake answers:

```json
{"protocolVersion":"2025-06-18",
 "capabilities":{"tools":{"listChanged":false},"resources":{"subscribe":false,"listChanged":false},
                 "prompts":{"listChanged":false},"completions":{},"logging":{}},
 "serverInfo":{"name":"grovefs","title":"GroveFS view governance","version":"<crate version>"},
 "instructions":"…"}
```

`instructions` is the server's own operating manual (four paragraphs on
discover / preview / reshape / dry-run, plus the authentication hint). Clients
inject it into context, so **this skill deliberately does not repeat its
wording** — it only adds implementation details and pitfalls. Each tool's own
`description` is likewise self-explanatory.

- The client then sends `notifications/initialized` (**no id, never answered**).

## 2. Method surface and the authentication gate

| Method | Under `--require-api-key` | Notes |
|---|---|---|
| `initialize` | open | the only method that can carry `apiKey`; success stamps the session's identity (later audit attribution) |
| `ping` | open | answers `{}` |
| `notifications/*`, `initialized` | open | received silently, **never answered** (don't wait for a reply) |
| `tools/list` | **gated** | 9 tools plus self-describing schemas |
| `tools/call` | **gated** | governance failures stay inside the result (`isError`), never the JSON-RPC error layer |
| `resources/list` | **gated** | enumerates slot by slot (physical→tree; derived/alias/fork→definition+tree); `-32603` if the store cannot be read |
| `resources/templates/list` | **gated** | two templates (static) |
| `resources/read` | **gated** | malformed URI / unknown view → `-32002` (a bad URI is a protocol problem, not a governance refusal) |
| `completion/complete` | **gated** | uses `params.argument` only (`ref` is ignored); non-view-name arguments answer an empty list |
| `prompts/list` | open | two playbooks |
| `prompts/get` | open | missing/unknown prompt → `-32602` |
| `logging/setLevel` | open | only tunes this session's verbosity; never behind the gate |
| anything else | — | with an id → `-32601 method not found: <m>`; an unknown notification without an id stays silent |

Unauthenticated access to a gated method → `-32001` with
`authentication required: present a valid apiKey in the initialize params (or export GROVEFS_MCP_API_KEY for the server)`;
the audit log also records a failure per the audit rules
(`unauthorized: no valid api key`).

## 3. Error codes

| Code | Trigger | Typical text |
|---|---|---|
| `-32700` | malformed JSON frame | `parse error: …` (**the server keeps serving**; the stream is not dropped) |
| `-32601` | unknown method with an id | `method not found: foo/bar` |
| `-32602` | argument problems: `resources/read` without `uri`, `prompts/get` missing/wrong arguments, internal error in `completion/complete` | `unknown prompt '<x>'` / `rotate-alias needs arguments 'real', 'new' and 'old'` |
| `-32603` | internal error: `resources/list` cannot read the store | — |
| `-32001` | authentication gate | see above |
| `-32002` | resource URI unavailable | `unsupported resource uri '<uri>' (expected grovefs://view/<name>/(definition\|tree))` / `resource uri '<uri>' names no view` |
| (no code) | governance refusals, missing tool arguments, unknown tools | `isError:true` inside the result + the CLI's verbatim wording |

## 4. Resources (byte-identical to tool results)

Templates:

```json
[{"uriTemplate":"grovefs://view/{name}/definition","name":"view-definition","mimeType":"application/json"},
 {"uriTemplate":"grovefs://view/{name}/tree","name":"view-tree","mimeType":"application/json"}]
```

- `grovefs://view/<name>/definition`: the slot's **verbatim governance text**
  (raw derived rules, or the `base <x>` line of an alias/fork); a physical view
  answers its mount semantics. Equivalent to `get_view`.
- `grovefs://view/<name>/tree`: the listing a mount actually sees (rendered
  through the real lens, synthetic directories included). Equivalent to
  `list_view_tree {"view":"<name>"}`.

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

- `completion/complete`: `{"argument":{"name":"base","value":"re"}}` → view names
  from the local store matching the prefix (case-insensitive), as
  `{"completion":{"values":[…],"total":N,"hasMore":bool}}` (at most 100). Works only
  for `name`/`base`/`view`/`real`; **queries local SQLite only**, never
  remote/PG.
- `logging/setLevel`: `{"level":"warning"}`. Once a threshold is set, the server
  appends `notifications/message` frames (logger `"grove-mcp"`) **after the
  answered call**, but **only at or above the threshold**: success is `info` +
  `<method> ok`, any refusal or failure is `warning` + `<method> failed: <reason>`
  — with the threshold at `warning`, a successful call pushes nothing at all.
  Logged calls: `tools/call`, `tools/list`, `resources/list`, `resources/read`,
  `prompts/get`, `completion/complete`; `initialize`/`ping`/`prompts/list`/
  templates stay quiet. Notification frames are written **after** the response,
  so stdout remains a valid frame stream.
  Severity ordering: `debug 10 < info 20 < notice 30 < warning 40 < error 50 <
  critical 60 < alert 70 < emergency 80`; unknown names count as `info`.

## 7. What each writing tool leaves on disk (usable for out-of-band verification)

| Tool | On-disk effect |
|---|---|
| `define_derived_view` | overwrites `<db>/meta/views/<name>.layout` (first line `base <base>` + rule lines) |
| `alias_view` | writes `<db>/meta/views/<name>.layout` (`base <real>`); with `retire`, **writes the new one before deleting the old** |
| `fork_view` | new tree `<db>/meta/<name>.db` (plus `-wal`/`-shm`) + `<db>/meta/views/<name>.layout`, and updates the cross-view ledger `refs.db` |
| `remove_view` | deletes the `.layout`; if the slot was a fork, also deletes its `meta/<name>.db*` and runs `rebuild_ledger` |
| `preview_rules` / `validate_definition` / the three read-only probes | **write nothing** |

## 8. CLI cross-reference (same library functions, same refusal texts)

| MCP tool | CLI equivalent |
|---|---|
| `list_views` | `grovefs view list` |
| `define_derived_view` | `grovefs view define --name N --base B --rules F` (rules come from a **file**, not inline text) |
| `alias_view` | `grovefs view alias --name N --real R [--retire OLD]` |
| `fork_view` | `grovefs view fork --name N --base B` |
| `remove_view` | `grovefs view rm --name N` |

**Four interface-only calls** (no same-named CLI subcommand; they are read-only
probe / dry-run library functions in `grovefs::mcp`):
`get_view`, `list_view_tree`, `preview_rules`, `validate_definition`. The
approximate CLI equivalents: read `<db>/meta/views/<name>.layout` directly;
temporarily `mount --layout FILE` to see a lens's shape; illegal rules fail fast
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
| `no local metadata tree for view 'X' … LOCAL SQLite store only` | `list_view_tree` only walks local `meta/X.db`; remote/PG-backed trees are not walked | use the CLI on the store machine, or keep a local metadata copy (`view import`) |
| `no physical base tree … PostgreSQL-backed view cannot be forked` | fork needs a local SQLite bare base | first `grovefs --home H init/view create --view B`, or use a derived view instead |
| Wrote the `.layout` but the mount still shows the old shape | a bare local mount resolves its definition once at mount time | remount; mounts through `meta-server` reshape themselves within ≤2s |
| `list_view_tree` answers `truncated:true` | hit the `depth`(6)/`max`(800) bounds | drill down with `path` in segments, or raise `depth`/`max` |
| Hangs waiting for a reply after sending a notification | id-less frames are never answered by design | send notifications without waiting; only await replies for requests with an id |

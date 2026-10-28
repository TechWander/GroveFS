---
name: grovefs-mcp
description: Drive GroveFS view governance through the `grovefs mcp` stdio JSON-RPC server (9 tools: list_views, get_view, list_view_tree, define_derived_view, alias_view, fork_view, remove_view, preview_rules, validate_definition). Use when a task mentions grovefs, views, derived lenses, aliases, forks, mount --view, layout rules (hide/only/map/link), reshaping a data listing, or API-key gating of the GroveFS MCP entrance. Covers handshake, framing, exact refusal texts, verification loops, and hard boundaries (control plane only — never mounts, never reads data bytes).
---

# GroveFS View Governance (MCP Entrance)

Lets an agent **reshape data listings through an API**: discover which views a
store has and what their trees actually look like, then publish / rotate /
remove view definitions (read-only lenses, writable public aliases, standalone
forks).

**This entrance is the control plane**: it only writes
`<db-path>/meta/views/*.layout` (a fork additionally writes `meta/<name>.db`
and rebuilds the `refs.db` ledger). It **never mounts and never reads object
bytes**. Mounting and data I/O belong to the CLI/ops plane, not here.

## When to use this skill

- The task mentions grovefs / a view / a derived view / an alias / a fork /
  `mount --view` / layout rules.
- You need to "turn one data listing into another shape" without touching the
  files themselves.
- You need to know whether a definition will be accepted or refused before
  publishing it (dry run).
- You need to gate the interface per caller (API key + audit).

Not applicable: moving bytes (`rsync -a -H blocks/`), moving a namespace across
machines (`grovefs view export/import` — **CLI-only, no MCP tool for this**),
reading or writing file contents (mount the view instead).

## Step one: bring the server up

The server is spawned as a child process by the MCP client; frames go in on
stdin and come out on stdout:

```bash
grovefs mcp --db-path <DB_PATH>          # metadata root (contains meta/ and meta/views/); falls back to --home
grovefs mcp --db-path <DB_PATH> --require-api-key   # add the gate for multiple callers (see "Authentication")
```

MCP client configuration (the generic `mcpServers` shape; Claude Desktop and
IDE agents work the same way):

```json
{ "mcpServers": { "grovefs": {
  "command": "grovefs",
  "args": ["mcp", "--db-path", "/abs/path/to/store"],
  "env": { "GROVEFS_MCP_API_KEY": "ID:SECRET" }
} } }
```

`env` is only needed when the server was started with `--require-api-key`;
exporting it pre-authenticates the whole server.

**When your client does not manage the process for you**, send frames yourself
with the dependency-free script in this directory (see `scripts/mcp_call.py`):

```bash
python3 scripts/mcp_call.py --db-path <D> list                    # tools/list
python3 scripts/mcp_call.py --db-path <D> call list_views '{}'
python3 scripts/mcp_call.py --db-path <D> call define_derived_view '{"name":"release","base":"team","rules":"hide /photos/private"}'
python3 scripts/mcp_call.py --db-path <D> --api-key ID:SECRET list # through a --require-api-key gate
```

Check the wiring with a self-test first (it uses a throwaway store root and
never touches real data):

```bash
bash scripts/smoke.sh
```

## Protocol essentials (verbatim with the implementation)

- Newline-delimited JSON-RPC 2.0, protocol version `2025-06-18`; `initialize`
  echoes the client's `protocolVersion` back verbatim without validating it.
- **Governance failures are not protocol errors**: per MCP convention they come
  back inside the result as
  `{"result":{"content":[{"type":"text","text":"…"}],"isError":true}}`, so an
  agent client shows the wording instead of crashing. The JSON-RPC layer is
  reserved for protocol problems: malformed frame `-32700` (the server keeps
  serving), unknown resource URI `-32002`, authentication failure `-32001`.
- **Frames without an id (notifications) are never answered** — do not wait for
  a reply after sending one (`notifications/initialized`,
  `notifications/cancelled`).
- Every tool accepts an optional `db_path` that overrides the server default,
  so one process can serve several store roots.
- Missing arguments and unknown tools surface as `isError`, not as protocol
  errors.

## The nine tools

| Tool | Arguments (★ required) | Purpose |
|---|---|---|
| `list_views` | `db_path` | Whole-store snapshot: physical views plus every definition slot (kind / real_view / rule counts / mount semantics) |
| `get_view` | ★`name` | Full record of one view, including the **verbatim `definition` rules** — read it before editing rules |
| `list_view_tree` | `view` \| ★(`base`+`rules`), plus `path`/`depth`(6)/`max`(800) | Renders the listing a mount would actually show; can also preview an unpublished rule set **without touching disk**. `size` is a metadata field, not a byte count (dirs 4096 / files 0 / symlinks report the target length) |
| `define_derived_view` | ★`name` ★`base` ★`rules` | Publish/replace a **read-only** derived lens |
| `alias_view` | ★`name` ★`real` \| `retire` | Publish a writable public alias; `retire` rotates names gaplessly (write new, then remove old) |
| `fork_view` | ★`name` ★`base` | Seed an **independent metadata tree** (structure writable, content `EROFS`, bytes shared) |
| `remove_view` | ★`name` | Delete a definition slot (derived/alias/shared); deleting a fork drops its own tree too |
| `preview_rules` | ★`rules` | Validates verb syntax only; errors carry line numbers |
| `validate_definition` | ★`name` ★`base` ★`rules` | Runs a publish through the **entire governance guard set** before anything is written, returning the exact refusal text |

Neither read-only probe touches data bytes: `list_view_tree` opens the local
`<db-path>/meta/<view>.db` read-only and renders it through the same `LayoutMeta`
lens a mount uses; `get_view` only reads the definition file's text.
**Remote / PostgreSQL-backed trees are not walked** (`list_view_tree` needs a
local `meta/<view>.db`).
Because the walk only reads metadata and never asks the data plane for object
lengths, the `size` in a response is **not a file size** (dirs `4096`, files `0`,
symlinks report their target's character length); mount and `stat` for real bytes.

## Rule text (four verbs)

One verb per line; `#` starts a comment line:

```
hide <src>          # prune a subtree
only <src>          # serve only this subtree
map  <src> <dst>    # reattach entries under a virtual display path (dst parents are synthesized)
link <src> <dst>    # give the same entries an extra name (multi-view)
```

- `<src>`/`<dst>` are **absolute display paths** (starting with `/`) and **must**
  be taken from a `list_view_tree` response. Never guess them.
- A `base <x>` line inside the rules is refused: the base view is the `base`
  argument, not rule content.

## Golden workflow (follow this order when reshaping a view)

Copy this checklist and work down it:

```
- [ ] 1. list_views                       → which physical views/definitions exist; don't collide with a live name
- [ ] 2. get_view (if editing an existing lens) → grab the verbatim definition for read-modify-write
- [ ] 3. list_view_tree {"view":"<base>"} → real display paths as rule material
- [ ] 4. draft the rules text
- [ ] 5. preview_rules {"rules":"<draft>"} → syntax/line numbers (fast, parser only)
- [ ] 6. validate_definition {name,base,rules} → full-guard dry run, get the verdict before writing
- [ ] 7. define_derived_view {name,base,rules} → publish
- [ ] 8. list_view_tree {"view":"<new name>"} → confirm the rendered shape matches intent
```

Step 6 must never be skipped: it returns the exact refusal text a real publish
would produce, and **writes nothing**. Step 8 closes the feedback loop — the work
is only done when the shape it displays matches intent; otherwise go back to
step 4.

Derived views always mount read-only (write through to the base view). A `map`
`<dst>`'s parent directories are synthesized on demand (scaffolding), but you
**cannot create or rename entries inside a purely synthetic directory**
(`EPERM`): the directory only becomes real once something is `map`ped into it.

## What gets refused (exact texts, so you self-check instead of retry-guessing)

| What you tried | The `isError` text you get back |
|---|---|
| Derived name == base name | `derived view name must differ from its base` |
| Definition name collides with a physical db name | `'<name>' already names a physical view db — pick another name` |
| Defining over an alias / fork name | `'<name>' is a pure alias — remove it first …` / `… is a fork …` |
| Alias pointing at an alias | `alias target '<real>' is itself an alias (-> '<x>') — chains are not supported` |
| Alias pointing at a derived view | `alias target '<real>' is a derived (virtual) view — aliases map onto a physical view only` |
| A second public name for the same real view | `real view '<real>' is already governed by alias '<old>' — one public name per real view; to rotate it gaplessly use … --retire` |
| `retire` removing an alias of a different view | `--retire '<old>' currently aliases a different real view ('<r>'), not '<real>'` |
| Forking a base that is not a local SQLite bare base | `no physical base tree … \`fork\` copies a LOCAL SQLite tree … A PostgreSQL-backed view cannot be forked` |
| Forking a base that is itself governed | `base '<base>' is governed by a definition (alias/derived/fork) — fork the physical base tree instead` |
| Fork name == base name | `fork name must differ from its base` |
| Forking a name that already has a definition | `view '<name>' already has a definition — remove it first (\`grovefs view rm --name <name>\`) before forking` |
| Deleting a physical base view name | `no view definition '<name>' under <db>/meta/views` (`remove_view` only manages definition slots) |
| Name is not a safe path element | `invalid view name '<name>' (must be one safe path element)` |

## Authentication and audit (when fronting the interface to many callers)

- `grovefs mcp --require-api-key` gates **every method that reads or writes
  governance state**: `tools/list`, `tools/call`, `resources/list`,
  `resources/templates/list`, `resources/read`, `completion/complete`;
  `initialize` / `ping` / `prompts/*` / `logging/*` **stay open** (so a client can
  handshake, fetch the static playbooks, and control verbosity).
- Credentials: `grovefs apikey --db-path <D> create --label L --for mcp` (the MCP
  plane requires scope `mcp` or `any`; `--view`/`--ro` are **mount-plane**
  dimensions). The plaintext is printed once; only the SHA-256 is stored.
- Presentation: `params.apiKey = "<ID>:<SECRET>"` in `initialize`, or export
  `GROVEFS_MCP_API_KEY` on the server side.
- Unauthenticated access to a gated method → JSON-RPC `-32001` with
  `authentication required: present a valid apiKey in the initialize params (or export GROVEFS_MCP_API_KEY for the server)`.
- Every `tools/call` and `resources/read` (including failures, including
  refused-unauthenticated) lands in the audit table: read it back with
  `grovefs apikey calls`; after `apikey revoke` that key fails auth immediately.

## Hard boundaries (do not try to work around them, and do not promise otherwise to the user)

- **Display governance is not a security boundary**: derived/alias/fork only
  reshape the name tree that is presented. What is visible is readable, and
  readable is writable back (a fork excepted). The real hard boundaries are
  tokens and revocable API keys (plus OS-level permissions on the store root).
- The default `grovefs mcp` (no `--require-api-key`) **trusts whoever can spawn
  the process** — equivalent to holding OS access to the store root.
- A fork's read-only content is a **client-side gate**: a client speaking the
  wire protocol straight to the `data-server` bypasses it.
- This interface never mounts, never reads data bytes, never creates physical
  views, and does not do `view export/import` (those are CLI).
- Hot-reload of an edited definition **only exists at the `meta-server`
  entrance**: live mounts reshape themselves within the cache TTL (≤2s) without
  remounting; a bare local mount (`--home` direct) resolves its definition once
  at mount time, so edits need a remount to show.
- `list_view_tree` walks are bounded: `depth` defaults to 6, `max` to 800, and a
  truncated walk honestly returns `truncated:true` — for big trees, drill down
  with `path` in segments; don't expect one call to return everything, and never
  treat a truncated listing as the whole tree.

## More material

- Full method surface (resource URIs, prompt playbooks, completion, logging, error codes, CLI cross-reference): [reference.md](reference.md)
- Real transcripts and end-to-end dialogue samples: [examples.md](examples.md)
- Dependency-free stdio client: `scripts/mcp_call.py`
- Wiring self-test (throwaway store root, whole workflow): `scripts/smoke.sh`

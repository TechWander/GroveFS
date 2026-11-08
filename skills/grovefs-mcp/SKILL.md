---
name: grovefs-mcp
description: Drive GroveFS view governance through the `grovefs mcp` stdio JSON-RPC server (10 tools: list_views, get_view, list_view_tree, define_derived_view, alias_view, fork_view, graft_view, remove_view, preview_rules, validate_definition). Use when a task mentions grovefs, views, derived lenses, aliases, forks, mount --view, layout rules (hide/only/map/link), reshaping a data listing, or API-key gating of the GroveFS MCP entrance. Covers handshake, framing, exact refusal texts, verification loops, and hard boundaries (control plane only — never mounts, never reads data bytes).
---

# GroveFS View Governance (MCP Entrance)

Lets an agent **reshape data listings through an API**: discover which views a
store has and what their trees actually look like, then publish / rotate /
remove view definitions (read-only lenses, writable public aliases, standalone
forks).

**This entrance is the control plane**, and its home follows the metadata
backend: on embedded SQLite it writes `<meta-dir>/meta/views/*.layout` (a fork
additionally writes `meta/<name>.db` and rebuilds the `refs.db` ledger); when
the server runs with `--meta-store postgres://…` definitions write to the PG table
`grovefs_control.view_defs` instead and nothing control-plane lands on local
disk (fork is not implemented on PG — it refuses). It **never mounts and never
reads object bytes**. Mounting and data I/O belong to the CLI/ops plane, not
here.

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
grovefs mcp --meta-dir <DB_PATH>          # metadata root (contains meta/ and meta/views/); falls back to --home
grovefs mcp --meta-dir <DB_PATH> --require-api-key   # add the gate for multiple callers (see "Authentication")
grovefs mcp --meta-dir <DB_PATH> --meta-store postgres://user:pw@host:5432/db  # PG control plane (definitions/keys/audit in grovefs_control; env GROVEFS_META_STORE also works)
```

MCP client configuration (the generic `mcpServers` shape; Claude Desktop and
IDE agents work the same way):

```json
{ "mcpServers": { "grovefs": {
  "command": "grovefs",
  "args": ["mcp", "--meta-dir", "/abs/path/to/store"],
  "env": { "GROVEFS_MCP_API_KEY": "ID:SECRET" }
} } }
```

`env` is only needed when the server was started with `--require-api-key`;
exporting it pre-authenticates the whole server.

**When your client does not manage the process for you**, send frames yourself
with the dependency-free script in this directory (see `scripts/mcp_call.py`):

```bash
python3 scripts/mcp_call.py --meta-dir <D> list                    # tools/list
python3 scripts/mcp_call.py --meta-dir <D> call list_views '{}'
python3 scripts/mcp_call.py --meta-dir <D> call define_derived_view '{"name":"release","base":"team","rules":"hide /photos/private"}'
python3 scripts/mcp_call.py --meta-dir <D> --api-key ID:SECRET list # through a --require-api-key gate
```

Check the wiring with a self-test first (it uses a throwaway store root and
never touches real data):

```bash
bash scripts/smoke.sh
```

## Protocol essentials (verbatim with the implementation)

- Newline-delimited JSON-RPC 2.0, protocol version `2025-06-18`; the
  `2025-03-26` and `2024-11-05` revisions are accepted too. `initialize` echoes
  a revision the server speaks and answers any other value with `2025-06-18`
  (never a blind echo of something it cannot honor).
- **Governance failures are not protocol errors**: per MCP convention they come
  back inside the result as
  `{"result":{"content":[{"type":"text","text":"…"}],"isError":true}}`, so an
  agent client shows the wording instead of crashing. The JSON-RPC layer is
  reserved for protocol problems, and each code tells you a different story:

  | Code | When it fires (measured) |
  |---|---|
  | `-32700` | frame is not valid JSON — `invalid JSON-RPC frame: …`; **the server keeps serving** the later lines |
  | `-32601` | method not implemented — `method not found: foo/bar` |
  | `-32602` | bad *protocol* arguments: `resources/read` with no `uri`, `prompts/get` unknown/missing prompt, bad `completion/complete` argument |
  | `-32603` | internal error, today only `resources/list` failing to read the store |
  | `-32001` | authentication gate (see "Authentication") |
  | `-32002` | resource URI unusable — malformed URI **or** the view's tree is not in the bound store |

  A *missing tool argument* is deliberately **not** a protocol error: it comes
  back as `isError` with `missing string argument 'name' (tool '…' requires: …)`,
  like any refusal. An unknown tool lists the whole surface and, when the name is
  close to a real one, adds `— did you mean '…'?`.
- Frames without an id (notifications) are never answered — do not wait for
  a reply after sending one (`notifications/initialized`,
  `notifications/cancelled`).
- Every tool accepts an optional `db_path` that overrides the server default,
  so one process can serve several store roots — **except** `preview_rules`
  (parser-only, no store) and except under a `--meta-store postgres://…` server,
  where the control plane is URI-bound and `db_path` is ignored (verified:
  the reply's `store` still names the URI).
- A PG-bound server answers against PG even with no local `meta/` directory at
  all; a file-bound server never reaches a PG or remote tree (see the probe
  refusal text below).
- Missing arguments and unknown tools surface as `isError`, not as protocol
  errors.
- `tools/list` entries carry `title` + `annotations` (`readOnlyHint`,
  `destructiveHint`, `idempotentHint`, `openWorldHint:false`), so a client can
  auto-approve the five read-only tools (`list_views`, `get_view`,
  `list_view_tree`, `preview_rules`, `validate_definition`). The four
  JSON-returning ones also declare an `outputSchema` and return
  `structuredContent` next to the text block — prefer `structuredContent` when
  your client surfaces it; the text block is the same JSON for everyone else.
- `arguments` may arrive as a JSON object or as a string holding one (both are
  accepted); anything else is `-32602`.

## The ten tools

| Tool | Arguments (★ required) | Purpose |
|---|---|---|
| `list_views` | `db_path` | Whole-store snapshot as `{backend, store, physical[], defined[]}` — `backend` is `sqlite`/`postgres` and `store` names the control-plane root actually read (use it to confirm you are talking to the store you meant) |
| `get_view` | ★`name` | Full record of one view, including the **verbatim `definition` rules** — read it before editing rules |
| `list_view_tree` | `view` \| ★(`base`+`rules`), plus `path`/`depth`(6)/`max`(800) | Renders the listing a mount would actually show; can also preview an unpublished rule set **without touching disk**. `view` resolves like a mount — a fork's own tree is walked as `kind:"fork"`. `size` is a metadata field, not a byte count (dirs 4096 / files 0 / symlinks report the target length) |
| `define_derived_view` | ★`name` ★`base` ★`rules` | Publish/replace a **read-only** derived lens |
| `alias_view` | ★`name` ★`real` \| `retire` | Publish a writable public alias; `retire` rotates names gaplessly (write new, then remove old) |
| `fork_view` | ★`name` ★`base` \| `seed` | Seed an **independent metadata tree** (structure writable, content `EROFS`, bytes shared). `seed:false` creates the tree EMPTY (root only) to be filled by `graft_view` |
| `graft_view` | ★`name` ★`map` \| `src`/`dest`/`skip_existing` | Copy a **curated subset** of one view's names into `name` — `name` may be a **fork OR a bare base view** (any tree that owns its `node` table; a derived lens or an alias is refused by role). `map` is plan text, one entry per line (blank lines and whole-line `#` comments ignored): `<src>` alone keeps that entry's **full relative path** in the destination (NOT its basename — the parent dirs are recreated), `<src>TAB<dst>` lands exactly at `dst` (that TAB is the only way to rename), and `dest` prefixes **every** line, TAB lines included. A trailing `#` is part of the name, not a comment. A directory source brings its whole subtree; missing destination dirs are created (0755); the reply's `entries` count includes auto-created parents; the whole plan lands in ONE transaction (any conflict refuses all of it unless `skip_existing`); bytes are shared (ledger hold per blob). `src` defaults to the fork's recorded base and is **required** when `name` is a base view |
| `remove_view` | ★`name` | Delete a definition slot (derived/alias/shared); deleting a fork drops its own tree too |
| `preview_rules` | ★`rules` | Validates verb syntax only; errors carry line numbers |
| `validate_definition` | ★`name` ★`base` ★`rules` | Runs a publish through the **entire governance guard set** before anything is written, returning the exact refusal text |

Neither read-only probe touches data bytes: `list_view_tree` opens the view's
tree read-only **through the backend the server is bound to** — the local
`<meta-dir>/meta/<view>.db` under `--meta-dir`, or the `grovefs_v_<hex>` PG
schema under `--meta-store postgres://…` — and renders it through the same
`LayoutMeta` lens a mount uses; `get_view` only reads the definition's text.
A tree the served store cannot reach is refused with the exact wording
(verified against the current build):

```
no metadata tree for view 'X' (<db>/meta/X.db) — list_view_tree walks this
store's own backend; a remote tree not reachable from here is not
```

… and the same probe is what the JSON-RPC layer answers as `-32002` when you
reach it through `resources/read` instead of as a tool (a resource URI that
cannot be served is treated as a protocol problem, not a governance refusal).
Because the walk only reads metadata and never asks the data plane for object
lengths, the `size` in a response is **not a file size** (dirs `4096`, files `0`,
symlinks report their target's character length); mount and `stat` for real bytes.

## Attachable context: resources, prompts, completion

When your client surfaces MCP resources, use them instead of a tool round-trip:

| URI | What you get |
|---|---|
| `grovefs://guide` | This server's operating manual as markdown (`text/markdown`) — the same content as `initialize.instructions` |
| `grovefs://views` | The whole-store snapshot — byte-identical to `list_views` |
| `grovefs://view/<name>/definition` | One slot's verbatim definition — identical to `get_view` |
| `grovefs://view/<name>/tree{?path,depth,max}` | The rendered listing — identical to `list_view_tree`, and `?path=&depth=&max=` drill into a big tree |

`prompts/list` offers the two playbooks (`reshape-view`, `rotate-alias`);
`completion/complete` is role-aware — a prompt's `base`/`real` argument completes
against physical trees, `rotate-alias`' `new`/`old` against existing aliases, and
a resource's `{name}` against every mountable name.

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

**The derived shape does NOT have to resemble the base tree.** Only the `<src>`
side is bound to the base's real display paths (step 3); `<dst>` paths are a
brand-new display tree — unrelated directory names, depths, and grouping are
normal. Verified semantics of the two re-pathing verbs:

- `map <src> <dst>` **moves** the entry: it disappears from `<src>` (its
  original directories may remain as empty scaffolds) and is served at `<dst>`
  with its real bytes.
- `link <src> <dst>` **keeps** `<src>` and adds `<dst>` — the same entries under
  an extra name.
- To make the custom tree the WHOLE view, `only <new-root>` (e.g.
  `map /seeds/docs/a.md /pub/alpha.md` + `only /pub` renders exactly
  `/pub/alpha.md` and nothing of the base layout).

Measured against the current build (base tree `/a/1.txt`, `/a/sub/2.txt`,
`/b/3.txt`), the non-obvious ones:

- `only` filters **source-side coordinates only**. A `map`/`link` `<dst>` and its
  synthesized parents survive even when they lie outside the `only` subtree, and a
  `link`ed entry keeps its original `<src>` position as well — `only /b` +
  `link /a /c` still shows all of `/a` and `/c`.
- `hide` is evaluated FIRST and also takes SOURCE coordinates, so `hide <dst>` is a
  no-op. `hide` beats `only`: `only /a` + `hide /a/sub` renders `/a` and `/a/1.txt`
  and nothing under `/a/sub`.
- `hide <src>` together with `map`/`link <src> …` leaves the destination present but
  EMPTY — the leaf name is served, the pruned subtree is not (`hide /a` +
  `map /a /x/a` renders `/x` and `/x/a`, no children).
- `link` is **not** a hard link: both display names come from the SAME source row,
  so removing or renaming at either name is reflected at both. A real `ln` through a
  mount instead adds a second row pointing at the same inode.
- Nested rules use nested coordinates correctly: a rule's `<src>` is the source
  path, a second `map`'s `<src>` may be the FIRST `<dst>` (display tree), and
  `<dst>` must be unique across rules (a clash is refused at parse time).
- Through a WRITABLE (`--layout`) lens: writing inside `only` works, creating inside
  a pass-through ancestor outside `only` is `EPERM`, creating inside a purely
  synthetic directory is `EPERM` — but a subtree relocated in by `map`/`link` IS
  writable even when its source path is outside `only`.
- Rules match PATH TEXT, not inodes: rename a source entry and every rule naming
  the old path silently addresses nothing.

What the lens cannot do is *invent content*: every rendered entry is some base
entry re-pathed, mirrored, or pruned, plus synthesized scaffold dirs — a derived
view never holds bytes the base doesn't have.

Derived views always mount read-only (write through to the base view). A `map`
`<dst>`'s parent directories are synthesized on demand (scaffolding), but you
**cannot create or rename entries inside a purely synthetic directory**
(`EPERM`): the directory only becomes real once something is `map`ped into it.

## What gets refused (exact texts, so you self-check instead of retry-guessing)

Every string below is the byte-exact `isError` text measured from the current
build. Match on the leading phrase, not on the whole sentence — the tail of some
lines interpolates your own names/paths.

| What you tried | The `isError` text you get back |
|---|---|
| Missing a required tool argument | `missing string argument '<key>' (tool '<name>' requires: …)` |
| Typoed a tool name | `unknown tool '<name>'` + `— did you mean '<nearest>'?` + `(available: …)` |
| `list_view_tree` with only `base` | `'base' also needs inline 'rules' to simulate a lens (got base '<b>' alone)` |
| `list_view_tree` with neither `view` nor `base`+`rules` | `provide 'view' (an existing view) or 'base' + 'rules' (a simulated lens)` |
| `list_view_tree` on a name with no reachable tree | `no metadata tree for view '<X>' (<path>) — list_view_tree walks this store's own backend; …` |
| `get_view` / `list_view_tree` on an unknown name | `no view '<name>' in <store> (not a definition nor a physical view tree) — \`list_views\` names what exists` |
| Bad verb or ambiguous destination in rules | `bad layout rules: line <N>: …` |
| Derived name == base name | `derived view name must differ from its base` |
| Definition name collides with a physical db name | `'<name>' already names a physical view db — pick another name` |
| Defining over an alias / fork name | `'<name>' is a pure alias — remove it first …` / `'<name>' is a fork (an independent metadata tree) — remove it first …` |
| Alias target is itself an alias | `alias target '<real>' is itself an alias (-> '<x>') — chains are not supported; alias the physical view instead` |
| Alias target is a derived view | `alias target '<real>' is a derived (virtual) view — aliases map onto a physical view only` |
| Alias target is a fork | `alias target '<real>' is a fork (an independent metadata tree) — aliases map onto a physical view only; give the fork its own key binding …` |
| A second public name for the same real view | `real view '<real>' is already governed by alias '<old>' — one public name per real view; to rotate it gaplessly use \`… --retire\`` |
| `retire` naming something that is not an alias here | `--retire '<old>' is not a pure alias here (a derived view is separate — \`grovefs view list\` shows what '<old>' actually is)` |
| `retire` removing an alias of a different view | `--retire '<old>' currently aliases a different real view ('<r>'), not '<real>' — rotation must retire the previous name of the same view` |
| `retire` equal to the new name | `--retire must name a DIFFERENT (previous) alias, not the one being created` |
| Forking a base with no local tree | `no physical base tree for '<base>' in <store> — \`fork\` copies a LOCAL SQLite tree; … A PostgreSQL-backed view cannot be forked (…)` |
| Forking through a `--meta-store postgres://…` server even when the base schema exists | `fork is not implemented on the PostgreSQL backend yet (the tree copy needs a per-view snapshot path; PG-backed views are served through a meta-server or a mount)` |
| Forking a base that is itself governed | `base '<base>' is governed by a definition (alias/derived/fork) — fork the physical base tree instead` |
| Fork name == base name | `fork name must differ from its base` |
| Forking a name that already has a definition | `view '<name>' already has a definition — remove it first (\`grovefs view rm --name <name>\`) before forking` |
| `fork_view` with `seed:false` (succeeds, and tells you the next step) | `forked view 'lab' from base 'team' as an EMPTY tree: root only, nothing copied` + `note: place the subset you want with \`grovefs view graft --name lab --src team --map plan.tsv\` (or the \`graft_view\` tool, same implementation) …` |
| A successful `graft_view` (renames + one directory subtree) | `grafted 3 entries (1 directory) from 'team' into 'lab' at /: 2 shared object(s), 0 skipped` — names moved, bytes shared |
| `graft_view` whose destination has no tree of its own | `no tree for graft destination 'zz' in <store> — create it first (\`grovefs view create --view zz\` for a base view, or \`fork_view\` with \`seed:false\` for an empty fork …)` |
| `graft_view` into/out of a derived lens | `graft destination 'lens1' is a derived lens: it renders another tree read-only and owns no nodes — use a base view or a fork` (same text with `graft source …` when the SOURCE is the lens) |
| `graft_view` into/out of an alias | `graft destination 'pub' is an alias of 'team' — use 'team' (an alias has no tree of its own)` (same pattern for the source role) |
| `graft_view` onto a base view without saying where entries come from | `'team' is a base view — pass --src to name the view its entries come from` |
| `graft_view` with `src` equal to `name` | `a view cannot graft from its own tree — pick another source` |
| A plan that is only blank lines / comments | `empty graft plan — one '<src>' or '<src>TAB<dst>' line per entry (blank lines and '#' comments are ignored)` |
| A plan path containing `..` | `line 1: path '../esc' must not contain '..'` |
| A plan line naming the root itself (`/`) | `line 1: empty path — the root itself cannot be grafted, name an entry inside it` |
| A destination name already taken (whole plan rolls back) | `'M31_2025.fits' already exists in the destination — rename it in the plan or pass --skip-existing` |
| `remove_view` on a name with no definition slot (incl. a physical base view) | `no view definition '<name>' in <store>` — the store is named as `… (local files)` or `… (PostgreSQL grovefs_control)`, **not** as a `<db>/meta/views` path |
| Name is not a single safe path element (empty, `.`, `..`, containing `/`, `\\` or whitespace) | `invalid view name '<name>' (must be one safe path element)` for `define`/`validate`; `invalid alias name …` / `invalid fork name …` / `invalid destination name …` (and `invalid source view name …`) for those tools; `remove_view`, `get_view` and `list_view_tree` shorten it to `invalid view name '<name>'`, and a bad **base** reads `invalid base view name '<base>'` / `invalid real view name '<real>'` |

## Authentication and audit (when fronting the interface to many callers)

- `grovefs mcp --require-api-key` gates **exactly these six methods**: `tools/list`,
  `tools/call`, `resources/list`, `resources/templates/list`, `resources/read`,
  `completion/complete`. Measured open (never gated): `initialize`, `ping`,
  `prompts/list`, **`prompts/get`** (so a playbook is readable pre-auth — it is
  static text, not store state), `logging/setLevel`, and all notifications.
- Credentials: `grovefs apikey --meta-dir <D> create --label L --for mcp`
  (or `--for any`). The MCP entrance asks for scope `mcp`, so a `--for mount`
  key is refused with `this api key is scoped to 'mount', not 'mcp'`.
  `--view`/`--ro` are **mount-plane** dimensions (see the boundary below).
  The plaintext is printed once; only the SHA-256 is stored.
- Presentation: `params.apiKey = "<ID>:<SECRET>"` in `initialize`, or export
  `GROVEFS_MCP_API_KEY` on the server side (verified: an env-preauthorized
  session is admitted and its later calls are attributed to that key id).
- Unauthenticated access to a gated method → JSON-RPC `-32001` with
  `authentication required: present a valid apiKey in the initialize params (or export GROVEFS_MCP_API_KEY for the server)`.
  **The client never learns *why* the key failed** — a bad id, a bad secret, a
  revoked key and a wrong-scope key all look like the same `-32001`. The real
  reason is written to the audit log as an `initialize` row
  (`authentication failed: unknown api key id` / `invalid api key secret` /
  `this api key has been revoked` / `this api key is scoped to 'mount', not 'mcp'`),
  so debugging an auth problem is an operator job: `grovefs apikey calls`.
- Audit: every `tools/call` and `resources/read` lands in the table, plus every
  gated call refused for no key and every `initialize` attempt, success or
  failure. Columns: `ts, key_id, key_label, method, tool, ok, detail`. `detail`
  is secret-free and size-bounded — it keeps the identifying names
  (`name=… base=… real=… view=… retire=… path=…`) and reduces `rules` to a line
  count (`rules=2L`); `depth`/`max`/`db_path` are not recorded. A refused call
  stores the refusal text (clipped to 200 chars) as its `detail`.
- Revocation is immediate: after `apikey revoke` that key fails `initialize`
  authentication on the next connection (verified → the gated methods return
  `-32001` again and the audit gets `this api key has been revoked`).
- On a `--meta-store postgres://…` server the keys and the audit rows are the PG
  tables `grovefs_control.api_keys` / `grovefs_control.mcp_calls` (mint with
  `grovefs apikey --meta-store <URI> create …`); no local `api_keys.db` is created —
  verified. So a key minted anywhere gates every server pointed at that URI.
- If the control-plane backend is unreachable the server **does not start**: it
  exits with `Error: … "postgres connect error connecting to server"` (verified
  against a dead URI) — so a "command immediately returns nothing" symptom is a
  startup problem, not an auth problem. The key store itself is opened
  best-effort: if only it fails while the control store is fine, every gated call
  is still refused (`-32001`) but **no audit row can be written** — never read an
  empty audit log as "nothing happened".

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
- **A key's `--ro` flag (and `apikey set-ro`) does NOT restrict this entrance.**
  `ro` is enforced only on the mount plane (`meta-server`/`data-server` write
  verdicts); the MCP server never reads it. Verified: an `--ro` key with scope
  `mcp` published a derived view through `define_derived_view` successfully. So
  there is no read-only MCP credential — gate the plane by *who* holds a key, and
  reach for `ro`/`--view` only for mount credentials.
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

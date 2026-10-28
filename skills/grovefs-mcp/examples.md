# Real Transcripts

Everything below is raw output produced against this repository (binary
`./target/release/grovefs`, protocol `2025-06-18`) — not hand-written examples.
Lines marked `[stderr]` are diagnostics from the client script, not protocol
frames.

The sample store root was built with the CLI (the MCP entrance never creates
physical base views):

```bash
grovefs --home <D>/store import --view team --source <D>/seeds
# imported into view 'team' under /seeds: 4 dir(s), 3 object(s) (3 hardlinked zero-copy, 0 copied), 0 skipped, 0 failed
```

Note that `import` nests the source directory **under `/seeds`**, so the real
display path is `/seeds/photos/private`, not `/photos/private`.
**This is exactly why rules must be written from a `list_view_tree` response and
never guessed from the source directory layout.**

## 1. One read-only lens: look → write rules → publish → verify

```bash
python3 scripts/mcp_call.py --db-path <D>/store call list_view_tree '{"view":"team"}'
```
```
[stderr] [handshake] grovefs 0.1.0, protocol 2025-06-18
{
  "dir_count": 5,
  "entries": [
    {"kind": "dir",  "path": "/seeds",                   "size": 4096},
    {"kind": "dir",  "path": "/seeds/docs",              "size": 4096},
    {"kind": "dir",  "path": "/seeds/docs/2026",         "size": 4096},
    {"kind": "file", "path": "/seeds/docs/2026/b.md",    "size": 0},
    {"kind": "file", "path": "/seeds/docs/a.md",         "size": 0},
    {"kind": "dir",  "path": "/seeds/photos",            "size": 4096},
    {"kind": "dir",  "path": "/seeds/photos/private",    "size": 4096},
    {"kind": "file", "path": "/seeds/photos/private/x.jpg", "size": 0}
  ],
  "file_count": 3,
  "kind": "physical",
  "rendered_from": "team",
  "root_path": "/",
  "truncated": false
}
```

`size` is the `size` field in the metadata, **not a file's byte count**: the walk
never asks the data plane for object lengths, so dirs answer `4096` (the stat
placeholder), files answer `0`, and symlinks answer their target's character
length. The tool description and the `initialize` `instructions` both state this
explicitly; mount and `stat` for real sizes, and never use this field for usage
or checksums.

```bash
python3 scripts/mcp_call.py --db-path <D>/store --quiet \
  call define_derived_view '{"name":"release","base":"team","rules":"hide /seeds/photos/private"}'
```
```
defined derived view 'release' over base 'team': 1 hide, 0 only, 0 reloc rule(s)
note: mounts of 'release' are read-only (virtual shape) — write to base view 'team'
```

```bash
python3 scripts/mcp_call.py --db-path <D>/store --quiet call list_view_tree '{"view":"release"}'
```
```
{
  "dir_count": 4,
  "entries": [
    {"kind": "dir",  "path": "/seeds",                "size": 4096},
    {"kind": "dir",  "path": "/seeds/docs",           "size": 4096},
    {"kind": "dir",  "path": "/seeds/docs/2026",      "size": 4096},
    {"kind": "file", "path": "/seeds/docs/2026/b.md", "size": 0},
    {"kind": "file", "path": "/seeds/docs/a.md",      "size": 0},
    {"kind": "dir",  "path": "/seeds/photos",         "size": 4096}
  ],
  "file_count": 2,
  "kind": "derived",
  "rendered_from": "team",
  "root_path": "/",
  "truncated": false
}
```

`/seeds/photos` is still there and the `private` under it is gone — this is the
shape a mount will show. The same shape is also available as a resource (§4).

## 2. What a refusal looks like (`isError`, not a protocol error)

```bash
... call preview_rules '{"rules":"bog /x"}'
```
```
bad layout rules: line 1: unknown layout verb 'bog'
```

```bash
... call validate_definition '{"name":"team","base":"team","rules":"hide /seeds/docs/a.md"}'
```
```
derived view name must differ from its base
```

```bash
... call list_view_tree '{"view":"ghost"}'
```
```
no local metadata tree for view 'ghost' (.migtest/skill/store/meta/ghost.db) — list_view_tree introspects a LOCAL SQLite store only; a remote/PG-backed tree is not walked here
```

All three texts are identical to what `grovefs view …` prints (same library
functions). The client script reports them as exit code 3 instead of crashing.

## 3. Raw frames (no client library at all)

```bash
printf '%s\n%s\n%s\n' \
 '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"curl","version":"0"}}}' \
 '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
 '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_views","arguments":{}}}' \
 | grovefs mcp --db-path <D>/store
```

Only two frames come back — `notifications/initialized` **gets no reply**, by
design rather than by packet loss:

```
{"id":1,"jsonrpc":"2.0","result":{"capabilities":{"completions":{},"logging":{},"prompts":{"listChanged":false},"resources":{"listChanged":false,"subscribe":false},"tools":{"listChanged":false}},"instructions":"Customize data listings by governing SERVER-SIDE views over one store. …","protocolVersion":"2025-06-18","serverInfo":{"name":"grovefs","title":"GroveFS view governance","version":"0.1.0"}}}
{"id":2,"jsonrpc":"2.0","result":{"content":[{"text":"{\n  \"db_path\": \".migtest/skill/store\",\n  \"defined\": [\n    {\n      \"hide\": 1,\n      \"kind\": \"derived\",\n      \"line\": \"view 'release' -> base 'team': 1 hide, 0 only, 0 reloc rule(s) (read-only)\",\n      \"mount\": \"read-only (derived)\",\n      \"name\": \"release\",\n ...
```

The `instructions` field in the `initialize` result is the server's own
operating manual; MCP clients inject it into context, so this skill does not
repeat its wording.

## 4. Resources / completion / log pushes

```bash
... read grovefs://view/release/definition
```
```
{
  "contents": [
    {
      "mimeType": "application/json",
      "text": "{\n  \"base\": \"team\",\n  \"definition\": \"base team\\nhide /seeds/photos/private\",\n  \"hide\": 1,\n  \"kind\": \"derived\",\n  \"mount\": \"read-only (virtual shape; writes go to the base view)\",\n  \"name\": \"release\",\n  \"only\": 0,\n  \"reloc\": 0\n}",
      "uri": "grovefs://view/release/definition"
    }
  ]
}
```

You must peel `contents[0].text` first (that inner string is the JSON payload).

```bash
... complete base 'r'
```
```
{"completion": {"hasMore": false, "total": 1, "values": ["release"]}}
```

```bash
... setlevel warning call list_view_tree '{"view":"ghost"}'      # a refused call
```
```
[stderr] [setlevel] warning — later calls may push notifications/message
[stderr] [push] notifications/message {"data": "tools/call failed: no local metadata tree for view 'ghost' (.migtest/skill/store/meta/ghost.db) \u2014 list_view_tree introspects a LOCAL SQLite store only; a remote/PG-backed tree is not walked here", "level": "warning", "logger": "grove-mcp"}
[stderr] the tool refused this call (isError) — not a protocol error
```

With the threshold at `warning`, **successful calls push nothing** (`info` does
not reach the gate) — never read "no push" as "not executed". Push frames are
written **after** the response, so the client must keep reading (the
`mcp_call.py` in this directory drains a short window after each answer).

## 5. One public name per view, and gapless rotation

```bash
... call alias_view '{"name":"team-public","real":"team"}'
```
```
defined pure alias 'team-public' -> real view 'team'
note: 'team' is no longer a mount entry point while this definition exists (mounts by its real name are refused, locally and via meta-server); alias mounts show the FULL tree and are WRITABLE
```

```bash
... call alias_view '{"name":"team-2","real":"team"}'            # trying to add a second public name
```
```
real view 'team' is already governed by alias 'team-public' — one public name per real view; to rotate it gaplessly use `grovefs view alias --name team-2 --real team --retire team-public`
```

The correct move retires the old name in the same call (write-before-remove, so
the public name never has a gap):

```bash
... call alias_view '{"name":"team-public2","real":"team","retire":"team-public"}'
```

## 6. The gate (`--require-api-key`) and the audit

```bash
python3 scripts/mcp_call.py --db-path <D>/store --server-arg --require-api-key list
```
```
[stderr] JSON-RPC error -32001: authentication required: present a valid apiKey in the initialize params (or export GROVEFS_MCP_API_KEY for the server)
[stderr]   → this entrance is gated: present apiKey in initialize params (--api-key ID:SECRET) or export GROVEFS_MCP_API_KEY for the server
```

Presenting a credential works (mint the key via the CLI; the plaintext appears
once):

```bash
grovefs apikey --db-path <D>/store create --label agent --for mcp     # → "<id>:<secret>"
python3 scripts/mcp_call.py --db-path <D>/store --server-arg --require-api-key --api-key '<id>:<secret>' list   # normal 9-tool answer
grovefs apikey --db-path <D>/store calls                              # the refused tools/list is in the audit too
```

## 7. Wiring self-test

`bash scripts/smoke.sh` runs the full workflow on a `mktemp -d` throwaway store
root (discover → preview → dry-run → publish → verify → guards → alias rotation
→ fork → resources/prompts/completion → logging → cleanup → API-key gate and
audit), printing every assertion:

```
SUMMARY: pass=43 fail=0
ALL MCP CHECKS PASSED
```

Any `FAIL:` line means **the contract between this skill and the implementation
has been broken** (the implementation changed a wording, or the client framing
here is wrong) — it is never a data problem: the script never touches a real
store root and deletes its temp dir on exit.

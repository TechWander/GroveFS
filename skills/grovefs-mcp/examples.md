# 真实回显样例

下面全部是本仓库上跑出来的原始输出（二进制 `./target/release/grovefs`，协议 `2025-06-18`），
不是我拼的示例。标注 `[stderr]` 的行是客户端脚本的诊断输出，不属于协议帧。

样例存储根由 CLI 造出（MCP 不建物理基视图）：

```bash
grovefs --home <D>/store import --view team --source <D>/seeds
# imported into view 'team' under /seeds: 4 dir(s), 3 object(s) (3 hardlinked zero-copy, 0 copied), 0 skipped, 0 failed
```

注意 `import` 把源目录**整体挂在 `/seeds` 下**，所以真实显示路径是 `/seeds/photos/private` 而不是
`/photos/private`。**这就是为什么规则必须照 `list_view_tree` 的回包写，不能照源目录结构猜。**

## 1. 一副只读透镜：看 → 写规则 → 发布 → 复核

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

`size` 是元数据里的 `size` 字段，**不是文件字节数**：遍历不向数据面问对象长度，
所以目录回 `4096`（stat 占位）、文件回 `0`、软链回 target 文本的字符长度。
工具描述与 `initialize` 的 `instructions` 已把这个口径写清；要真实大小请挂载后 `stat`，
别拿这个字段当用量或校验。

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

`/seeds/photos` 还在、它底下的 `private` 没了 —— 这就是挂载将看到的形状。同一形状也能作为资源取到（§4）。

## 2. 拒绝长什么样（`isError`，不是协议错误）

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

这三条的文案与 `grovefs view …` 完全相同（同一套库函数）。客户端脚本把它们当 exit code 3 上报，而不是崩溃。

## 3. 裸帧（不经任何客户端库）

```bash
printf '%s\n%s\n%s\n' \
 '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"curl","version":"0"}}}' \
 '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
 '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_views","arguments":{}}}' \
 | grovefs mcp --db-path <D>/store
```

回帧只有两条——`notifications/initialized` **没有回包**，这是设计而非丢包：

```
{"id":1,"jsonrpc":"2.0","result":{"capabilities":{"completions":{},"logging":{},"prompts":{"listChanged":false},"resources":{"listChanged":false,"subscribe":false},"tools":{"listChanged":false}},"instructions":"Customize data listings by governing SERVER-SIDE views over one store. …","protocolVersion":"2025-06-18","serverInfo":{"name":"grovefs","title":"GroveFS view governance","version":"0.1.0"}}}
{"id":2,"jsonrpc":"2.0","result":{"content":[{"text":"{\n  \"db_path\": \".migtest/skill/store\",\n  \"defined\": [\n    {\n      \"hide\": 1,\n      \"kind\": \"derived\",\n      \"line\": \"view 'release' -> base 'team': 1 hide, 0 only, 0 reloc rule(s) (read-only)\",\n      \"mount\": \"read-only (derived)\",\n      \"name\": \"release\",\n ...
```

`initialize` 结果里的 `instructions` 就是服务端自带的操作手册；MCP 客户端会把它注入上下文，
所以本 skill 不重复它的措辞。

## 4. 资源 / 补全 / 日志推送

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

要内容得先剥一层 `contents[0].text`（它本身才是 JSON 文本）。

```bash
... complete base 'r'
```
```
{"completion": {"hasMore": false, "total": 1, "values": ["release"]}}
```

```bash
... setlevel warning call list_view_tree '{"view":"ghost"}'      # 被拒的调用
```
```
[stderr] [setlevel] warning — later calls may push notifications/message
[stderr] [push] notifications/message {"data": "tools/call failed: no local metadata tree for view 'ghost' (.migtest/skill/store/meta/ghost.db) \u2014 list_view_tree introspects a LOCAL SQLite store only; a remote/PG-backed tree is not walked here", "level": "warning", "logger": "grove-mcp"}
[stderr] the tool refused this call (isError) — not a protocol error
```

阈值设 `warning` 时，**成功的调用不会有推送**（`info` 达不到门限）——别把「没推送」当成「没执行」。
推送帧排在应答**之后**，所以客户端要能读到它（本目录的 `mcp_call.py` 在拿到应答后再排空一小段）。

## 5. 一视图一公开名，以及无缝轮换

```bash
... call alias_view '{"name":"team-public","real":"team"}'
```
```
defined pure alias 'team-public' -> real view 'team'
note: 'team' is no longer a mount entry point while this definition exists (mounts by its real name are refused, locally and via meta-server); alias mounts show the FULL tree and are WRITABLE
```

```bash
... call alias_view '{"name":"team-2","real":"team"}'            # 想再加一个公开名
```
```
real view 'team' is already governed by alias 'team-public' — one public name per real view; to rotate it gaplessly use `grovefs view alias --name team-2 --real team --retire team-public`
```

正确做法是把旧名一起撤掉（先写后删，公开名不断档）：

```bash
... call alias_view '{"name":"team-public2","real":"team","retire":"team-public"}'
```

## 6. 上门（`--require-api-key`）与审计

```bash
python3 scripts/mcp_call.py --db-path <D>/store --server-arg --require-api-key list
```
```
[stderr] JSON-RPC error -32001: authentication required: present a valid apiKey in the initialize params (or export GROVEFS_MCP_API_KEY for the server)
[stderr]   → this entrance is gated: present apiKey in initialize params (--api-key ID:SECRET) or export GROVEFS_MCP_API_KEY for the server
```

带凭据即可（key 用 CLI 铸，明文只出现一次）：

```bash
grovefs apikey --db-path <D>/store create --label agent --for mcp     # → "<id>:<secret>"
python3 scripts/mcp_call.py --db-path <D>/store --server-arg --require-api-key --api-key '<id>:<secret>' list   # 正常出 9 个工具
grovefs apikey --db-path <D>/store calls                              # 被拒的 tools/list 也在审计里
```

## 7. 接线自检

`bash scripts/smoke.sh` 会在 `mktemp -d` 的临时存储根上跑完整流程（发现 → 预览 → 干跑 → 发布 →
复核 → 守卫 → 别名轮换 → fork → 资源/提示/补全 → 日志 → 清理 → API key 门与审计），逐条打印断言：

```
SUMMARY: pass=40 fail=0
ALL MCP CHECKS PASSED
```

任何一条 `FAIL:` 都是**这个 skill 与实现的约定被破坏**的信号（实现改了文案、或客户端发帧方式不对），
不是数据问题：脚本从不碰真实存储根，跑完即删。

# GroveFS MCP 参考（方法面 / 错误码 / 落盘效果 / CLI 对照）

本文件是 [SKILL.md](SKILL.md) 的细则附录：只讲实现里真实存在的东西，逐条对应 `src/mcp.rs`。

## 1. 服务与握手

```
grovefs mcp [--db-path <D>] [--home <H>] [--require-api-key]
```

- `--db-path`：治理库根（含 `meta/`、`meta/views/`），与 `grovefs view` / `meta-server --db-path` 同一个目录概念；缺省回退 `--home`，再回退 `$GROVEFS_HOME` / `~/.grovefs`。
- 每个工具都带可选 `db_path`，**逐调用覆盖**默认根（一个进程服务多个存储根）。
- 帧格式：换行分隔的 JSON-RPC 2.0，一行一帧，UTF-8，stdin 进 / stdout 出。
- `initialize` 的 `params`：`protocolVersion`（原样回声，不校验）、`capabilities`、`clientInfo`，
  以及可选 **`apiKey: "<ID>:<SECRET>"`**（唯一能带凭据的开放方法）。
- 握手成功回：

```json
{"protocolVersion":"2025-06-18",
 "capabilities":{"tools":{"listChanged":false},"resources":{"subscribe":false,"listChanged":false},
                 "prompts":{"listChanged":false},"completions":{},"logging":{}},
 "serverInfo":{"name":"grovefs","title":"GroveFS view governance","version":"<crate version>"},
 "instructions":"…"}
```

`instructions` 是服务端自带的操作手册（发现/预览/换形/干跑四段话 + 鉴权提示）：客户端会把它注入上下文，
所以**本 skill 不重复它的文案**，只补上实现细节与坑位。工具自己的 `description` 同样自解释。

- 客户端随后发 `notifications/initialized`（**无 id，永不作答**）。

## 2. 方法面与鉴权门

| 方法 | `--require-api-key` 时 | 说明 |
|---|---|---|
| `initialize` | 开放 | 唯一可携带 `apiKey` 的方法；成功即给会话盖章身份（后续审计归属） |
| `ping` | 开放 | 回 `{}` |
| `notifications/*`、`initialized` | 开放 | 静默接收，**不作答**（别等回包） |
| `tools/list` | **门** | 回 9 个工具 + 自描述 schema |
| `tools/call` | **门** | 治理失败在结果内（`isError`），不进 JSON-RPC 错误层 |
| `resources/list` | **门** | 逐槽枚举（物理→tree；派生/别名/fork→definition+tree）；库读不出时 `-32603` |
| `resources/templates/list` | **门** | 两副模板（静态） |
| `resources/read` | **门** | URI 不合法/未知视图 → `-32002`（坏 URI 是协议问题，不是治理拒绝） |
| `completion/complete` | **门** | 只用 `params.argument`（`ref` 被忽略）；非视图名类参数回空列表 |
| `prompts/list` | 开放 | 两副剧本 |
| `prompts/get` | 开放 | 缺参/未知提示 → `-32602` |
| `logging/setLevel` | 开放 | 只调本会话详谈程度，从不被门挡住 |
| 其它 | — | 有 id → `-32601 method not found: <m>`；无 id 的未知通知保持静默 |

未鉴权碰门内方法 → `-32001`，文案：
`authentication required: present a valid apiKey in the initialize params (or export GROVEFS_MCP_API_KEY for the server)`；
同时按审计规则记一条失败（`unauthorized: no valid api key`）。

## 3. 错误码

| 码 | 触发 | 典型文案 |
|---|---|---|
| `-32700` | 非法 JSON 帧 | `parse error: …`（**服务继续可用**，不会掉线） |
| `-32601` | 未知方法且带 id | `method not found: foo/bar` |
| `-32602` | 参数问题：`resources/read` 缺 `uri`、`prompts/get` 缺/错参数、`completion/complete` 内部错 | `unknown prompt '<x>'` / `rotate-alias needs arguments 'real', 'new' and 'old'` |
| `-32603` | 内部错误：`resources/list` 读不出存储 | — |
| `-32001` | 鉴权门 | 见上 |
| `-32002` | 资源 URI 不可用 | `unsupported resource uri '<uri>' (expected grovefs://view/<name>/(definition\|tree))` / `resource uri '<uri>' names no view` |
| （无码） | 治理拒绝、缺工具参数、未知工具 | 结果内 `isError:true` + 与 CLI 逐字相同的文案 |

## 4. 资源（与工具结果逐字节相同）

模板：

```json
[{"uriTemplate":"grovefs://view/{name}/definition","name":"view-definition","mimeType":"application/json"},
 {"uriTemplate":"grovefs://view/{name}/tree","name":"view-tree","mimeType":"application/json"}]
```

- `grovefs://view/<name>/definition`：该槽的**逐字治理文本**（派生规则原文，或别名/fork 的 `base <x>` 行）；物理视图回它的挂载语义说明。等价于 `get_view`。
- `grovefs://view/<name>/tree`：挂载实际所见的清单（经真实透镜渲染，含合成目录），等价于 `list_view_tree {"view":"<name>"}`。

两者（资源与工具）回的 `entries[].size` 都是**元数据字段而非字节数**：目录 `4096`、文件 `0`、
软链为 target 字符长度——遍历只读元数据，不向数据面问对象长度（工具描述与 `instructions` 已自述这一点）。

`resources/read` 的信封是 `{"contents":[{"uri","mimeType","text"}]}`，`text` 里才是与工具结果逐字节相同的 JSON 串
（所以客户端取内容时要先剥一层 `contents[0].text`）。

## 5. 提示（剧本，实例化成一条 user 消息）

| 提示 | 参数 | 内容 |
|---|---|---|
| `reshape-view` | ★`base`，`goal`（可选） | 五步「读真实路径 → 起草规则 → `validate_definition` 干跑 → `define_derived_view` 发布 → `list_view_tree` 复核」，全程控制面、不挂载 |
| `rotate-alias` | ★`real` ★`new` ★`old` | 一条 `alias_view {name:new, real, retire:old}` 先写后删无缝轮换；再 `list_views` 确认；老连接自行排空 |

## 6. 补全与日志

- `completion/complete`：`{"argument":{"name":"base","value":"re"}}` → 回本地存储里前缀匹配（大小写不敏感）的视图名
  `{"completion":{"values":[…],"total":N,"hasMore":bool}}`（最多 100 条）。只对 `name`/`base`/`view`/`real` 生效；**只查本地 SQLite**，不触达远端/PG。
- `logging/setLevel`：`{"level":"warning"}`。设了阈值之后，服务端在**被应答的调用**之后追加
  `notifications/message` 帧（`logger: "grove-mcp"`），但**只有严重度达到阈值才推**：成功是 `info` + `<method> ok`，
  任何拒绝或失败是 `warning` + `<method> failed: <reason>` —— 阈值设为 `warning` 时，一次成功调用什么都不推。
  记日志的调用：`tools/call`、`tools/list`、`resources/list`、`resources/read`、`prompts/get`、`completion/complete`；
  `initialize`/`ping`/`prompts/list`/模板列表保持安静。通知帧排在应答**之后**写出，stdout 仍是合法帧流。
  严重度单调：`debug 10 < info 20 < notice 30 < warning 40 < error 50 < critical 60 < alert 70 < emergency 80`，未知名字按 `info` 计。

## 7. 每个写工具在盘上留下什么（可用来做旁路核验）

| 工具 | 落盘 |
|---|---|
| `define_derived_view` | 覆写 `<db>/meta/views/<name>.layout`（首行 `base <base>` + 规则行） |
| `alias_view` | 写 `<db>/meta/views/<name>.layout`（`base <real>`）；带 `retire` 时**先写新再删旧** |
| `fork_view` | 新树 `<db>/meta/<name>.db`（+`-wal`/`-shm`）+ `<db>/meta/views/<name>.layout`，并更新跨视图账本 `refs.db` |
| `remove_view` | 删 `.layout`；若槽是 fork，连带删它的 `meta/<name>.db*` 并 `rebuild_ledger` |
| `preview_rules` / `validate_definition` / 三个只读探查 | **什么都不写** |

## 8. CLI 对照（同一套库函数、同一套拒绝文案）

| MCP 工具 | CLI 等价 |
|---|---|
| `list_views` | `grovefs view list` |
| `define_derived_view` | `grovefs view define --name N --base B --rules F`（规则走**文件**，不是内联文本） |
| `alias_view` | `grovefs view alias --name N --real R [--retire OLD]` |
| `fork_view` | `grovefs view fork --name N --base B` |
| `remove_view` | `grovefs view rm --name N` |

**接口独有的四个**（CLI 无同名子命令；它们是 `grovefs::mcp` 里的只读探查 / 干跑库函数）：
`get_view`、`list_view_tree`、`preview_rules`、`validate_definition`。CLI 侧近似做法：直接读
`<db>/meta/views/<name>.layout`；先 `mount --layout FILE` 临时挂一副透镜看形状；非法规则在
`view define` 处 fail-fast ——而接口能在**不落盘**的前提下拿到同一判决。

**只有 CLI 有、MCP 没有的**（别在接口里找）：`init` / `view create`（建物理基）、`mount` / `umount`、
`import`（零拷贝登记存量）、`view export` / `view import`（单视图命名空间跨机搬运）、
`apikey *`（铸 key、scope、审计回读、撤销）、`meta-server` / `data-server`、条带/HA/GC。
接口 = `grovefs view` 的同一套治理，**智能体做不出 CLI 做不出的事，也绕不过任何守卫**。

> 名字对照提醒：CLI 侧另有 `grovefs view import`（把某视图的**元数据命名空间**从 JSONL 导入本存储根，跨机搬迁用）；
> 它与数据面的 `grovefs import --view V --source DIR`（存量字节零拷贝登记）**同名不同义**。两者都不在 MCP 工具面里。

## 9. 常见故障 → 处置

| 现象 | 原因 | 处置 |
|---|---|---|
| 拉起即 `-32001` | 服务端 `--require-api-key`，客户端没在 `initialize` 带 `apiKey` | 用 `grovefs apikey create --label agent --for mcp` 铸 key（明文只打印一次），放进 `initialize.params.apiKey` 或服务端 `GROVEFS_MCP_API_KEY` |
| 鉴不上：`invalid api key id` / `invalid api key secret` / `has been revoked` | 明文抄错、被撤销、或 id:secret 拼错 | `grovefs apikey list` 对 id，必要时重铸 |
| 鉴不上但 key 有效 | scope 不含 MCP 平面 | 重铸为 `--for mcp`（或 `--for any`）；`--for mount` 的 key 开不了治理接口 |
| `no local metadata tree for view 'X' … LOCAL SQLite store only` | `list_view_tree` 只走本地 `meta/X.db`，远端/PG 后端不被遍历 | 在存储机上用 CLI，或本地留一份元数据（`view import`） |
| `no physical base tree … PostgreSQL-backed view cannot be forked` | fork 需要本地 SQLite 裸基 | 先 `grovefs --home H init/view create --view B`，或改用派生视图 |
| 写完 `.layout` 但挂载仍是旧形状 | 本地裸挂载挂载时解析一次 | 重挂；经 `meta-server` 的挂载则在 ≤2s 内自动换形 |
| `list_view_tree` 回 `truncated:true` | 命中 `depth`(6)/`max`(800) 界 | 用 `path` 下钻分段，或调大 `depth`/`max` |
| 发通知后一直等回包卡住 | 无 id 帧按设计不作答 | 只发不带等待；等应答只对带 id 的请求 |

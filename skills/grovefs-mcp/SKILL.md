---
name: grovefs-mcp
description: Drive GroveFS view governance through the `grovefs mcp` stdio JSON-RPC server (9 tools: list_views, get_view, list_view_tree, define_derived_view, alias_view, fork_view, remove_view, preview_rules, validate_definition). Use when a task mentions grovefs, 视图/view, derived lens, alias, fork, mount --view, layout rules (hide/only/map/link), reshaping a data listing, or API-key gating of the GroveFS MCP entrance. Covers handshake, framing, exact refusal texts, verification loops, and hard boundaries (control plane only — never mounts, never reads data bytes).
---

# GroveFS 视图治理（MCP 入口）

让智能体**通过接口自动定制数据列表**：查清某个存储里有哪些视图、它们的树真实长什么样，
然后发布/轮换/删除视图定义（只读透镜、可写公开别名、独立 fork）。

**这个入口是控制面**：只写 `<db-path>/meta/views/*.layout`（fork 另写 `meta/<name>.db` 并重建
`refs.db` 账本），**从不挂载、从不读对象字节**。挂载与数据读写属 CLI/运维面，不在这里。

## 何时用本 skill

- 用户提到 grovefs / 视图 / 派生视图 / 别名 / fork / `mount --view` / layout 规则；
- 需要「把某个数据列表改成另一种形状」而不碰文件本身；
- 需要在发布前确认一副定义会被接受还是被拒（干跑）；
- 需要按调用方把接口锁起来（API key + 审计）。

不适用：搬运字节（走 `rsync -a -H blocks/`）、跨机搬命名空间（走 `grovefs view export/import`，
**CLI 独有，MCP 没有这个工具**）、读写文件内容（走挂载）。

## 第一步：把服务拉起来

服务端由 MCP 客户端以子进程方式启动，stdin 进帧、stdout 出帧：

```bash
grovefs mcp --db-path <DB_PATH>          # 元数据根（含 meta/ 与 meta/views/）；缺省回退 --home
grovefs mcp --db-path <DB_PATH> --require-api-key   # 多调用方时上门（见「鉴权」）
```

MCP 客户端配置（通用 `mcpServers` 写法，Claude Desktop / IDE 智能体同构）：

```json
{ "mcpServers": { "grovefs": {
  "command": "grovefs",
  "args": ["mcp", "--db-path", "/abs/path/to/store"],
  "env": { "GROVEFS_MCP_API_KEY": "ID:SECRET" }
} } }
```

`env` 只在服务端加了 `--require-api-key` 时需要；导出它 = 整个服务器预认证。

**客户端没帮你管进程时**，用本目录的零依赖脚本自己发帧（见 `scripts/mcp_call.py`）：

```bash
python3 scripts/mcp_call.py --db-path <D> list                    # tools/list
python3 scripts/mcp_call.py --db-path <D> call list_views '{}'
python3 scripts/mcp_call.py --db-path <D> call define_derived_view '{"name":"release","base":"team","rules":"hide /photos/private"}'
python3 scripts/mcp_call.py --db-path <D> --api-key ID:SECRET list # 过 --require-api-key 的门
```

接线是否正确，先跑一次自检（用临时存储根，不碰真实数据）：

```bash
bash scripts/smoke.sh
```

## 协议要点（与实现逐字一致）

- 换行分隔的 JSON-RPC 2.0，协议版本 `2025-06-18`；`initialize` 会原样回声客户端的 `protocolVersion`。
- **治理失败不是协议错误**：按 MCP 惯例回在结果里 `{"result":{"content":[{"type":"text","text":"…"}],"isError":true}}`，
  所以智能体客户端能显示文案而不是崩掉。JSON-RPC 层只留给协议问题：非法帧 `-32700`（服务继续可用）、
  未知资源 URI `-32002`、鉴权失败 `-32001`。
- **无 id 的帧（通知）一律不作答**——发出后不要等回包（`notifications/initialized`、`notifications/cancelled`）。
- 每个工具都接受可选 `db_path`，覆盖服务器默认值（同一进程可服务多个存储根）。
- 缺参与未知工具走 `isError`，不是协议错误。

## 九个工具

| 工具 | 参数（★必填） | 用途 |
|---|---|---|
| `list_views` | `db_path` | 全店快照：物理视图 + 所有定义槽（kind/real_view/规则计数/mount 语义） |
| `get_view` | ★`name` | 单个视图全文，含**逐字 `definition` 规则**——改规则前先读它 |
| `list_view_tree` | `view` \| ★(`base`+`rules`)，加 `path`/`depth`(6)/`max`(800) | 渲染挂载实际所见的清单；也可**不落盘预览一副待发布规则**。`size` 是元数据字段不是字节数（目录 4096 / 文件 0 / 软链为 target 长度） |
| `define_derived_view` | ★`name` ★`base` ★`rules` | 发布/替换**只读**派生透镜 |
| `alias_view` | ★`name` ★`real` \| `retire` | 发布可写公开别名；`retire` 先写后删、无缝轮换 |
| `fork_view` | ★`name` ★`base` | 播种**独立元数据树**（结构可写、内容 EROFS、字节共享） |
| `remove_view` | ★`name` | 删定义槽（派生/别名/共享）；删 fork 连它自己的树一起删 |
| `preview_rules` | ★`rules` | 只校动词语法，报错带行号 |
| `validate_definition` | ★`name` ★`base` ★`rules` | 发布前把 define 的**全部治理守卫**走一遍，不写盘，回显确切拒绝文案 |

两个只读探查不碰数据字节：`list_view_tree` 以只读方式打开本地 `<db-path>/meta/<view>.db`、
经与挂载同一副 `LayoutMeta` 透镜渲染；`get_view` 只读定义文件文本。
**远端 / PG 后端的树不会被遍历**（`list_view_tree` 需要本地 `meta/<view>.db`）。
因为遍历只读元数据、不向数据面问对象字节，回包里的 `size` 就**不是文件大小**
（目录 `4096`、文件 `0`、软链是 target 的字符长度）；需要真实字节就挂载后 `stat`。

## 规则文本（四动词）

一行一个动词，`#` 行注释：

```
hide <src>          # 剪掉一棵子树
only <src>          # 只服务这棵子树
map  <src> <dst>    # 把条目改挂到虚拟显示路径（dst 父目录自动合成）
link <src> <dst>    # 同一批条目再加一个名字（多视图）
```

- `<src>`/`<dst>` 都是**绝对显示路径**（以 `/` 开头），**必须**取自 `list_view_tree` 的回包，不要猜。
- 规则里写 `base <x>` 行会被拒：基视图是 `base` 参数，不是规则内容。

## 黄金工作流（改形状时照这个顺序）

复制这份清单并逐步推进：

```
- [ ] 1. list_views                       → 有哪些物理视图/定义，别和现有名字撞车
- [ ] 2. get_view（若是在改已有透镜）      → 拿到逐字 definition，做「读—改—写」
- [ ] 3. list_view_tree {"view":"<base>"} → 取真实显示路径作为规则素材
- [ ] 4. 起草 rules 文本
- [ ] 5. preview_rules {"rules":"<草稿>"} → 语法/行号（快，只校解析器）
- [ ] 6. validate_definition {name,base,rules} → 全部守卫干跑，写盘前拿到确切判决
- [ ] 7. define_derived_view {name,base,rules} → 发布
- [ ] 8. list_view_tree {"view":"<新名>"} → 复核渲染出的形状与意图一致
```

第 6 步绝不能跳过：它返回的就是真实发布时会给出的逐字拒绝文案，且**什么都不写**。
第 8 步是反馈闭环——**只有它显示的形状符合意图才算完成**，否则回到第 4 步。

派生视图挂载一律只读（写要到基视图）。`map` 的 `<dst>` 父目录会按需合成（scaffold），但**纯合成目录里
不能创建/重命名条目**（`EPERM`）：先把某个条目 `map` 进去，那一层的目录才真实出现。

## 会被拒什么（确切文案，便于自查而不是重试猜测）

| 你想做的事 | 收到的 `isError` 文案要点 |
|---|---|
| 派生名 == 基名 | `derived view name must differ from its base` |
| 定义名占用物理 db 名 | `'<name>' already names a physical view db — pick another name` |
| 在别名/ fork 的名字上再定义 | `'<name>' is a pure alias — remove it first …` / `… is a fork …` |
| 别名指向别名 | `alias target '<real>' is itself an alias (-> '<x>') — chains are not supported` |
| 别名指向派生视图 | `alias target '<real>' is a derived (virtual) view — aliases map onto a physical view only` |
| 同一真实视图再加第二个公开名 | `real view '<real>' is already governed by alias '<old>' — one public name per real view; to rotate it gaplessly use … --retire` |
| `retire` 撤的不是同一视图的旧别名 | `--retire '<old>' currently aliases a different real view ('<r>'), not '<real>'` |
| fork 的基不是本地 SQLite 裸基 | `no physical base tree … \`fork\` copies a LOCAL SQLite tree … A PostgreSQL-backed view cannot be forked` |
| fork 的基自己已被治理 | `base '<base>' is governed by a definition (alias/derived/fork) — fork the physical base tree instead` |
| fork 名 == 基名 | `fork name must differ from its base` |
| 要 fork 的名字已有定义 | `view '<name>' already has a definition — remove it first (\`grovefs view rm --name <name>\`) before forking` |
| 删一个物理基视图名 | `no view definition '<name>' under <db>/meta/views`（`remove_view` 只管定义槽） |
| 名字不是一个安全路径元素 | `invalid view name '<name>' (must be one safe path element)` |

## 鉴权与审计（把接口 front 给多个调用方时）

- `grovefs mcp --require-api-key` 给**所有读写治理状态的方法**设门：`tools/list`、`tools/call`、
  `resources/list`、`resources/templates/list`、`resources/read`、`completion/complete`；
  `initialize` / `ping` / `prompts/*` / `logging/*` **始终开放**（让客户端能握手、取静态剧本、控制详谈程度）。
- 凭据：`grovefs apikey --db-path <D> create --label L --for mcp`（MCP 平面要求 scope 为 `mcp` 或 `any`；
  `--view`/`--ro` 是**挂载平面**的维度）。明文只打印一次，库里只存 SHA-256。
- 呈现方式：`initialize` 的 `params.apiKey = "<ID>:<SECRET>"`，或服务端导出 `GROVEFS_MCP_API_KEY`。
- 未鉴权访问受限方法 → JSON-RPC `-32001`，文案
  `authentication required: present a valid apiKey in the initialize params (or export GROVEFS_MCP_API_KEY for the server)`。
- 每一次 `tools/call` 与 `resources/read`（含失败、含未鉴权被拒）都进审计表：`grovefs apikey calls` 回读；
  `apikey revoke` 后该 key 立即鉴不上。

## 硬边界（不要试图绕过，也不要向用户这样承诺）

- **显示治理不是安全边界**：派生/别名/fork 只改变呈现的名字树；可见即可读，可读即可写回（fork 除外）。
  真正的硬边界是 token 与可撤销 API key（以及 OS 级的存储根权限）。
- 默认的 `grovefs mcp`（不加 `--require-api-key`）**信任能启动该进程的人**——等同于持有存储根的 OS 权限。
- fork 的内容只读是**客户端门**：直接说 wire 协议的 `data-server` 客户端绕过它。
- 本接口不挂载、不读数据字节、不建物理视图、不做 `view export/import`（那些是 CLI）。
- 改定义的**热生效只在 `meta-server` 入口**：活着的挂载在缓存 TTL 内（≤2s）自动换形，无需重挂；
  本地裸挂载（`--home` 直连）在挂载时解析一次定义，改了要重挂才见新形状。
- `list_view_tree` 的遍历有界：`depth` 默认 6、`max` 默认 800，截断时诚实回 `truncated:true`——
  大树用 `path` 下钻分段拿路径，不要指望一次读全，也不要把被截断的清单当成全树。

## 更多资料

- 完整方法面（资源 URI、提示剧本、补全、日志、错误码、CLI 对照表）：[reference.md](reference.md)
- 真实回显与端到端对话样例：[examples.md](examples.md)
- 零依赖 stdio 客户端：`scripts/mcp_call.py`
- 接线自检（临时存储根，跑完整工作流）：`scripts/smoke.sh`

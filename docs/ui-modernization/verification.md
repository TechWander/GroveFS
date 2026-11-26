# 控制台现代化改造 — 验证记录

日期：2026-11-25
环境：Linux，Chromium 1117（Playwright），PostgreSQL 12 @ 127.0.0.1:5433

## 1. 静态检查

| 检查 | 结果 |
|---|---|
| `npm run typecheck` | 通过 |
| `npm run lint` | 通过 |
| `npm run i18n:audit` | 通过（仅报 7 条既有 legacy 词条，无新增缺失） |
| `npm run build` | 通过，产物 `src/webui/app.css 44.37 kB`、`app.js 259.63 kB` |
| `cargo check` | 通过 |
| `cargo test` | 全部通过（含新增 3 个 `instance_report` 单测） |

## 2. SQLite 演示栈

启动：`meta-server`（127.0.0.1:18886/18885）+ `web`（127.0.0.1:18887），账号 `rootops`。
数据：3 个 base、1 个 fork、2 个账号、2 把密钥、1 条授权。

- 6 个页面在 1600 / 1024 / 768 / 375 / 320 下截图，`scrollWidth - clientWidth = 0`。
- 320px：6 个导航项以等宽网格单行显示，无横向溢出。
- 视图详情新增“宿主实例”区块；SQLite 下显示单一 `local` 逻辑实例。
- 跨页跳转实测：
  - Keys 行 view → Views，详情为同一视图；
  - Accounts 授权 base → Views，详情为该 base；
  - 账号抽屉“全部密钥…” → Keys，搜索框为账号名；
  - Audit 行 key_id → Keys，搜索框为密钥 id；
  - ViewDetail → Keys / Accounts 两条互链均落地。

## 3. 多实例 PostgreSQL 演示栈

注册表：`file:` 两个实例（`main` = grovefs，`shard2` = grovefs_shard2），`main` 为控制平面。
`web` 监听 127.0.0.1:18897；通过控制台在 `shard2` 上钉住创建 `inventory`、`ledger`。

- `store.info.storage`：
  - `backend = postgres`；
  - `instances = [{main, control:true, placedViews:148}, {shard2, control:false, placedViews:2}]`（148 为测试库中既有视图的置放记录）；
  - `placement` 共 150 条；
  - `instanceError = null`。
- 实例卡：控制平面徽标、完整含密码 URI（默认展示）、隐藏/显示切换、复制、置放数、视图清单前 8 项 + `+N`、进入过滤列表、在该实例新建 base。
- 隐藏切换后 URI 变为掩码（实测 masked 节点数 = 1）。
- “查看这些视图”落地 `#/views`，列表仅 2 行（`shard2` 的 inventory / ledger）。
- 详情页“宿主实例”显示 `shard2`、控制平面“否”、完整 URI `postgres://grove:grovepw@127.0.0.1:5433/grovefs_shard2`。
- “在此新建 base”表单预置 `pg_instance=shard2`（实测输入值）。
- 深链 `#/views?instance=shard2` 刷新后仍为 2 行。
- 7 档宽度（1600/1280/1024/820/768/414/320）遍历 6 页：无横向溢出，无 pageerror。

截图目录：
- `/tmp/gfsui/run/shots/`
- `/tmp/gfsui/pgrun/shots/`

## 4. 已知边界

- `?view=` / `?instance=` 在**同一页面内**改 hash 不会重新挂载页面，因此不会再次消费 query；刷新或跨页跳转均生效。控制台自身的跳转按钮直接写 filter store，不依赖 query。
- 单实例 PostgreSQL / SQLite / treedb 均只显示一个逻辑实例；多实例 `placement` 缺失的视图显示 `hash` 徽标，不猜测实例。
- `store.info` 的实例 URI 为完整凭据，仅 Super 可读，未写入审计、URL 或 localStorage。

## 5. 实例治理网页化（2026-11-26 追加）

本节覆盖上一批只读面之后的三项工作：PG 实例增删改查从 `instances.conf` 迁至网页端、
treedb 多实例清单管理、视图页长列表布局修复，以及一轮视觉统一。它把 §2 中
「不改变任何写路径语义」的范围**扩展为受管注册表的治理写面**。

### 5.1 后端治理 op（见 `ecd50ea`、`b380c98`）

- serve() 首启收养内联 spec → `file:<base>/instances.conf`，首实例命名 `control`（`control:true` 语义一致）。
- 新增 Super op：`storage.{add,update,remove,preview,set_control,relocate}`，三清单同步
  （`gates` / dispatch / `ALLOWED_OPS`），`every_routed_op_has_an_access_rule` 通过。
- 删除守卫：非空（仍有置放视图）拒绝；控制平面实例须先 `set_control` 交接；
  证据缺失（placement 读不到）拒绝；`storage.preview` 输出受影响视图列表、每视图
  node / distinct-ref 计数与 `grovefs_refs` 一致性扫描（`refs.instances[].{holderRows,zeroRows,missingViews}` + `refs.unverified`）。
- 控制面交接两阶段：`set_control` 只暂存 `@control-pending=` 指令，`apply_pending_control` 在下次启动、
  listener 建立前迁移并轮转首条 —— UI 如实标注「需重启」。
- treedb：注册表为**清单非路由器**（每 meta-server 只路由一个 `--meta-store` 地址），卡片恒
  `control:false`、匹配配置地址者为 `serving`；`set_control`/`relocate` 如实拒绝；删除守卫= serving 拒删、
  非空拒删、证据缺失拒删；`#[cfg(feature="treedb")]` 门控，默认档 / native 档 / all-features 均编译。

### 5.2 前端（见 `0ce6b6e`、`175f807`、`67e0540`）

- `types.ts`/`ops.ts` 对齐后端真实 JSON：PG `views` 为对象数组 + `placedViews` 名称表 + `refs`；
  treedb `views` 为视图名字符串数组（不可达时 `null`）。
- `InstanceCard.vue` PG+treedb 共用网格（`auto-fit, minmax(300px,1fr)`），如实区分
  control / serving / experimental 徽标；写动词同时受 `managed` 与 `session.can` 双重门控。
- 四个守卫 dialog：`EditUriModal`、`RemovePreviewModal`（preview→confirm，`canDelete` 为假禁用删除并显示
  服务端原句 `blockReason`）、`RelocateModal`（按视图逐个 move）、`SetControlModal`（两阶段+重启提示）；
  `AddInstancePanel` 在托管注册表下网页端登记实例，服务端拨号校验。非托管时页面降级为只读并说明原因。
- 视图页长列表：`#view-list` 有界内部滚动（`min(66vh,34rem)` + `overscroll-behavior:contain`），>50 视图
  不再撑破页面；≤768px 折叠按钮（`.list-toggle`）+ `.is-collapsed` 收起为抽屉，宽屏无影响（class 切换，无内联 style）。
- 滚动条改用 `--line-strong`/`--fg-faint` 令牌，修复深色档此前遗留的暖米色滑条。

### 5.3 静态检查

| 检查 | 结果 |
|---|---|
| `npm run typecheck` | 通过 |
| `npm run lint` | 通过 |
| `npm run i18n:audit` | 通过（`t()` 字符串 0 缺失；仍报既有 legacy 词条若干） |
| `npm run build` | 通过，产物 `src/webui/app.css 49.87 kB`、`app.js 292.01 kB` |
| `cargo test --lib` | 通过（293 passed / 0 failed，含 storage op ACL 一致性与 treedb registry 单测） |

### 5.4 待补真机项

本轮以静态门 + 单元/集成测试为验收；§2/§3 的多实例 PG 与 treedb 演示栈真机走查（CRUD 全链路、
删除守卫拒绝文案、preview 一致性数字、>50 视图滚动与 ≤768px 折叠截图）建议按 §4 验证计划第 8–12
项在换版后复跑一遍，换版校验须遵循「重载文档 + 校验运行中标志物」而非仅 fetch 资源新鲜度。

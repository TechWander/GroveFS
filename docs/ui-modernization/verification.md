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

## 6. 引擎判据、审计模块与视觉重设计（2026-11-26 第二次追加）

对应规格 §7。量具：无头 Chrome 147 + CDP，脚本 `.tmpedit/rd_verify.mjs`（真实输入登录、
逐页截图、`getComputedStyle` 探活、DOM 序与文本扫描断言）。两档部署各自跑一遍：
现网 treedb 档（fdb4 :8787，截图 `.tmpedit/shots-deployed/`）与临时 sqlite 档
（本机 :15412，截图 `.tmpedit/shots-sqlite-arm/`）。

### 6.1 静态门

| 检查 | 结果 |
|---|---|
| `npm run typecheck` | 通过 |
| `npm run lint` | 通过 |
| `npm run i18n:audit` | 通过（`t()` 492 串 0 缺失；退出码 0；仍列 7 条 legacy 词条，属旧版页面遗留） |
| `npm run build` | 通过，201 modules／3.15s；`src/webui/`：`app.css 58.57 kB`（gzip 11.76）、`app.js 296.78 kB`（gzip 104.46）、`index.html 1.61 kB` |
| `cargo test --release --features treedb --lib` | 304 passed / 0 failed（含 treedb registry 8 条，其中 `repair_splits_duplicated_names` 为本轮新增） |

### 6.2 换版与产物一致性

- 本机 release 二进制 md5 `648709bb3cc633f3fed6d684983a156c`；gfs1／gfs2／gfs-client／gfs-fdb4
  四机 `/usr/local/bin/grovefs` 已全部对齐（`fdb4_upgrade.sh status` 复核）。
- gfs1 根盘已不再满（实测 86%、余 343M），`/usr/local/bin/grovefs` 为普通实文件而非
  `/data/gfs1/grovefs.bin` 的软链；`fdb4_upgrade.sh push` 的 gfs1 分支只写后者并回显前者，
  所以那一行报 `9416687f…` 是脚本读错了文件、不是安装失败。已手工 `tee`+`mv` 补装，
  两个路径现在都是 `648709bb…`。
- fdb4 栈按 `/data/fdb/stack-start.treedb.sh` 重启：meta-server pid=428661、web pid=428684，
  manager pid=415658 全程未动，treedb 引擎（7777/7778）未重启；50055/50110/50111/8787/7777/7778 均在听。
- 换版校验按「重载文档 + 校验运行标志物」：从 :8787 取回的三份产物与本机 `src/webui/`
  逐字节相同 —— `index.html 1c7a8e6b…`、`app.js 9e09794a…`、`app.css 470350b2…`。

### 6.3 事故与恢复（如实记录）

本轮第一次重启误用了 `.deploy/restart_fdb4.sh`：该脚本硬编码 `$BASE/stack-start.sh`，而那份
launcher 是 2026-11-25 SQLite 档时期的产物（`--meta-dir $BASE/sqlite`、不传 `--meta-store`），
于是把在跑的 treedb 档 meta-server 换成了 sqlite 档，web 随即以
`authentication failed: unknown api key id` 拒起（`web-gw.key` 属于 treedb 控制面库）。
影响面：约 2 分钟内 sqlite 档 meta-server 起来过又停掉；treedb 引擎进程与 manager 全程未动，
数据面、视图树、审计台账均未受影响，无任何写入。随后按正确入口重启并复核。

**教训**：fdb4 换版重启的唯一入口是 `.build-bin/fdb4_upgrade.sh restart`（它从
`/proc/<pid>/cmdline` 回读原命令行，不会改档）；`.deploy/restart_fdb4.sh` 已不适用于当前拓扑。

### 6.4 现网 treedb 档真机断言

| 断言 | 结果 |
|---|---|
| 树引擎与控制面分开呈现 | Overview 卡片「树引擎 treedb」＋「树存于 treedb 服务器；控制面在本地 SQLite」，本部署区两栏 `treedb` / `sqlite · /data/fdb/treedb/ctrl (local files)` ✔ |
| 实例名自愈 | 注册表由两条 `primary` 变为 `primary = 127.0.0.1:7778` / `primary-2 = 127.0.0.1:7777`；meta.log 第 1036 行打出 `treedb registry renamed the duplicated instance 'primary' to 'primary-2' — instances are addressed by name` ✔ |
| 徽标不再冒充控制面 | 两张 treedb 卡徽标均为「服务中」+「实验性」，无「控制面」；卡下保留一句「控制面：本地 SQLite」 ✔ |
| 审计页 DOM 顺序 | `page-head → retbar is-edit → bar → filterbar → tbl-wrap → pager → muted`，retbar/pager 均存在，默认每页 20 行 ✔ |
| 分页翻页 | 点页码 2 后首行时间由 `11-26 20:33:31` 变为 `11-26 20:29:10` ✔ |
| 每页条数切换 | 20→50 后表体 50 行；因全部装入，`.pager` 按 `hit.length > size` 正确卸载 ✔ |
| 设计 token 实渲染 | body 画布 = 账簿细线 `repeating-linear-gradient` + 两层 radial bloom；header 68px + 投影；活动导航为 pine 三段渐变实底 + `rgb(242,251,246)` 文字；卡片 16px 圆角 + 白→`#fbfcfa` 渐变表面 + 三层墨色阴影 + 1px 内高光 ✔ |
| 三声字体 | `.card:not(.text) .n` = Iowan Old Style 衬线栈 / 41.6px（2.6rem）；`.card.text .n` 按设计降到 system-ui / 19px；`th` 渐变带 + 1.32px 字距 ✔ |
| 深色档 | 截图 `06_audit_dark` 无残留浅色面 ✔ |
| 窄屏 430px | 截图 `08_audit_narrow`：retbar 折叠为单列，无横向溢出 ✔ |
| 控制台错误 | 无 ✔ |

### 6.5 sqlite 档真机断言（任务 1 的反向验证）

用同一二进制在本机拉起一个纯 sqlite 档栈（`meta-server --meta-dir /tmp/gfs-arm/srv
--control-listen 127.0.0.1:15411` + `web --listen 127.0.0.1:15412`，一个基视图），
逐页扫描可见文本中的 `treedb`：

| 页面 | `treedb` 出现次数 | 引擎词频 |
|---|---|---|
| 总览 | 0 | sqlite ×3 |
| 视图 | 0 | — |
| 存储 | 0 | sqlite ×2（小标题为「嵌入式 SQLite」，不渲染实例卡） |
| 密钥 | 0 | — |
| 账号 | 0 | — |
| 审计 | 0 | — |

Overview 卡片为「树引擎 sqlite」+「元数据与内容同库」，本部署区两栏
`sqlite · one db file per view under <base>/meta/` 与 `sqlite · /tmp/gfs-arm/srv (local files)`。
审计页 retbar 存在、pager 缺席（17 行 ≤ 20，属正确语义）。验证后临时栈已停、
`/tmp/gfs-arm` 已清除，现网不受影响。


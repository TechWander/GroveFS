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

## 7. 部署后四条反馈（2026-11-26 第三次追加）

对应规格 §8 与 ADR-0003。量具同上：无头 Chrome + CDP，脚本 `.tmpedit/r3_verify.mjs`
（真实输入登录、点语言按钮切 EN/ZH、逐页截图、`getComputedStyle` 与 DOM 序断言）加
`.tmpedit/r3_narrow.mjs`（窄屏溢出）；驱动入口是本机隧道 `127.0.0.1:18787 → gfs-fdb4:8787`，
截图落在 `.tmpedit/shots-r3/`（15 张）。

### 7.1 静态门

| 检查 | 结果 |
|---|---|
| `npm run typecheck` | 通过 |
| `npm run lint` | 通过 |
| `npm run i18n:audit` | `zh.ts entries 633`／`strings sent to t(): 526`／`of which missing: 0`／打印 `no missing keys`；另列 `legacy entries zh.ts lacks: 8` |
| `npm run build` | 通过；`src/webui/`：`app.css 59.55 kB`、`app.js 309.82 kB`、`index.html 1.61 kB`。同源码连跑两次三份产物 md5 全等，构建可复现（产物要提交，这条必须成立） |
| `cargo test --features treedb,web --lib` | 305 passed / 0 failed（`--lib` 显式指定，避免集成测试缺席造成假绿） |

`legacy entries zh.ts lacks: 8` 的口径要说清：该数字只是把新表与旧手写控制台
`src/webui/app.js` 里的 `const I18N_ZH`（392 条）作集合差，不判断旧词条是否仍被页面使用；
真正的「用到却没译」由 `missing: 0` 度量。这 8 条里 7 条是旧控制台自己的长句（新页面不再发这些串），
1 条是本轮**主动删除**的 `'rows to load': '载入行数'` —— 那个标签误导操作员（见 §7.3）。

上一轮记的「492 串 0 缺失」是本轮改动前的基线，本轮增至 526 串（其中一条被 §7.5 的
banner 改写从模板里挪进了常量，仍计数）：新增的区块标题、落位预测句与行动化拒绝文案都进了表。

### 7.2 后端与 API 取证

在 gfs-fdb4 上直接打控制面（`/tmp/probe_round3.sh`、`/tmp/probe_grant3.sh`，登录字段是
`account` 而非 `user`）：

| 事实 | 实测 |
|---|---|
| 两个引擎名确实是两件事 | `store.info` → `backend:"sqlite"`、`metadataBackend:"treedb"`；顶层 `instances:[]`，实例清单在 `storage.instances`，每行带 `serving` |
| 落位规则归服务端回答 | `view.plan {"view":"probe-r3-a"}` → `{"addr":"127.0.0.1:7778","instance":"primary","rule":"name hash","exists":false,"serving":2}` |
| 已有名会被如实预告 | `view.plan {"view":"test1100"}` → `exists:true`、`instance:"primary"` |
| `instance` 真能钉位 | `view.create {"view":"probe-r3-a","instance":"primary-2"}` → `created BASE view 'probe-r3-a' in treedb on instance 'primary-2' at 127.0.0.1:7777`，再 `view.plan` 复核落点未变 |
| 授权修复已上线 | `grant.add bob→test1100` 成功返回 `{"account":"bob","asNamed":true,"base":"test1100","grantedBy":"rootops"}` —— 这正是用户报的失败用例 |
| 真幽灵视图仍报原句 | `grant.add bob→ghost-nope` → `view 'ghost-nope' has neither a definition nor a tree`（前端映射需要它原样透传） |

`test1100` 经 `physical.list` 证实一直真实存在（`["alpha","bravo","charlie","cxrt","echo","fk1",
"india","test110","test1100"]`），报错不是幽灵视图，而是 §8.5 记的 `resolve_base` 判定路径缺陷。
探针视图 `probe-r3-a`、`uictl-1` 验证后均已 `view rm --base` 删除，收尾复核注册表命中 0。

### 7.3 换版与产物一致性

- release 二进制 md5 `d6194b67903de4cf7b75c3803ec02e81`；gfs1／gfs2／gfs-client／gfs-fdb4
  四机 `/usr/local/bin/grovefs` 全部对齐。
- `fdb4_upgrade.sh push` 的 gfs1 分支仍只写 `/data/gfs1/grovefs.bin` 却回显
  `/usr/local/bin/grovefs` 的校验和——本轮再次手工补装并复核，两个路径同为 `d6194b67…`
  （脚本自身的这一处口径问题未改，见 §6.2 同样记录；gfs1 登录用户不是 root，补装须走 `sudo`）。
- 重启走 `.build-bin/fdb4_upgrade.sh restart`：meta-server pid=434838（50110/50111）、
  web pid=434980（8787），manager pid=415658 未动（其 `exe` 显示 `(deleted)`，即仍在跑换版前的
  inode，与 §6.3 的约定一致：manager 不归本脚本重启），treedb 引擎 7777/7778 未重启；
  meta-server 命令行仍是
  `--meta-dir /data/fdb/treedb/ctrl --meta-store treedb://127.0.0.1:7777,127.0.0.1:7778`（档未变）。
- 换版校验按「重载文档 + 校验运行标志物」：从 :8787 取回的三份产物与本机 `src/webui/`
  逐字节相同 —— `index.html 1c7a8e6b…`、`app.js cb37a947…`、`app.css 3213d05b…`。
- CSP 与产物名复核：`index.html` 无 `style=` 属性、无 `<style>` 块，仍是固定名
  `index.html` / `app.js` / `app.css`（`include_str!` 内嵌，故改前端必须重装二进制）。

### 7.4 真机断言（47 项，全绿）

| 反馈 | 断言 | 实测 |
|---|---|---|
| 1a | 概览用一句话解释两个引擎名的关系 | `.engine-note` 存在，EN 与 ZH 各读到一次完整句；ZH 为「树住在 treedb 服务器里；控制面 — 定义、凭据、账号和这条审计轨迹 — 是你正在通信的 meta-server 上的本地 SQLite。本页出现两个引擎名，是对两个不同问题各自给出的答案…」 ✔ |
| 1a | 不是并列两枚 chip 了事 | 小标题点名「树引擎」，`tree engine` 命中 ✔ |
| 1b | 创建表单暴露实例下拉 | `#new-base-instance` 存在，`option` 恰 3 条（auto + primary + primary-2） ✔ |
| 1b | 「不选默认落哪台」由服务端回答 | 输入 `uictl-1` 后 auto 项变为 `auto — primary-2 (this name hashes there)` ✔ |
| 1b | 已占用的名在调用前就拦下 | 输入 `test1100` 后 `#view-new .err` = `a tree named test1100 already exists on primary — Create would be refused, so pick another name` ✔ |
| 1b | 钉位真的生效（UI 路径，非 curl） | 选 `primary-2` 点「创建」→ toast `created BASE view 'uictl-1' in treedb on instance 'primary-2' at 127.0.0.1:7777` ✔ |
| 1c | treedb 卡如实标注无需凭据 | `.inst-uri-label .cred.none` 两枚，文本 `no credentials|no credentials`（ZH：`无需凭据|无需凭据`） ✔ |
| 1c | 说明认证住在何处，避免误以为漏填 | 实例卡正文含 `GROVEFS_TREEDB_KEY_FILE`；注册表单 hint 同句 ✔ |
| 1c | 页面上不存在空的必填凭据框 | 存储页 `input[type=password]` 计数 0 ✔ |
| 2 | 「载入行数」不在保留策略条里 | `.retbar select` 计数 0 ✔ |
| 2 | 上限控件迁入分页条并改名 | `.pager .pager-fetch select` 存在，标签 `fetched from server`（ZH：`服务端读取行数`） ✔ |
| 2 | 提高上限确实多发一次请求 | 50→200 后 `.pager-note` = `the server was asked for 200 rows and returned 200, so older calls are almost certainly still there — raise “fetched from server” to read further back` ✔ |
| 3 | 拒绝就地显示、不是一闪而过的 toast | 点「授予」后 `.refusal` 出现，`.toast` 计数 0 ✔ |
| 3 | 第一行给行动、第二行留服务器原句 | head：`there is no view named uictl-1 on this server — create it first, or delegate one that the Views page lists`；detail：`view 'uictl-1' has neither a definition nor a tree`；并附「打开该页」链接 ✔ |
| 4 | 区块节奏 28px | 首个 `.bar` computed `margin-top = 28px`，`.page-head` `margin-bottom = 28px` ✔ |
| 4 | 概览有跨页引导 | `.flow li` = 4（ZH 渲染为「创建基准视图 → 授权给某个账号 → 签发挂载密钥 → 查看发生了什么」） ✔ |
| 4 | 明细区有层级而非一片卡片 | `.detail .det-h` = 2 ✔ |
| 4 | 密钥页参照区排在数据之后 | 文档序 `.bar h2`（`你可见的密钥`）早于 `.help` ✔ |
| 4 | 窄屏无横向溢出（另跑 9 项） | 420／640／820px × overview／views／audit 共 9 格，`scrollWidth − clientWidth = 0px` ✔ |
| — | 控制台错误 | 页面侧 0；唯一一条是首屏 `GET /api/session` 的 401 —— 无会话时的探针，
其存在本身就是「先问一次再决定要不要摆登录卡」的实现，见 §7.6 ✔ |

### 7.5 只有截图能抓到的缺陷：服务端句子顶穿了前端字典

真机截图（`.tmpedit/shots-r3/08_storage_zh.png`）暴露出一处静态门与断言脚本都判不出来的问题：
存储页顶部那枚「实验性」横幅在中文界面下仍是整句英文。根因不在译文，而在措辞的归属：
`store.info` 的 treedb 臂把这句话当数据发了出来（`src/remote/meta/server_ext.rs` 的
`routingNote`），前端写的是 `storage?.routingNote ?? t('…')` —— 服务端字段有值时，
字典里那条译文永远走不到。i18n 门看不见它，因为模板里确实写了 `t('…')`；断言也看不见它，
因为那条断言查的是实例卡徽标。

修法按本仓既有规矩：**服务端持有事实（这台部署有几个在服地址），控制台持有操作员读到的句子**。
`StoragePage.vue` 现在从 `routingNote` 里取出数字，用字典键
`treedb routes new trees by name hash across ${n} serving address(es); other entries are inventory, not placement targets` 渲染；句式一旦对不上就退回服务端原文，
而不是编一句话出来。zh.ts 补进对应译文，`missing` 仍为 0。复验断言：

| 断言 | 实测 |
|---|---|
| 中文界面下横幅已译，数字仍来自服务端 | `.exp-banner` = 「实验性 treedb 按名称哈希把新树路由到 2 个在服地址；其余条目只是清单，不是落位目标」 ✔ |

**教训**：凡是「后端 JSON 字段直接当文案渲染」的地方，都是字典的盲区；新增这类字段时要一并
登记译文并写明回退路径，光看 `t()` 调用次数不足以证明页面是双语的。

### 7.6 唯一一条 console error 的定性

`47 项`跑完只记到一条：`network/error: … 401 (Unauthorized)`。用清掉 cookie 的干净 profile
单独取证（`.tmpedit/p401c.mjs`，为此给 `cdp.mjs` 加了首屏 `httpErrors` 记录）：

```
boot-phase http errors: ["401 http://127.0.0.1:18787/api/session"]
sign-in card present: true
```

即它是应用启动时「现在这个浏览器还会话在吗」的那一次探测：无会话 → 401 → 摆登录卡，
属预期路径，不是任何一页的数据请求失败。页面侧 console error 为 0。

### 7.7 量具自身的三处纠正（如实记录）

首轮 `RUN_EXIT=1` 的失败全在脚本、不在被测页面，值得记下来避免复发：

1. 语言切换原先靠 `page.navigate(BASE + '/#/')` —— 同文档 hash 跳转不触发模块重载，
   于是「切到 EN」实际什么都没做，英文正则断言全判 FAIL，而页面输出的中文恰恰证明译文是对的。
   现在改成点真实的 `.lang-btn` 并以 `localStorage['gfs.lang']` 校验到位（`t()` 走的是响应式
   ref，本就无需 reload）。
2. 点击 helper 原先「先 `scrollIntoView`、再另发一次 `querySelector` 量盒子」，中间隔了一次
   Vue 重渲染，`#r3-grant` 已不在，`getBoundingClientRect()` 抛 null。合并成一次 evaluate 后消失。
   同时删掉了 `#page-accounts` 这类页面根本不存在的外壳 id（页面由 `PageHead` 起头，不包 id）。
3. 跨主机清理走 `execSync` 嵌套引号会静默失败（首轮就漏下了 `uictl-1`）。改为先在 fdb4 落
   `/tmp/rmview.sh`、只收一个 argv，脚本并断言 `removed base view 'uictl-1'` 出现后才继续。

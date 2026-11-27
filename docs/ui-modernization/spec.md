# GroveFS 控制台现代化改造规格（Spec）

状态：待批准
范围：`console/`（Vue3 + Pinia + hash router）与 `src/pg_store.rs`、`src/remote/meta/server_ext.rs` 的只读信息面
不改变：CSP（`default-src 'none'`）、固定产物名（`src/webui/{index.html,app.js,app.css,favicon.svg}`）、CLI 行为、任何写路径语义

## 1. 目标与验收总则

1. 视觉体系现代化：中性画布 + 白色 surface + 墨色文字 + 松绿 accent；sans 标题（系统 UI、紧字距）、mono 数据；10–12px 圆角、发丝边框、180–220ms 克制动效。
2. 业务关联闭环：视图 ↔ 宿主实例 ↔ 绑定密钥 ↔ 被授权账号 ↔ 审计记录，任意方向最多两次点击互达。
3. 数据库差异化：SQLite / 单实例 PG / 多实例 PG / treedb 四种形态各有专属信息架构与措辞，不共用一套卡片硬套。
4. 多 PG 实例隔离：每个实例独立卡片、独立视图清单、独立“在该实例新建 base”入口、独立过滤视图列表。
5. 响应式：320 / 375 / 414 / 600 / 768 / 1024 / 1280 / 1600 八档无横向溢出。
6. 交互反馈：所有异步动作有 loading / 成功 / 失败三态；键盘可达；`prefers-reduced-motion` 下停用位移与循环动画。
7. 无功能退化：现有 28 个 op 的调用参数与解析不变；`cargo test` 与前端四项检查全绿。

## 2. 后端：实例与置放信息（只读）

### 2.1 `src/pg_store.rs` 新增

```rust
pub struct InstanceSummary {
    pub name: String,          // registry 名 / pg1..pgN
    pub uri: String,           // 完整连接串（含密码），仅供 Super 的 store.info
    pub control: bool,         // 是否控制平面（首个实例）
    pub placed_views: Option<i64>, // 单实例或读不到时为 None
}

pub struct InstanceReport {
    pub instances: Vec<InstanceSummary>,      // 顺序 = registry 顺序
    pub placement: HashMap<String, String>,   // view -> instance
    pub error: Option<String>,                // 置放表读取失败的原文，不吞掉
}

/// 只解析 + 只读控制实例；绝不建表、绝不写。
pub fn instance_report(spec: &str) -> Result<InstanceReport, String>
```

规则：

* `expand_store_spec` 失败 → `Err`（沿用原错误文案）。
* 单实例：`placed_views = None`、`placement` 为空、`error = None`，不发任何 SQL（保持零额外开销）。
* 多实例：向控制实例发起**一次**连接，执行
  `SELECT view, instance FROM grovefs_control.view_placement`。
  * 表不存在（`is_undefined_table`）→ 空 map、`error = None`。
  * 连接/查询失败 → `placement` 为空、`placed_views = None`、`error = Some(原文)`，函数本身仍 `Ok`。
* `placed_views` = 按 `placement` 聚合计数（单实例不填）。
* 不新增写路径；`placement_counts` 保持私有不变。

### 2.2 `src/remote/meta/server_ext.rs` → `store.info`

在既有字段后新增（仅 `Super` 分支已满足，不改鉴权）：

```json
"storage": {
  "backend": "sqlite" | "postgres" | "treedb",
  "instances": [{"name":"pg1","control":true,"uri":"postgres://grove:secret@127.0.0.1:5433/grovefs","placedViews":3}],
  "placement": {"viewA":"pg1"},
  "instanceError": null
}
```

* `StoreKind::Pg(spec)`：调用 `instance_report(spec)`；`Err` 时退化为单条 `instances`（name = `"postgres"`、control = true、uri = **原始 spec**、placedViews = null）+ `instanceError` 原文，保证控制台不因存储探测失败而整页报错。
* 凭据可见性：`store.info` 已是 `Super` 专属，操作员需要在这里核对真实连接串，因此 `uri` **原样返回，不做 `redact_uri`**；`redact_uri` 仍只用于日志、错误文案与 CLI 输出。
* `StoreKind::Sqlite`：单条 `{name:"local", control:true, uri:null, placedViews:null, backend:"sqlite"}`，`placement` 空。这是“一个逻辑实例”，不伪造多实例。
* `StoreKind::Treedb(addr)`：单条 `{name:"treedb", control:true, uri:"treedb://host:port", placedViews:null}`。
* `view.stats` 增加 `"instance": <String|null>`：多实例 PG 下用 `resolve_view_home` 的成功结果填充；单实例填该实例名；解析失败填 null，**不得**使 `view.stats` 整体失败。

### 2.3 测试

`src/pg_store.rs` 单元测试（不依赖真实 PG）：

* `instance_report` 对 `file:` registry（两行假 URI）返回 2 条、首条 `control = true`、uri **完整保留密码**。
* 单条 `postgres://u:p@h/db` 返回 1 条、`placed_views = None`、`placement` 空。
* 缺文件 registry → `Err` 且文案包含路径。

`server_ext` 现有测试若覆盖 `store.info` 字段，补充 `storage.backend == "sqlite"` 与 `instances.len() == 1`。

## 3. 前端：信息架构与页面

### 3.1 路由与导航

* `console/src/router/index.ts`：`PAGES` 增加
  `{ name:'storage', path:'/storage', label:'Storage', icon:'storage', component: StoragePage }`，
  位置在 `views` 与 `keys` 之间；`PageMeta.icon` 联合类型加 `'storage'`。
* `console/src/components/chrome/NavIcon.vue`：新增 `storage` 分支（分层数据库圆柱线稿，24×24，`currentColor`，2px 圆角）。
* `console/src/i18n/zh.ts` + `i18n/index.ts`：新增 `Storage`、`Storage & instances`、实例相关词条（约 25 条，附中文）。
* 导航承载 6 项：≥1024px 单行；768–1023px 隐藏序号、保留图标+文字；<768px 导航横向滚动、`scroll-snap`，当前项自动滚入视野；<480px 仅图标+`aria-label`，`title` 提示。

### 3.2 新增 `console/src/pages/StoragePage.vue`

数据源：`usePageLoad('store.info')`（现有 composable）+ `physical.list` + `def.list`。

结构（按 `storage.backend` 分支）：

1. **头部**：eyebrow（`Metadata backend`）、标题、`describe` 原文、后端徽标。
2. **SQLite 专属**：单张“嵌入式存储”面板，展示 `base` 路径、视图数、定义数、`residentEngines/residentViews`，并说明“每视图一个本地库、HA 由 VACUUM INTO 快照承担”。不出现实例网格。
3. **单实例 PG**：单张实例卡（名称、**完整 URI（含密码）**、控制平面徽标、视图总数、置放数），下方“连接与容量”面板（resident 引擎/视图、会话数）。
4. **多实例 PG**：实例网格（`auto-fit, minmax(300px,1fr)`），每卡：
   * 名称 + `Control plane` 徽标（首个）+ 置放视图数；
   * **完整连接 URI（含真实密码，默认可见）**：`MonoText` 展示（可换行/横向滚动，绝不截断密码），配复制按钮；旁边标注“含凭据”与一个“隐藏”切换（仅本页会话内生效，默认展示）；
   * 该实例视图名列表（由 `status.placement` 反查 + 未置放归入“未记录置放 / hash 路由”折叠区），点击进入 `/views?instance=<name>`；
   * 主操作“在此实例新建 base” → 打开 `NewBasePanel` 并预置 `pg_instance=<name>`；
   * `instanceError` 存在时在其上方显示警示条（原文，不重写）。
5. **treedb**：单卡“实验性树引擎”，展示地址、说明“控制平面仍在本地 SQLite”，不显示实例网格。
6. 底部“置放规则”说明：最小负载选新家、`--pg-instance` 可钉住、跨实例 fork/graft 被拒绝。

空态/错误态：无视图时卡片内显示“尚无视图落在此实例”并给新建入口；`store.info` 失败时整页沿用现有 `PageLoad` 错误态 + 重试。

### 3.3 视图页（`ViewsPage.vue` / `ViewList.vue` / `ViewDetail.vue`）

* `stores/filters.ts`：新增 `instance` 维度，与现有 query 读写一致；`/views?instance=pg2` 深链生效，筛选条显示可清除的实例 chip。
* `ViewList`：新增“实例”列（≥900px 显示；窄屏并入名称下方副行），值取 `store.info.storage.placement[name]`；未记录时：
  * 单实例 → 该实例名；
  * 多实例 → `hash` 徽标，`title` 说明“未记录置放，按名称哈希路由”。
* `ViewDetail`：新增 “Home instance” 区块（名称 + 控制平面标记 + 完整 URI，含密码，可复制），数据优先取 `view.stats.instance`；同时增加两条互链：“绑定密钥（N）→ `/keys?view=<name>`”、“相关账号 → `/accounts?view=<name>`”。
* `NewBasePanel`：接受 `pgInstance?: string`；提交 `view.create` 时附 `pg_instance`；表单内显示“目标实例”只读徽标（从 Storage 页进入时）。

### 3.4 交叉跳转矩阵（必须全部实现）

| 起点                         | 目标       | 携带筛选              |
| -------------------------- | -------- | ----------------- |
| Storage 实例卡                | Views    | `instance=<name>` |
| Views 行/详情                 | Keys     | `view=<name>`     |
| Views 详情                   | Accounts | `view=<name>`     |
| Keys 行的 view               | Views    | `view=<name>`     |
| Keys 详情                    | Accounts | `owner=<account>` |
| Accounts 授权行 base\_view    | Views    | `view=<base>`     |
| Accounts 详情                | Keys     | `owner=<account>` |
| Audit 行 key\_label/key\_id | Keys     | `q=<label>`       |
| Audit 行 detail 中 view（存在时） | Views    | `view=<name>`     |

实现方式：`useNav.ts` 增加 `goTo(page, query)`（`router.push({path, query})`）；接收页在 `onMounted` 读取 query 写入对应 filter store，并即时清除 query（`router.replace`）避免刷新粘连。

### 3.5 设计系统（`console/src/styles/console.css` 重写 tokens + 组件适配）

> 本节是 2026-11-22 那一轮的记录，其 token 数值与字体决策已被 §7.3（2026-11-26 视觉再设计）
> 整体取代：画布/文字/强调的具体色值、圆角、阴影、动效时长都已变更，`--font-display`
> 也从系统 sans 回到衬线栈。以代码为准的读者请直接读 §7.3。

* 画布：`--bg:#f4f6f2`；surface `--bg-2:#ffffff`；`--bg-subtle:#f7f9f6`；`--bg-sunken:#ecefe9`；`--line:#e2e6df`；`--line-soft:#eef1ec`；`--line-strong:#cfd6cc`。
* 文字：`--fg:#101c16`、`--fg-dim:#55635a`、`--fg-faint:#6d7a71`（纸面对比 ≥4.5:1）。
* 强调：`--accent:#0f6b4f`、`--accent-hover:#0b5740`、`--accent-soft:#e3f0e8`、`--accent-line:#b7d3c5`。
* 状态与四类视图保留语义色，仅调低饱和以匹配新画布。
* 字体：`--font-display` 改为系统 sans（与 `--font-ui` 同栈，`font-weight:680`、`letter-spacing:-0.02em`）；`--mono` 不变；登录页插画与 `login-caption-zh` 保留衬线字体栈（局部 `--font-serif`）。
* 圆角：`--r-xs:6px; --r-sm:8px; --r-md:12px; --r-lg:16px`。
* 阴影：`0 1px 2px rgba(16,28,22,.05), 0 14px 32px -24px rgba(16,28,22,.35)`；抽屉沿用单侧投影。
* 动效：`--dur:.2s`、`--ease:cubic-bezier(.2,.7,.2,1)`；路由淡入 160ms、抽屉 220ms、卡片 hover 位移 1px；`@media (prefers-reduced-motion: reduce)` 全量关闭 transition/animation。
* 组件层统一：header sticky + 半透明模糊仅在不违反 CSP 的前提下用纯色回退；表格表头 11px 大写字距、行 hover 用 `--bg-subtle`；按钮 focus ring 用 `--ring`；空态统一为“图标 + 一句结论 + 一个主操作”。
* 页面栅格：`main` 最大宽 1180px、左右 `clamp(16px,4vw,40px)`；页面标题区与内容区 24px 间距。
* 响应式：≤900px 表格改为“主列 + 次要信息副行”（沿用现有 `DataTable` 能力）；≤600px 抽屉全屏、卡片内边距 16px、过滤条换行。

### 3.6 不再保留/明确边界

* 不引入 webfont、图标 CDN、`v-html`、内联 `style` 属性、内联 `<script>`。
* 不改变 `src/webui` 产物文件名与 `vite.config.ts`。
* 不为 SQLite/treedb 伪造多实例。
* 凭据展示：`store.info`（Super 专属）返回并展示**完整含密码 URI**；`redact_uri` 继续用于日志、错误文案与 CLI，不在控制台其他页面重复展示凭据。
* 不改任何写操作参数与审计语义。

## 4. 验证计划

前端（`console/`）：

1. `npm run typecheck`
2. `npm run lint`
3. `npm run i18n:audit`
4. `npm run build` 且 `git diff --stat src/webui` 仅预期产物变化

后端：
5. `cargo check`
6. `cargo test --lib pg_store`（新增单测）
7. `cargo test`

真机场景：
8\. SQLite demo 栈（`/tmp/gfsui` 脚本重建）：登录 → 5+1 页全截图 → 抽屉/对话框/空态/错误态。
9\. 多 PG：本机 5433 建两个库，`--meta-store file:` 注册表启动；在 pg2 新建 base；验证 Storage 两卡、置放数、Views 实例列、详情 Home instance、跨实例 fork 拒绝文案不变。
10. 八档宽度截图（320/375/414/600/768/1024/1280/1600），检查 `document.documentElement.scrollWidth <= clientWidth`。
11\. 键盘 Tab 走查 header → 主导航 → 页面主操作；`prefers-reduced-motion: reduce` 下无动画。
12\. 深链：`#/views?instance=pg2`、`#/keys?view=x` 刷新后仍生效。

文档：
13. 更新 `docs/cli-audit-2026-11-22.md`（新增第 11 节“控制台实例视图与关联”）或在 `docs/ui-modernization/` 追加验证记录（截图路径、命令输出摘要）。

## 5. 风险与回滚

* 风险：`store.info` 新增 `storage` 字段体积变大（多实例+多视图时 placement 为 O(视图数)）。缓解：仅 `Super` 可读；单实例不产生 placement。
* 风险：响应内含明文数据库密码（按需求保留）。缓解：`store.info` 仅 `Super`；页面提供“隐藏”切换；不写入审计、localStorage 或 URL；除 Storage/ViewDetail 外不重复渲染。
* 风险：导航第 6 项在 320px 挤压。缓解：<480px 仅图标 + `aria-label`。
* 回滚：后端仅新增字段与只读函数；前端页面按文件粒度独立，可单独回退 `StoragePage.vue` 与 `PAGES` 条目；`console.css` 一次性替换，保留 git 可回退。

## 6. 追加范围：受管注册表的治理写面（2026-11-26）

用户后续要求把 PG 实例的增删改查从 `instances.conf` 迁至网页端，并为 treedb 提供同款清单管理。
这将本规格「不改变任何写路径语义」的边界**扩展到受管注册表的写入**，交付记录见
`verification.md` §5：

- 新增六个 Super 治理 op `storage.{add,update,remove,preview,set_control,relocate}`，同步登记
  `gates` / dispatch / `ALLOWED_OPS`，默认拒绝与 ACL 一致性测试兜底。
- 完整守卫流程：删除前必 preview（受影响视图 + node/distinct-ref 计数 + `grovefs_refs` 一致性），
  非空即拒；控制平面实例须先 `set_control` 两阶段交接（暂存 `@control-pending=`，重启迁移）。
- treedb 以「清单非路由器」如实建模：卡片无 control、`serving` 标记路由地址，`set_control`/`relocate`
  如实拒绝，保留实验性标记与「控制平面仍在本地 SQLite」说明。
- 视图页长列表（>50）有界滚动 + ≤768px 折叠抽屉；滚动条配色统一为主题令牌，修深色档遗留暖米色。
- 未变项：CSP、产物文件名、`store.info` 仅 Super、非托管注册表仍为只读降级。

## 7. 追加范围：引擎判据、审计模块与视觉体系（2026-11-26）

用户提出三项要求：修正部署引擎信息的混淆、把审计保留策略与分页做进审计页、用
frontend-design 方法整体重做视觉。逐条落地如下。

### 7.1 部署引擎的真值判据

- 后端有两个语义不同的 `backend` 字段，前端必须分开读，不能拿其一当其二：
  `store.info.backend` 是**控制面**（treedb 档仍为 `"sqlite"`，因为定义、密钥、账号、审计
  落在本地 SQLite），`store.info.metadataBackend` 才是**树引擎**（取自 `rt.store.backend_name()`）。
  轻量 `info` 只返回 `rt.control.backend()`，故要看树引擎必须走 `store.info`。
- Overview 标题下并列两枚 chip：`控制面 sqlite` 与 `树引擎 treedb`。treedb 专属块（实验性
  说明、实例清单、路由措辞）只在树引擎为 treedb 时渲染；sqlite/pg 档不出现任何 treedb 字样。
- 中文措辞同批纠正：`InstanceCard` 上 treedb 的 `serving` 徽标曾译作「主控服务」，与 PG 的
  `control plane`「控制面」在中文里语义碰撞（treedb 根本没有 control 实例），改为「服务中」；
  并把 zh.ts 中 11 处「控制平面」统一到规范词条「控制面」，与徽标、`（控制面）` 括注同词。
- 后端同批修掉一个会让引擎信息真的读错的状态：treedb 注册表按**名字**寻址
  （`storage.update/remove/preview` 都是 `find(|(n,_)| n == name)` 取第一条），而旧版收养把
  每个 serving 地址都写成 `primary` —— 重名的那台服务器对所有写动词隐形，网页也把两张卡
  折叠成一个键。现由 `unique_name()` 保证收养不重名，并由 `treedb_repair_names()` 在启动时
  自愈历史重名（保留每条名字的首次出现，后续加 `-2`/`-3` 后缀；地址与行序不动），改名逐条
  打进启动日志，因为操作员手上那本书确实被改了。

### 7.2 审计模块

- **保留策略上移**：`AuditRetentionPanel` 从页底移入 `page-head` 正下方，套新的 `.retbar` 条幅
  — grid `minmax(0,1fr) auto`、pine 渐变洗底、3px 左脊、`.retbar-v` 用 serif 数值 chip 显示当前
  生效档位，右侧是预设 chips 与窗口动词；读取上限的控件在分页条（§8.4）。非 Super 只读时走 `.retbar.is-read`，去 pine
  化以免被读成可写。
- **列表分页**：新增 `console/src/components/ui/ListPager.vue`，`.pager` 条幅含每页 20/50/100
  切换与页码导航（首尾 + 当前邻域 + 省略号），页态存 `filters.auditPage` / `auditPageSize`。
- 审计页 DOM 顺序固定为 `page-head → retbar → bar → filterbar → tbl-wrap → pager → muted`，
  已由无头浏览器断言（见 `verification.md`）。
- 分页是已取回集合内的前端切片，不改后端 `limit` 语义、不改审计写入与保留默认值。

### 7.3 视觉体系（当前值，替代 §3.5）

设计语言为 “field instrument”：有细线的绿灰画布、带受光上唇的白板面、单一松绿家族由深到
亮分级、衬线标题 + 系统 UI 正文 + mono 数据三声配对。

* 画布：`--bg:#e7ebe4`；`--bg-2:#ffffff`、`--bg-3:#fbfcfa`（板面渐变终点）、
  `--bg-subtle:#f2f5f1`、`--bg-sunken:#dfe5dc`；`--line:#dde3d9`、`--line-soft:#eaeee7`、
  `--line-strong:#c6cfc3`。
* 画布不是纯色：`--mesh` 两层 radial bloom（白色高光 + pine 淡染），`--texture` 用
  `repeating-linear-gradient(180deg, rgba(13,24,18,.032) 0 1px, transparent 1px 2rem)` 画
  账簿细线；`body` 取 `background: var(--texture), var(--mesh)` 且两层都 `fixed`。
* 文字：`--fg:#0d1812`、`--fg-dim:#4f5e55`、`--fg-faint:#6a786f`。
* 强调：`--accent:#0f6b4f`、`--accent-2:#17835f`、`--accent-3:#4ec396`、
  `--accent-hover:#0a5340`、`--accent-soft:#dcefe5`、`--accent-line:#a6d2bf`、
  `--accent-ink:#f2fbf6`（实底 pine 上的文字色）、`--gold:#96690f`。
  `--accent-grad` / `--accent-grad-hover` 是 135° 三段 pine 渐变，用于活动导航项、主按钮、
  选中 chip、当前页码。
* 字体：`--font-display: var(--font-serif)`，即回到衬线栈 `'Iowan Old Style',
  'Palatino Linotype', Palatino, 'Songti SC', 'Noto Serif CJK SC', Georgia,
  'DejaVu Serif', serif`（中文宿主装有 Noto Serif CJK SC，可实渲染）；页面标题
  `--fs-page: clamp(2rem, 1.55rem + 1.7vw, 2.9rem)`；统计数字 `.card .n`、`.sstat-n`、
  `.retbar-v` 一律 serif；数据、连接串、表格仍走 `--mono`。
* 圆角：`--r-xs:6px; --r-sm:8px; --r-md:12px; --r-lg:16px; --r-xl:20px`（抽屉/对话框）、
  `--r-pill:999px`（导航项、chip、徽标、页码）。
* 深度只用四件事，以免六十条规则各说各话：`--surface-grad` 板面渐变（上唇亮、基部沉）、
  堆叠阴影（接触 + 距离两层，皆以墨色 `rgba(13,24,18,α)` 着色而非中性灰）、1px 内高光
  `--hair`、以及抬升物件上的 pine 脊（`--glow` 与 3px `--accent-grad` 左边脊）。无一处靠
  模糊蒙混成层次。
* 动效：`--dur:.2s`、`--dur-2:.32s`、`--dur-3:.55s`；`--ease` 保留，新增
  `--ease-out: cubic-bezier(.16,1,.3,1)` 与 `--ease-spring: cubic-bezier(.22,1.2,.36,1)`
  （对话框 pop）。卡片入场 stagger 间隔 45ms × 8 张；hover 位移 2–3px 并给 link 卡加
  `5px 6px 0` 的 accent-soft 偏移实影；按下时 `translate(2px,2px)` 收掉偏移。末段
  `@media (prefers-reduced-motion: reduce)` 的 `*` 兜底规则不变，压平全部动画与过渡。
* 深色档：`:root[data-theme='dark']` 只覆盖 token 值（含新增的 `--mesh`、`--texture`、
  `--accent-grad-hover`、`--hair` 等），组件层不出现硬编码浅色。本轮顺手清掉表头、选中行、
  secret-card、skip-link、file 按钮漏网的几处，并留下一条可复验的不变量：除 token 定义行外，
  `console.css` 全文 `#rrggbb` 计数为 0（`awk 'NR>1 && /#[0-9a-fA-F]{3,6}\b/ && !/^\s*--/'` 无输出）。
* §3.6 的边界全部保持：无 webfont、无 CDN 图标、无内联 `style`、无 `data:` 图片；
  CSP `style-src 'self'` 下所有渐变与纹理都写在 `app.css` 里。

## 8. 追加范围：部署后四条反馈（2026-11-26 第三次追加）

换版部署到现网 treedb 档后，用户提出四条反馈：引擎与凭据的展示仍让人困惑、审计「载入行数」
价值存疑、授权视图时报 `view 'test1100' has neither a definition nor a tree`、信息架构层面
仍嫌混乱。逐条落地如下，交付记录见 `verification.md` §7。

### 8.1 控制面为何仍是 SQLite：两枚 chip 之外必须有一句解释

`store.info` 的 `backend`（控制面）与 `metadataBackend`（树引擎）在前端已经分开读、分开标
（§7.1），但**分开标不等于解释**。概览页的「本部署」区因此在两栏之上加一段 `.engine-note`，
按 `catalog.engine` 四档各给一句完整关系陈述（`ENGINE_NOTE: Record<Engine, string>`）：treedb 档
说的是「树住在 treedb 服务器里；控制面 — 定义、凭据、账号和这条审计轨迹 — 是你正在通信的
meta-server 上的本地 SQLite。本页出现两个引擎名，是对两个不同问题各自给出的答案，而不是
同一次部署被报了两遍」。sqlite 档说「一个存储承担全部」，postgres 档说「同一个产品扮演两种角色」，
unknown 档说「不从那个名字推断任何东西」。

该区随后按「概览 → 操作 → 明细」重排，并给两组明细各配一句 `det-h` 小标题：
`Where the data lives`（两栏存储 + 实例账）与 `How far the trail goes`（审计轨迹覆盖范围）。

> 维护约束：`ENGINE_NOTE` 的 key 是 map 查表，`i18n-audit` 的字面量扫描看不见它，必须手工
> 登记进 `zh.ts`（`ENGINE_SUB` 同理）。这是本目录唯一一处「门是绿的但翻译可能缺」的地方。

### 8.2 创建视图可选 treedb 实例，且默认落点由服务端回答

- `view.create` 的实例字段从 `pg_instance` 规范为 `instance`（`pg_instance` 仍是 CLI 在发的别称，
  两者同义），treedb 臂按名钉位；名字不在注册表、或地址不在本机 `--meta-store` 路由集合内、
  或点名的那台是只读副本（`Stats.replica`），都用服务端原文拒绝，**不静默改投别台**。
- 新增纯读 op `view.plan`（`Gate::Super`，与 `view.create` 同级可见性）：`find_tree_home` 命中
  已有树则 `exists: true`，否则 `plan_placement` 给出落点，再经 `treedb_registry` 反查实例名。
  它不 open、不写、不改任何东西。存在的全部理由：「不选的话默认落哪台」必须由拥有那条规则
  的一侧回答，前端复刻判据就是本仓反复消灭的「两张表回答同一个问题」——落点自 2026-11-27 起是
  「最闲的可写引擎」（`docs/adr/0004-treedb-load-aware-placement.md`），要现读每台引擎的
  `ListViews`/`Stats`，前端更不可能自己算；`view.plan` 的答复因此多了 `rule`/`detail` 与
  每台一行 `load[]`，新建面板把「为什么是这台」原样播报，不复述判据。
- `NewBasePanel` 的实例下拉按 `catalog.engine` 分流：treedb 只列 `store.info` 标为 `serving` 的行
  （清单里的非路由条目不是落点候选），PG 列全簿。视图名输入后按序列号防抖调 `view.plan`，
  auto 标签实时播报落点，`exists` 为真时把 `view.create` 即将收到的拒绝提前标红在表单里。
- 同一轮把 treedb 的账本上了存储页：`store.info` 的实例行与 `storage.ledger`
  （`placedTrees` 之外四类说不清：`unnamedHome`/`offServing`/`unreachable`/`duplicates`）
  **同出一次并行 sweep**，
  所以一台宕机不再让这个调用整体失败，只让那台少掉树数。总览页的 base views 因此改成
  带下限措辞的读法——`viewsUnlistedFor` 非空时那个数是**下限**，卡片转 warn（口径见 ADR-0004 §4）。

### 8.3 treedb 不需要账号密码：这是设计，界面上要这么说

决策、边界与未来前置条件记入 `docs/adr/0003-treedb-address-identity.md`。界面三处如实标注：
实例卡 URI 行旁的 `no credentials` 徽标 + 卡下一句小字（引擎认证是服务器全局
`GROVEFS_TREEDB_KEY_FILE`，不是 per-instance 凭据）、注册表单的地址 hint、新建基视图的 hint。
任何一处都没有留下空的密码字段。

盲区（本轮真机截图抓到）：`store.info` 的 treedb 臂会随快照下发一句 `routingNote`，前端若把它
当文案原样渲染，中文界面就会出现整句英文，而 i18n 门查不出来（模板里那句 `t('…')` 只是回退分支，
服务端字段一有值就永远走不到）。规矩与 §8.1 的 `ENGINE_NOTE` 同源：**服务端持有事实，控制台持有
操作员读到的句子**。`StoragePage.vue` 因此只从 `routingNote` 里取那个数字
（`^treedb places new trees on the least-loaded writable engine among (\d+) serving address\(es\)`），措辞归字典；
句式对不上时退回服务端原文，不编造。新增任何「后端 JSON 字段直接当文案渲染」的地方，都必须同步
登记 zh.ts 译文并写明回退路径。

### 8.4 「载入行数」：既不是总数也不是已加载数，于是搬家并改名

判定结论：那个控件的值是**发给后端的 `limit`**（50/200/1000），不是后端返回的审计行总数，
也不是当前分页已加载的行数。所以它与 pager 的「共 N 条」不冗余，但①标签误导（读起来像
「已经载入了这么多」）、②位置错位（它 govern 的是读取窗口，与保留策略无关）。

- 迁入 `ListPager`：新 props `fetchLimit` / `loaded` / `fetchLimits` 与 `update:fetchLimit` 事件，
  标签改 `fetched from server`，改档即回第一页；`AuditRetentionPanel` 删掉 `retbar-load`、
  `LIMITS`、`setLimit` 与对 `useFilterStore` 的依赖，头注释写明它为何不再持有这个控件。
- 新增一条只在信息有价值时出现的句子：当 `loaded === fetchLimit`（服务端被要求返回 N 行且
  确实返回了 N 行）时，pager 下方说明更早的调用几乎肯定还在，调高上限即可继续向前翻。
- 用户设想的「预计可清理 N 条 + 实际清理动作」本就已存在，位置不动：`preview cut` 回显
  `${n} row(s) are older than ${b} (nothing deleted yet)`，`purge now` 是执行入口。

### 8.5 授权报错：后端修判定，前端给下一步

- 后端根因（已修）：`webauth::resolve_base` 直接问 `store.has_physical()`，而 treedb 档的树在引擎里、
  本地控制面没有 `meta/<name>/<name>.db`，于是判定恒 false — 视图真实存在也会被拒成
  「既无定义也无树」。修复是注入 `TreeProbe`（`resolve_base_tree` / `manages_tree` / `refusal_tree`），
  由 `server_ext.rs` 用 `physical_tree_exists` 把「问每一个引擎」这一步漏斗进去。
- 前端两层，且都不改写服务器原文：`utils/error.ts` 的 `refusalOf()` 把四条已知拒绝正则映射成
  「一句可执行的下一步 + 该去哪个页面」，`messageOf()` 的逐字透传契约不变；`GrantPanel` 在发请求
  之前先按本页已加载的命名空间拦截未知视图，把幽灵视图变成「先创建，再授权」，失败内联渲染
  （标题 + 服务器原文 + `open that page`），且不 rethrow — rethrow 会让 `ActionButton` 再 toast 一次，
  同一句话以两种形态出现。

### 8.6 信息架构：一个 section-gap、每页一句区域标题、一条跨页路径

- **节奏**：新增 `--section-gap: 1.75rem`（28px，落在要求的 24–32px 区间）。它由**开启区块的
  标题** `.bar` 的 `margin-top` 承载，靠块级兄弟的垂直 margin collapsing 统一全站，而不是给每个
  区块补 `margin-bottom`——后者与 panel 自带的间距折叠后仍是同一个数，却要求每个新组件重新对齐。
  `.page-head` 底部与 `.pager` 顶部同步改用该 token。
- **分区**：六页（Overview / Audit / Storage / Views / Keys / Accounts，按使用频率）一律
  「概览 → 操作 → 明细」自上而下；每组卡片或表格前一句 `.bar` 标题 + 一句说明：
  `At a glance`、`Typical path`、`Where the data lives`、`How far the trail goes`、
  `This deployment`、`Instances`、`Keys you can see`、`Accounts and their reach`。区块内的小标题
  复用既有 `det-h` 原语，不另造第二套（本轮曾加 `.grp`，发现与 `det-h` 重复后删除）。
- **引导**：Overview 新增 `.flow` 四步（创建基准视图 → 授权账号 → 签发挂载密钥 → 查看发生了什么），
  CSS counter 编号，每步是跳转链接。Storage 页把 `Register instance` 从表格之后的孤立工具条移进
  `Instances` 标题行——动词开在它作用的那个区；Keys 页的 HelpPanel 从「表单与表格之间」移到页末，
  因为它是参考而非步骤。Views 页原本已有 BarRow 分区，本轮不动其结构。
- **未破坏项**：CSP `style-src 'self'` 下无内联 `style`（构建产物 grep `style=` 为 0）、
  产物名仍固定为 `src/webui/{index.html,app.js,app.css}`、响应式断点与
  `prefers-reduced-motion` 兜底不变、`console.css` 除 token 定义外零硬编码色值。

## 9. 追加范围：节点角色与账户角色不得共用一个词（2026-11-26 第四次追加）

概览页曾打印 `server role: Primary` / `本控制台正在对话的进程`。操作员问“这是啥意思”，
原因有三层，逐层修：

- **一词两义**：`store.info.role` 是**节点**在 HA 副本对里的位置（`Primary`/`Standby`），
  `whoami` 回的 `info.role` 是**账户**权限（`admin`/`user`）。两者都叫 role，同页出现却
  没有任何限定。现在标签分别为 `node role (HA)` 与 `account role: ${r}`，被退役的旧 key
  `server role`、`the process this console is talking to` 已从 `zh.ts` 删除。
- **值恒真**：`MetaRuntime::role()` 对未配 HA 的节点**恒返回 `Primary`**，所以这一行在单写
  部署上是纯噪音。`store.info` 因此新增 `ha` 字段：**只在真的配了 HA 时非 `null`**，内容是
  判定这对副本是否健康的四项事实——对端地址、距上次对端心跳的秒数、fencing 纪元、仲裁租约
  是否在握，外加同步复制与 witness 清单。单调时钟过线时换算成“距今秒数”，`Instant` 不上线。
- **该说不说的分支**：`ha == null` 时界面写“未启用”加一句解释，而不是继续印 `Primary`；
  PG 档另写“冗余由 PostgreSQL 复制承担”——原生 HA 的快照投递只在 SQLite 档成立（`serve()`
  对 PG store 拒收 `--role/--peer/--witness`），给 PG 部署念一句它开不了的功能是误导。

`primary`/`standby` 两个词进字典（译作「主（Primary）」「备（Standby）」），与留在英文的
API 名词（`admin`、`mount`）区分开：节点角色决定这里是否接受写，它是要据此动手的运维判据，
不是接口名字。
### 9.1 同一页剩下的那句英文：`backendLayout` 透传，以及正文字面量里的 `/`

改完两行角色、部署上真机之后，截图里紧挨着的 `tree engine` 行仍然印着整句英文：
`store.backendLayout` 是后端 `backend_layout()`（`src/remote/meta/server_ext.rs`）下发的四种
固定句式之一，前端 `{{ store.backendLayout }}` 原样渲染。它与 §8.3 的 `routingNote` 同源同罪，
且 i18n 门同样查不出来——那句字符串从不经过 `t()`。

归属规矩照旧：**服务端持有事实，控制台持有操作员读到的句子**。差别在于这句里没有可提取的数字，
形状本身就是全部信息，所以按形状查字典整句替换：四种形状各一条 `LAYOUT_*` 测试，
`sqlite`/`postgres`（带 `file:` 注册表与不带走两条）/`treedb`；都不匹配时退回服务端原文，
绝不编造。`--meta-store`、`<base>` 是命令与路径，按惯例留在译文原形。

本轮踩到的新坑，记下来是给下一个写这种表的人：JS 正则**字面量**里的 `/` 就是定界符，
`/^one db file per view under <base\\/meta\\/>/` 实际被解析成正文
`^one db file per view under <base\\/meta\\/` 加一段名为 `meta` 的标志位，
于是该分支永远返回 false——`t()`、typecheck、eslint、i18n 门四道全是绿的。
`node -e` 复现时还要当心 shell 把 `\\/` 吃掉一层，看起来像“转义与构造函数不一致”。
规矩：**这类形状别把路径写进字面量正文**，锚定到 `/` 出现之前即可（四种形状的
60 字符前缀互不相同，足够唯一）；真要匹配 `/`，改用 `new RegExp(…)` 并另写断言。

因此这一行多了两道离仓校验（量具在 `.tmpedit/`，随仓 `.gitignore` 的 `/.*/` 规则不入库）：
`layout_check.py` 从 `server_ext.rs` 里取出服务端能下发的全部形状，逐一按代码自身的分支顺序
问“谁会接住它”，并要求字面量正文不含 `/`；`layout_render.mjs` 更狠，直接从**已构建的**
`src/webui/app.js` 里把压缩后的四个分支读回来评测，防止“源码修好而产物没重建”。
注入旧写法（带 `\\/` 的那版）后两道都变红，说明它们不是装饰。

同一个区域还剩一处英文：`control plane` 行把 `store.describe` 原样接在值后面。那句**是**逐部署
信息（`<路径> (local files)`、`remote control plane at <addr> (backend <x>)`、
`treedb <addr> view=<n> id=<i> conns=<k>`），不是可查表的句式，所以不在本轮范围；若要双语化，
做法与 `routingNote` 相同——拆出常量外壳与变量，外壳进字典。

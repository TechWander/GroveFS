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

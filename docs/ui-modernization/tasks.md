# GroveFS 控制台现代化改造任务清单

依赖顺序执行；每完成一项即更新 TodoWrite。

## T1 后端实例信息（backend）
- [x] `src/pg_store.rs`：新增 `InstanceSummary` / `InstanceReport` / `instance_report(spec)`（只解析、只读、绝不建表）。
- [x] `src/pg_store.rs`：单实例零 SQL；多实例一次连接读 `grovefs_control.view_placement`；表缺失、连接失败分别降级并回传 `error`。
- [x] `src/pg_store.rs`：补 3 个单元测试（registry 两条、单条 PG、缺文件），断言 `uri` 完整保留密码。
- [x] `src/remote/meta/server_ext.rs` `store.info`：新增 `storage{backend,instances,placement,instanceError}`，按 Sqlite/Pg/Treedb 分支；PG 分支返回**原始含密码 URI**（Super 专属）。
- [x] `src/remote/meta/server_ext.rs` `view.stats`：新增 `instance`（best-effort，不影响整体成功）。
- [x] 验证：`cargo check && cargo test --lib pg_store`。

## T2 前端数据层
- [x] `console/src/types.ts`：`StoreInstance`、`StoreInfo.storage`、`StoreInfo.instances/placement`、`ViewStats.instance`。
- [x] `console/src/stores/catalog.ts`：暴露 `instances`、`placement`、`instanceError`、`backend` 的 getter。
- [x] `console/src/stores/filters.ts`：新增 `instance`、`view`、`owner` 三个可深链维度。
- [x] `console/src/composables/useNav.ts`：新增 `goTo(page, query)`。
- [x] 验证：`npm run typecheck`。

## T3 Storage 页与导航
- [x] `console/src/pages/StoragePage.vue`：四种 backend 分支（SQLite / 单 PG / 多 PG / treedb）。
- [x] 实例卡：名称、控制平面徽标、完整 URI（含密码，默认展示 + 复制 + 隐藏切换）、置放视图数、视图清单、进入 Views 过滤、在该实例新建 base。
- [x] `console/src/components/views/NewBasePanel.vue`：支持 `pgInstance` 预置并随 `view.create` 发送 `pg_instance`。
- [x] `console/src/router/index.ts` + `NavIcon.vue` + i18n：新增 Storage 目的地与图标、词条。
- [x] 响应式导航：1024/768/480 三档行为。
- [x] 验证：单实例与多实例两种后端下页面截图。

## T4 关联闭环
- [x] `ViewsPage.vue` / `ViewList.vue`：实例列与 `instance` 过滤条；未置放视图的 `hash` 徽标说明。
- [x] `ViewDetail.vue`：Home instance 区块（含完整 URI）+ 绑定密钥/相关账号两条互链。
- [x] `KeysPage.vue` / `KeyTable.vue` / `KeyDrawer.vue`：view → Views、owner → Accounts。
- [x] `AccountsPage.vue` / `GrantTable.vue` / `AccountDrawer.vue`：base_view → Views、账号 → Keys。
- [x] `AuditPage.vue`：key_label/key_id → Keys，detail 中 view → Views（仅在可解析时）。
- [x] 验证：9 条跳转矩阵逐条点击，落地页筛选正确。

## T5 设计系统
- [x] `console/src/styles/console.css`：:root tokens 全量替换（画布/surface/墨色/accent/圆角/阴影/动效）。
- [x] 组件层校准：header、按钮、表格、徽标、抽屉、对话框、提示、空态、登录页。
- [x] `prefers-reduced-motion` 与 focus-visible 全量覆盖。
- [x] 响应式：main 栅格、≤900px 表格副行、≤600px 全屏抽屉、≤480px 图标导航。
- [x] 验证：八档宽度无横向溢出。

## T6 全场景验证
- [x] `npm run typecheck && npm run lint && npm run i18n:audit && npm run build`。
- [x] `cargo check && cargo test`。
- [x] SQLite demo 栈：登录、6 页、抽屉/对话框/空态/错误态截图。
- [x] 多 PG（127.0.0.1:5433 两库）：实例卡、置放、实例内新建 base、Views 过滤、详情 Home instance、跨实例 fork 拒绝。
- [x] 深链刷新、键盘 Tab、reduced-motion。
- [x] 追加验证记录文档（截图路径 + 命令摘要）。

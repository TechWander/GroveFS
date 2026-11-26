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

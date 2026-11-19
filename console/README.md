# GroveFS 管理台前端（`console/`）

这里是 `grovefs web` 那套单页应用的重构版源码：**Vue 3（Composition API + `<script setup>`）+ TypeScript 严格模式 + Vite**。
重构只动架构与呈现，不动业务与契约——`src/web.rs` 一行未改，`/api/*` 的入参回包、路由白名单、
CSP、会话与 CSRF 机制全部沿用；构建产物直接落在 `src/webui/{index.html,app.js,app.css}`，
仍由 `include_str!` 在编译期嵌进二进制（`src/web.rs:56-59`）。

- 依赖全是各生态当时的**稳定版**，锁无 caret，无 beta/rc/alpha：`vue 3.5.45`、`vue-router 5.5.1`、
  `pinia 4.1.0`、`vite 8.3.8`、`typescript 6.0.3`、`vue-tsc 3.3.14`、`eslint 10.15.0`、
  `typescript-eslint 8.73.0`、`eslint-plugin-vue 10.12.1`、`prettier 3.10.1`（详见 `package.json`）。
- **UI 组件库有意不装**：网关的 CSP 是 `script-src 'self'; style-src 'self'`，且二进制要在离线主机上
  自解释——任何第三方组件包都意味着引入一套与本仓设计令牌打架的样式与一批新的运行时依赖。
  控制台的词汇表只有按钮 / 表格 / 徽标 / 抽屉 / 对话框 / toast 六样，13 个 `ui/` 原语（外加列/片两张描述表）就足以全覆盖，
  也让「无内联样式、无 CDN、无 webfonts」这条既有纪律保持得下去。

---

## 1. 目录结构

```
console/
├── index.html              34  入口文档：只有 #app 一个挂载点（外壳是 App.vue，不再有两份 DOM 描述）
├── vite.config.ts          65  产物契约：固定文件名、单 chunk、无内联、产物写进 ../src/webui
├── eslint.config.js        69  flat config：js + ts(type-checked) + vue/recommended，最后接 prettier
├── tsconfig.json               strict 全集 + @/* 路径别名（见 §5 工程规范）
├── .prettierrc.json            semi:false / singleQuote / printWidth 100
├── .gitignore                  node_modules / dist / *.local
├── public/favicon.svg          原样透传到 src/webui/（`/favicon.svg` 是网关第四条静态路由）
├── tools/i18n-audit.js    161  词典双向核对（`npm run i18n:audit`，参照物取自 git 历史）
└── src/
    ├── main.ts             19  createApp + pinia + router，样式以 import 方式进 `/app.css`
    ├── App.vue             91  外壳状态机：booting / anonymous / ready，并挂住全局的确认框 / 行抽屉 / 一次性密钥 / toast
    ├── env.d.ts            21  给 ESLint 的 TS program 用的 *.vue 兜底声明（vue-tsc 用不到它）
    ├── types.ts           173  后端回包的形状：SessionReply / ViewRow / KeyRow / AuditRow / …
    ├── api/               203  ── 传输层
    │   ├── http.ts             四个端点的 fetch 封装：同源带 cookie、写操作带 x-grovefs-csrf
    │   └── ops.ts              Op = web.rs 的 ALLOWED_OPS 联合类型；OpResult 逐 op 标注回包形状
    ├── router/             70  ── 五页一张表：router / 导航 / 页头三处共用，hash 模式
    ├── stores/            416  ── pinia，四个 store 各管一件事
    │   ├── session.ts          谁在登录 + 网关放行哪些 op（唯一真相，登录后复读 /api/session）
    │   ├── catalog.ts          视图名册（三个页面都要，带缓存与每次 load 刷新）
    │   ├── filters.ts          四个列表页的过滤状态；跨刷新与换页存活，登录时清空
    │   └── ui.ts               toast / 确认框 / 行抽屉——整站只允许一个抽屉打开
    ├── composables/       243  ── 跨组件的行为
    │   ├── useOps.ts           api()（抛原句）与 soft()（把拒绝降级成局部错误）两种口味
    │   ├── usePageLoad.ts      旧版 go(page) 的替身：pending / error / reload 订阅
    │   ├── overlayStack.ts     Esc 归谁：对话框盖住抽屉时，一次 Esc 只关最上面那层
    │   ├── useNav.ts           三处跨页跳转，一次带上「选谁」和「去哪页」两个事实
    │   └── useClipboard.ts     复制 + 失败回落
    ├── i18n/              588  ── index.ts（t / lang / 变量替换）+ zh.ts（397 条中文）
    ├── utils/             330  ── audit / error / keys / text / time / views：纯函数，可单测
    ├── styles/console.css 2138 设计令牌 + 组件样式 + 五个断点（沿用并补完旧版那套令牌）
    ├── pages/             906  ── 五页：Overview / Views / Keys / Accounts / Audit
    └── components/       4133  ── 65 个文件（62 个 SFC + 3 张描述表），按“是不是外壳 / 是不是原语 / 属于哪一页”分目录
        ├── chrome/  5   AppHeader、LoginCard、BrandMark、NavIcon、LangToggle
        ├── layout/  9   PageHead、PanelBox、KvList/KvRow、FieldRow、FldRow、BarRow、StatLine、HelpPanel
        ├── overlay/ 5   BaseModal、AskModal、BaseDrawer、DrawerSection、ToastHost
        ├── ui/     15   DataTable + table.ts、ActionButton、StateBadge、MonoText、MaskText、
        │                TimeCell、TwoLineCell、WhoCell、CopyButton、CountPill、ChipGroup + chips.ts、FilterBar、LoadingHint
        ├── overview/  1 StatCard
        ├── views/     8 ViewList、ViewDetail、LineagePanel、LineageTree + tree.ts、
        │              NewBasePanel、DefEditPanel、ImportPanel
        ├── keys/      5 KeyTable、MiniKeyTable、KeyRowActions、MintPanel、TtlSelect
        ├── accounts/  6 AccountTable、GrantTable、BasePills、CreateAccountPanel、GrantPanel、SelfAccountPanel
        ├── audit/     1 AuditTable
        ├── dialogs/   6 ForkDialog、DefineDialog、KeyEditDialog、PasswdDialog、RoleDialog、SecretDialog
        └── drawers/   4 KeyDrawer、AccountDrawer、AuditDrawer、AppDrawer
```

对照旧实现：`src/webui/app.js` 2395 行、`app.css` 1392 行、`index.html` 94 行，三个文件里
DOM 结构、事件绑定、词典、渲染逻辑互相按名字硬连接。新实现共 95 个源文件 / 9331 行（其中样式表 2138 行），
换回来的是「一个事实只写一处」：五页清单在 `router/`，op 清单在 `api/ops.ts`，
样式钩子随组件走，页面结构不再需要和一段 JS 协商 id。

---

## 2. 产物与网关的契约（为什么 `vite.config.ts` 长这样）

网关只答四条静态路由，且只有 GET：`/`、`/app.js`、`/app.css`、`/favicon.svg`（`src/web.rs:414-429`），
静态回复一律 `no-store`。CSP 是 `default-src 'none'; script-src 'self'; style-src 'self'; form-action 'none'`。
下面这份配置就是从这几条约束倒推出来的：

| 约束                                | 配置                                                                                                                                                                                  |
| ----------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 文件名必须是那四个                  | `entryFileNames`/`chunkFileNames` 固定 `app.js`；CSS 资源经 `assetFileNames` 改名 `app.css`；**不做 content hash**（哈希名是一个网关不会应答的 URL）                                  |
| `/app.js` 是唯一 script 路由        | `rollupOptions.output.codeSplitting: false`（现代表写法，替代废弃的 `inlineDynamicImports`），路由**一律不懒加载**                                                                    |
| 不能有内联脚本/样式、不能有 `data:` | `cssCodeSplit: false` + `assetsInlineLimit: MAX_SAFE_INTEGER`（不产生 side-car 文件）+ 产物里 0 个 `style=` 属性、0 个 `<style>`；`index.html` 只有一行 `<script type="module">` 外链 |
| 二进制要自包含、离线可用            | `outDir: '../src/webui'` + `emptyOutDir`，产物**入库**，所以 `cargo build` 不需要 node 工具链（只有改前端才需要 npm）                                                                 |
| 旧版就是 ES2020 手写文件            | `target: 'es2020'`，不抬高语法下限                                                                                                                                                    |
| 开发期不碰已装二进制                | `server.port 5173` + `/api` 代理到 `127.0.0.1:8787`                                                                                                                                   |

样式走 `import './styles/console.css'` 而不是 `<link>`：这样 Vite 才会把它作为**唯一**的
`/app.css` 产出，浏览器拿到的样式与二进制里嵌的那份必然同源同批——这是旧版靠“手写三件套”
白捡的性质，现在由构建配置来保证。

---

## 3. 本地启动与构建

前置：Node `^20.19.0 || >=22.12.0`（`engines` 已声明；`i18n:audit` 直接 import `.ts`，
靠的是 Node 的类型剥离）、npm、以及一份能跑起来的治理口。

```bash
cd console
npm ci                      # 严格按 package-lock.json 安装

npm run dev                 # http://localhost:5173，/api/* 代理到 127.0.0.1:8787
```

`npm run dev` 需要旁边有一个网关在听 8787；只想看页面，用已装好的二进制起一个也行：

```bash
cargo build --release
./target/release/grovefs web --control-server 127.0.0.1:15698 \
    --api-key-file ./control.key --listen 127.0.0.1:8787
```

四道质量门（改任何前端文件都要全绿，`build` 自身先跑 `vue-tsc --noEmit`）：

```bash
npm run typecheck     # vue-tsc -p tsconfig.json --noEmit
npm run lint          # eslint .（含 type-checked 规则集）
npm run format:check  # prettier --check .   （`format` 就地写回）
npm run build         # 产物写进 ../src/webui，同时 emptyOutDir 清掉上一版残留
npm run i18n:audit    # 词典双向核对（见 §6）
```

之后照常 `cargo build --release` 把新产物嵌进二进制，再按各自部署方式换版。本机的四机台架是
`bash .build-bin/fdb4_upgrade.sh push && … restart && … status`（该目录不入库，属于取证脚本），
换完用 `bash .build-bin/fdb4_console_read.sh` 逐字节核对“下发资源 == 源码产物”。

---

## 4. 关键实现思路

### 4.1 `usePageLoad`——每个页面都必须有 loading 和 error

旧版的 `go(page)`（`app.js:932-945`）已经做了“先 `loading…`、抛错就把服务端原句写成 `.err`”这一对；
`usePageLoad` 把同一对抬成一个组合式函数，五页共用一份实现（不会再出现“这一页忘了写错误分支”），
并多了一层订阅：

```ts
const { pending, error } = usePageLoad(load)
```

它做四件事：进 `load()` 前置 `pending`、抛出的错取服务端原句存进 `error`、`onMounted` 跑一次、
并订阅 `ui.reload()`——旧版的 `reload()` 是在每个动词调用点手工调的，漏一处就显示一条
已经不存在的记录；现在页面只要声明了自己的 `load()`，刷新就是自动的。
页面模板的三段式是固定形状：`v-if="error"` → `v-else-if="pending && !data"` → `v-else`。

### 4.2 分块到达，而不是整页等最慢的那个

`OverviewPage` 一次要读 `whoami`、`store.info`、每个基视图的 `view.stats`、最近调用。
一开始实现成整页一个 `pending` 门控，实测签入后到“有数据”反而比旧版慢 75 ms——因为旧版是
卡片先进 DOM、表最后补。改成每块独立门控（`cardsReady` / `reachDone` / recent-calls 三态）后
中位数回到 **175.9 ms（旧版 180.2 ms）**。同时 `reachDone` 也挡住一句假话：基视图统计还在飞的时候
不能先渲染“没有分给你的基视图”。这条是本轮唯一发现的**真实回退**，修完有复测数据。

### 4.3 `api/ops.ts` 把白名单变成类型

`Op` 就是 `web.rs` 的 `ALLOWED_OPS`（30 个），`OpResult` 逐 op 标注回包形状。于是：
`gc.scan` / `gc.apply` / `refs.rebuild` / `view.rm_base` / `fork.graft` 这五个不过路由的动词
**在类型层面就写不出来**，写出来也过不了 `vue-tsc`；调用前先查 `/api/session` 返回的放行清单，
换了一个二进制时报“某个具名 op 不可用”，而不是一个说不清的 403。
这只是给读者的礼貌，判定仍在服务端 `gates()`——所以 `useOps.api()` 抛的是**服务端原句**，
一字不改地显示（`soft()` 用于“一个页面读六样东西，其中一样被拒”的场合，降级成局部 `.err`）。

### 4.4 i18n：英文整句即键，且有意不用 vue-i18n

`t()` 用英文句子当键，查 `zh.ts` 后替换 `${var}`；缺键降级成英文而不是抛错；语言存在
`gfs.lang`，`detect()` 回落 `navigator.language`。不用 vue-i18n 的原因是硬的：它的消息编译器
要 `new Function`，而 CSP 没有 `unsafe-eval`；改成 `unplugin-vue-i18n` 预编译则要重写 430 条
“文本即键”的句子，其中 `@`、`$`、`|` 还得逐个转义。`lang` 是 `ref`，所以模板里调用 `t()` 的
组件天然随切换重渲染；`App.vue` 另外 `watch(lang, …, { immediate: true })` 同步
`document.documentElement.lang` 与 `document.title`——中文页面被声明成 `en`，读屏软件的发音就错了。

有一处**必须记住的坑**：`<p class="muted">{{ t('A client presents `<id>:<secret>` …') }}</p>`
会让 Prettier 的 Vue 解析器把 mustache 里的 `<id>` 当成标签，整个文件拒绝格式化（`KeyDrawer.vue`
踩过）。这类句子移到 `<script setup>` 里包成 `computed`（不是常量：抽屉开着切语言要重译），
模板只引用它，词典键本身一字不改。

### 4.5 overlay：三种遮罩，一套 Esc 纪律

- `BaseModal` 保留旧版对话框的两条契约：**内联显示拒绝**（动词的 handler 拿到 `(fail, close)`，
  被拒的原句就显示在用户刚输入的文字旁边，`fail()` 保持对话框开着并把按钮放回可用）、
  **页脚恒在**（不因为一次失败就塌掉重排——这条本轮修过并重建复验）。
- 对话框这一侧**新增**的是焦点：打开时焦点落进卡片、`Tab` 走不出去、关闭后焦点回到按下它的那个控件。
  旧版全站只有一处 `.focus()`（登录输入框），这三条都没有。
- `BaseDrawer` 明确**不是**对话框：`aria-modal="false"`（旧版同样如此），读屏不会被关进面板里；`Esc`、遮罩、✕
  都关，焦点回到来源行。全站只有一个抽屉（`ui` store 存一条），换行即换记录，刷新即关——
  一条陈旧记录并排在新列表旁边比没有面板更糟。
- `overlayStack` 是个数组：密钥抽屉里可以叠“编辑”对话框，旧版只有一个全局 listener，
  一次 `Esc` 会把还压在下面的抽屉连带关掉。
- `SecretDialog`（一次性密钥）与抽屉同属 `App.vue` 而非某个页面：抽屉关了它还活着，
  因为那份凭据真的还存在。

### 4.6 `DataTable`：一行既是目标也是容器

`cols: Column<T>[]` 描述列（旧版 `keyCols()` 的 idea 原样继承），行可点开记录，而行内
`button`/`a`/`select`/`input`/`textarea`/`label` 上的点击**只**触发那个动词——守卫是
`ev.target.closest(CONTROLS)`，可点行同时拿到 `tabindex="0"`、`role="button"` 与 Enter/Space。
这套行语义与旧版 `app.js:631-643` **逐条相同**，属于「保留」而不是「改进」。
所有服务端字符串都以文本或组件节点进入 `<td>`：
全项目没有一处 `v-html`，且 `vue/no-v-html` 是 error 级 lint 规则——旧版“视图名不能注入标记”
这条性质就是靠它保住的。

### 4.7 样式：令牌继承，缺口补完

`console.css` 的前身是旧版 `app.css` 里已经存在的那套令牌（`9e631b4` 引入），本轮**继承而不是重设计**，
补的是它没覆盖到的地方：

- `.spin` 旧版**连这个类带 `@keyframes spin` 都不存在**（`app.css` 里查无此项），新版加了它，并给
  `ActionButton` / `BaseModal` 页脚 / 登录按钮 / `LoadingHint` 都挂上忙态环 + `aria-busy`。
  旧版飞行中确实会把按钮 `disabled`（`app.js:806-818`），但视觉上只有 `button:disabled{opacity:.5}`
  那一层变暗——“在路上”与“这个按钮点不动”分不清。截图 `02a_after_click` 就是这一格差异。
- `.field` 旧版只在 `.panel .field` 生效，所以**对话框里的标签与控件是相邻排布**；
  新版把 `.field` 提成组件级规则，面板与对话框共用“标签在上、控件满宽、hint 在下”的同一套排版。
- `for` 与 label 的显式绑定：旧版靠相邻关系配对，读屏拿不到名字（见 `FieldRow.vue` 注释）。
- 断点沿用 1180 / 1024 / 880 / 640 / 480，`prefers-reduced-motion: reduce` 关掉全部动效，
  `:focus-visible` 统一外圈，窄屏时五个导航项变成“图标在上、文字在下”的等宽五格而不是被裁字。
- 390×844 实测无横向溢出（`scrollWidth === clientWidth === 390`）；审计表在 `.tw` 容器内滚动，
  表格本身仍是 804 px。

---

## 5. 工程规范

- `tsconfig.json`：`strict: true`，另开 `noImplicitOverride`、`noUncheckedSideEffectImports`、
  `noUnusedLocals`、`noUnusedParameters`、`noFallthroughCasesInSwitch`、`erasableSyntaxOnly`、
  `verbatimModuleSyntax`、`isolatedModules`；路径别名 `@/*`。
- `eslint.config.js`：`typescript-eslint` 的 **recommendedTypeChecked**（走 projectService，
  所以 `tsconfig.json` 是 TS 看到的唯一真相）+ `plugin-vue` flat/recommended，最后接
  `eslint-config-prettier`。自定义里最值得记的两条：`vue/no-v-html: error`（§4.6），
  `@typescript-eslint/no-misused-promises: error` **对模板同样生效**——`@click` 绑一个没 `void`
  的 async 函数，正是治理调用被静默吞掉的唯一途径。
- `src/env.d.ts` 是给 ESLint 的 TS program 的 `*.vue` 兜底声明；`vue-tsc` 有自己的 Vue 语言插件，
  不会命中它，因此它**不会**削弱 `npm run typecheck`（传错 prop 照样报错，已验）。
- 命名与词汇约定：`PascalCase` 组件、`useXxx` composable、纯函数进 `utils/`。有一条纪律贯穿全局：
  **词汇型 prop 传词典键（`label="state"`，由接收方 `t()`），数据型 prop 传译文本身**——
  把服务端数据喂给 `t()`：轻则原样返回（等于没译），重则一条恰好碰上词典键的备注或视图名
  被翻译了——那是在改数据，不是在译界面。

---

## 6. 新旧对照

### 6.1 功能等价（逐项验过，不是“应该没动”）

| 旧实现                                                           | 新实现                                           | 验证方式                                                               |
| ---------------------------------------------------------------- | ------------------------------------------------ | ---------------------------------------------------------------------- |
| 登录 / 登出 / 8h 空闲 / 失败限流提示                             | `LoginCard.vue` + `stores/session.ts`            | `test_web.sh` 178 条 + 真机走查                                        |
| 五页 `nav button[data-page]` 切类                                | `router/index.ts` 一张表 + 5 条 hash 路由        | 走查逐页有数据；`#/keys` 可书签、可前进后退                            |
| `apikey` 七动词（list/create/revoke/rotate/set_ro/update/calls） | `api/ops.ts` 同名 `Op`                           | 走查含“铸一次密钥 → 一次性明文 → I have saved it 关闭”“轮换确认框取消” |
| `account` 建号/删号/改密/角色 + `grant add/rm`                   | `accounts/` 六组件 + `RoleDialog`/`PasswdDialog` | 走查账号抽屉 + 授权增删                                                |
| 视图清单 / 详情 / 血缘 / define / fork / import                  | `views/` 八组件 + `ForkDialog`/`DefineDialog`    | 走查视图详情与血缘树                                                   |
| 审计列表 + 搜索过滤 + 行抽屉                                     | `AuditPage.vue` + `AuditTable` + `AuditDrawer`   | 过滤用的 haystack 字段序与旧版 `app.js:2370-2371` **逐字一致**         |
| 中英双语 + `documentElement.lang`/`title` 同步                   | `i18n/` + `App.vue` 的 watch                     | 走查切语言并回读两个属性                                               |
| 行可点开抽屉、抽屉里再开对话框                                   | `DataTable` + `overlayStack`                     | 走查密钥/账号/审计三个抽屉                                             |
| 一次性密钥只打印一次、库里只有摘要                               | `SecretDialog.vue`                               | 走查断言 + 178 条回归里的不落盘断言                                    |

词典用 `npm run i18n:audit` 双向核对，本轮结果：`zh.ts` **397** 条 / 旧词典 392 条 /
送进 `t()` 的串 341 个 / **缺 0**（该显示的句子全部有译）/ 旧条目未被继承 **0**。
397 比 392 多的 5 条逐条核过都在页面上用得到：“Sections of the console”（帮助面板小标题）、
两句会话读取失败的提示（`… could not read its own session` / `… could not reach the gateway`）、
两个空态（“no base views delegated to you”、“no keys in its name”）。
参照物不再指向某个人的临时目录——缺省参数时脚本会走 git 历史，取“最后一次仍然内联着
`const I18N_ZH = {` 的 `src/webui/app.js`”，因此这条核对在换版落地之后依然可复现。

### 6.2 显示与交互上的改进（有证据的才写）

1. **动词有明确的忙态**：飞行中除了旧版已有的 `disabled`（视觉上只是变暗），多了 spinner 与
   `aria-busy`，“在路上了”从此看得见（截图 `02a` 对照）。
2. **首屏不再空窗**：外壳先出、`LoadingHint` 说“loading…”，旧版在 `/api/session` 那一趟回来之前是纯白纸。
3. **错误绝不静默**：`usePageLoad` 统一 error 分支 + `ActionButton` 把 rejection 变成 toast；
   局部读失败（如抽屉里的最近调用）只在块内降级成一行 `.err`，不吞掉整页。
4. **对话框表单排版统一**：标签上置、控件满宽、hint 在下方（§4.7）。
5. **Esc 只关最上面那层**（§4.5）。
6. **焦点管理**：对话框的初始焦点、Tab 循环、关闭后焦点归还（§4.5）。旧版全站只有一处 `.focus()`。
7. **DOM 变小、渲染复用**：登录屏 36 vs 79 个节点，走查末尾停在审计页时 337 vs 1552。
   旧版每次渲染都 `clear()`（`while (n.firstChild) n.removeChild(…)`）后逐节点重建整个列表，
   新版是 keyed `v-for`，过滤与刷新时复用行节点。
8. **签入后出数据的体感持平略快**：sign→shell 129.6 vs 150.8 ms，sign→data 175.9 vs 180.2 ms
   （9 轮交错的**中位数**，同一后端、localhost、headless Chrome 147、1440×900）。

### 6.3 有意的偏差（架构差异，不是功能增删）

- **URL 形状**：旧版永远停在 `/`，新版是 `/#/keys` 这类 hash 路由。不是选择 history 模式偷懒——
  网关除四条静态路由外一律 404，真 `/keys` 刷新即死；hash 是这份契约下唯一能同时满足
  “可分享/可前进后退”与“不改后端”的写法。
- **`#app` 的语义**：旧版把签入后的外壳就叫 `<div id="app">`（`index.html:53`，`#login` 是另一个
  section），靠 JS 切 `hidden`；两套样式表都从不按 `#app` 选择。新版 `#app` 只承担 Vue 挂载点
  这一件事，签入后的 `header`/`main` 直接是 `App.vue` 的分支，少一层没有语义的容器。
- **`#page-keys` 的包裹层**：样式表里唯一那条页面级规则（给铸 key 面板五列紧凑网格）保留了这个
  id，作用域从 `<section>` 换成一个 `<div>`，视觉一致。
- 上述 5 条新增词典键（§6.1）是全站内唯新的文案：两处空态、两处会话读失败的提示、
  一处帮助面板小标题，都中英各一条，并由 `i18n:audit` 盯着。

### 6.4 实测的代价（如实记录）

同批次同机器，同样是 9 轮交错**中位数**；除传输字节外，表里的指标都是**登录屏**这一屏的绝对值
（CDP `Performance.getMetrics` 的 `ScriptDuration` / `JSHeapUsedSize` 与页内 `querySelectorAll('*').length`；
这些计数器跨导航单调增长，做前后差值得到的是负数，这是个坑）。

| 指标                   | 新版                   | 旧版     | 说明                                            |
| ---------------------- | ---------------------- | -------- | ----------------------------------------------- |
| `app.js` 传输字节      | 221.18 kB（gzip 79.1） | 124.6 kB | Vue 运行时带来的净增，`no-store` 下每次都要重取 |
| `app.css`              | 32.0 kB（gzip 7.0）    | 42.1 kB  | 令牌收敛后反而小了                              |
| first-paint            | 40 ms                  | 40 ms    | 持平                                            |
| first-contentful-paint | 64 ms                  | 56 ms    | 脚本变大后的首次绘制代价                        |
| DOMContentLoaded       | 44.5 ms                | 29 ms    | 同上                                            |
| 脚本执行 CPU           | 8.2 ms                 | 1.0 ms   | 框架启动成本（一屏的累计值）                    |
| JS heap                | 6.75 MB                | 7.47 MB  | 常驻反而更省                                    |

结论要分开说：**功能没有回退**（回归 178/178、逐页走查等价、词典 0 缺 0 丢、四机下发资源逐字节核对 49/49），
**呈现与交互有提升**（忙态/加载/错误/排版/Esc/无障碍，均有截图或断言），
**冷启动的机读指标有一小幅后退**，代价来自框架运行时，收益是 DOM 规模与渲染复用；
在真实网络与真实治理往返（sign→data 一百多毫秒）面前，这几毫秒的绘制差被网络时间吃掉。
如果哪天这个差值变成要紧的事，可行的方向是砍体积而不是砍架构：把 `zh.ts` 拆成按需加载会破坏
“单 chunk”契约，所以更现实的是继续压 `console.css` 与组件数。

---

## 7. 加一个页面 / 改一个动词怎么下手

1. 动词若是**新** op：先改 `src/web.rs` 的 `ALLOWED_OPS`（后端契约，需单独评审），再补 `api/ops.ts`
   的 `Op` 与 `OpResult`，否则 `vue-tsc` 不让你调用它。
2. 新句子一律走 `t('English sentence')`，然后**必须**跑 `npm run i18n:audit`，把新键补进 `zh.ts`——
   缺键不会报错，只会在中文界面里露出英文，这正是这条检查存在的理由。
3. 新页面：`pages/XxxPage.vue` + `router/index.ts` 加一行（导航、页头、路由都从这张表来），
   取数一律 `usePageLoad`，列表一律 `DataTable`，遮罩一律 `BaseModal`/`BaseDrawer`。
4. 改样式优先改令牌，不要在组件里写死颜色与间距；不要引用样式表里不存在的类名。
5. 提交前四道门全绿 + `npm run build`，产物（`src/webui/`）与源码一起提交，
   这样没有 node 的机器 `cargo build --release` 出来的二进制仍然带着这次改动。

/*
  The GroveFS management console.

  Plain ES2020, no build step, no dependencies: the gateway's CSP is
  `script-src 'self'`, and an air-gapped deployment must render the same page as
  one with outbound access. DOM is built by hand (see `el`), which is also why
  nothing here can inject markup from a view name or a server error — every
  string reaches the page as a text node.

  Two rules the rest of the file obeys:
  1. EVERY capability check here is presentation only. The meta-server decides
     what an account may do (`server_ext::gates`); hiding a button is a courtesy,
     and a refusal from the server is rendered verbatim because its wording
     ("account 'x' is not an administrator, and 'view.create' is not per-view
     governance: …") is the answer to the user's actual question.
  2. An op is only offered if the gateway routes it. `/api/session` returns the
     console's own whitelist, so a page built against a different binary fails on
     a named missing op instead of a mystery 403.
*/
'use strict';

const ST = {
  csrf: null,       // per-session token, required on every mutating call
  who: null,        // whoami: {account, role, super, bases}
  ops: [],          // what the gateway will route
  page: 'overview',
  views: [],        // [{name, kind, base}] merged from physical.list + def.list
  sel: null,        // selected view name
  auditLimit: 50,   // kept across reloads, so "refresh" does not shrink the window
  vFilter: { q: '', kind: 'all' }, // client-side view list filter, reset per boot
  // The three list pages keep their filters in one place each: a page that
  // forgets what you typed the moment a button is pressed is a page you have
  // to re-search after every action.
  keyFilter: { q: '', state: 'all', scope: 'all', view: 'all' },
  keyCur: null,     // row id the drawer is open on (for the `.cur` stripe)
  accFilter: { q: '', role: 'all', state: 'all' },
  auditFilter: { q: '', window: 'all', plane: 'all', op: 'all', verdict: 'all' },
};

/* ----------------------------------------------------------------- i18n --- */
/* Two-language UI. The dictionary is keyed by the ENGLISH sentence — code reads
   naturally, a missing entry degrades to the original text, and only one table
   (zh) needs maintenance. Interpolated sentences use their template as the key
   (${var} is replaced after lookup), so nothing is ever concatenated into a
   key. Server-refusal sentences stay verbatim by design: the exact wording is
   the answer to the user's question (see the rules at the top of this file).
   Product vocabulary — base/fork/alias/lens, mount/mcp/admin/any, admin/user —
   is intentionally NOT translated: those names are the API's own terms. */
let LANG = 'en';
try {
  LANG = localStorage.getItem('gfs.lang')
    || ((navigator.language || '').toLowerCase().startsWith('zh') ? 'zh' : 'en');
} catch (e) { /* storage denied: the default stands */ }

const I18N_ZH = {
  /* chrome: sign-in, nav, header */
  'Governance console': '治理控制台',
  'Sign in': '登录',
  'Sign out': '退出登录',
  'Account': '账号',
  'Password': '口令',
  'password': '口令',
  'copy': '复制',
  'copied to clipboard': '已复制到剪贴板',
  'Overview': '总览',
  'Views': '视图',
  'Keys': '密钥',
  'Accounts': '账号',
  'Audit': '审计',

  /* shared */
  'Cancel': '取消',
  'Confirm': '确认',
  'nothing here': '这里还没有内容',
  'refresh': '刷新',
  'loading…': '加载中…',
  'the session ended — sign in again': '会话已结束 — 请重新登录',
  'signed out': '已退出',
  'sign-in failed (HTTP ${s})': '登录失败（HTTP ${s}）',
  'the gateway answered HTTP ${s} without a JSON body': '网关返回 HTTP ${s}，响应体不是 JSON',

  /* overview */
  'backend': '后端',
  'signed in as': '登录身份',
  'base views': '基视图',
  'definitions': '定义',
  'accounts': '账号',
  'live keys': '有效密钥',
  'mount sessions': '挂载会话',
  'open trees': '常驻树',
  'This deployment': '本部署',
  'server role': '服务器角色',
  'base views you govern': '你治理的基视图',
  'Your reach': '你的权限范围',
  'Deployment-wide counts belong to an administrator. You govern these base views and everything derived from them:': '全局计数属于管理员。你治理以下基视图及其全部派生：',
  '${n} derived': '${n} 个派生',
  'unreachable': '不可达',
  'Recent governance calls': '最近的治理调用',

  /* audit table */
  'when': '时间',
  'key': '密钥',
  'plane': '面',
  'op': '操作',
  'result': '结果',
  'detail': '详情',
  'ok': '成功',
  'failed': '失败',
  'no governance calls yet': '暂无治理调用',

  /* views page */
  'no views you can see': '没有你可看见的视图',
  'pick a view on the left': '请在左侧选择一个视图',
  '${n} mount session(s)': '${n} 个挂载会话',
  'base': '基',
  'stamp': '戳记',
  'tree': '树',
  'on this server': '在此服务器上',
  'keys': '密钥',
  'physical tree': '物理树',
  'fork tree': 'fork 树',
  'none of its own': '无自有树',
  'open': '已打开',
  'not open': '未打开',
  '${n} bound to this name': '${n} 把绑定到此名',
  'Export dump': '导出转储',
  'Fork…': 'Fork…',
  'Lens / alias…': '透镜 / 别名…',
  'Drop fork': '删除 fork',
  'Remove definition': '移除定义',
  'Drop this fork?': '删除这个 fork？',
  '${name}: its own tree is deleted. Its base ${b} and every shared object stay.': '${name}：其自有树将被删除。它的基 ${b} 与全部共享对象保留。',
  'Remove this definition?': '移除这个定义？',
  '${name} =\n${def}\n\nOnly the name goes away — no tree and no object is deleted.': '${name} =\n${def}\n\n仅移除名字 — 不删除任何树与对象。',
  'definition removed': '定义已移除',

  /* fork dialog */
  'Fork ${name}': 'Fork ${name}',
  'A fork copies the METADATA tree into a new writable view; content bytes stay shared until something in the fork rewrites them.': 'fork 会把元数据树复制成一个新的可写视图；内容字节保持共享，直到 fork 内有改写发生。',
  'new view name': '新视图名',
  'copy the current tree (unchecked: an empty fork to graft into)': '复制当前树（不勾选：留一个空 fork 用于接枝）',
  'Create fork': '创建 fork',
  'forked ${a} → ${b} (${n} nodes, ${m} blobs)': '已 fork ${a} → ${b}（${n} 个节点、${m} 个 blob）',

  /* define dialog */
  'derived lens (read-only shape over a base)': '派生透镜（在基之上的只读形状）',
  'alias (another name for it)': '别名（它的另一个名字）',
  'Define a view over ${name}': '为 ${name} 定义视图',
  'The definition text is exactly what `grovefs view define --file` takes.': '定义文本与 `grovefs view define --file` 的输入完全一致。',
  'kind': '种类',
  'definition': '定义',
  'Save': '保存',
  'definition saved': '定义已保存',

  /* new base view */
  'creating a BASE view is an administrator’s act — it chooses where bytes live.': '创建基视图是管理员操作——它决定字节落在哪个实例。',
  'New base view': '新建基视图',
  'name': '名称',
  'PG instance': 'PG 实例',
  'Create': '创建',
  'a base view owns physical deletion; lenses, aliases and forks reshape one': '基视图拥有物理删除权；透镜、别名与 fork 是对基的重塑',

  /* import */
  'metadata dump': '元数据转储',
  'as view': '作为视图',
  'replace an existing tree': '替换已存在的树',
  'import as a BASE view': '作为基视图导入',
  'Import': '导入',
  'choose a dump file first': '请先选择转储文件',
  'import failed (HTTP ${s})': '导入失败（HTTP ${s}）',
  'imported ${n} nodes, ${m} blobs as ${kind}': '已导入 ${n} 个节点、${m} 个 blob，作为 ${kind}',
  'Import a dump': '导入转储',
  'a dump becomes a BASE view, or a fork of a view you govern — content bytes are never in a dump': '转储会成为基视图，或你治理视图的一个 fork — 转储中不含内容字节',
  'imported as a FORK of ${name}; only an administrator can import a new base view': '将作为 ${name} 的 fork 导入；只有管理员能导入新基视图',

  /* keys */
  'Keys bound to ${name}': '绑定到 ${name} 的密钥',
  'label': '标签',
  'id': 'ID',
  'scope': '范围',
  'mode': '模式',
  'owner': '属主',
  'last used': '最近使用',
  'state': '状态',
  'read-only': '只读',
  'read-write': '读写',
  'revoked': '已吊销',
  'live': '有效',
  'no keys bound to this view yet': '此视图尚无绑定密钥',
  'Edit this definition': '编辑此定义',
  'saved — live mounts pick it up within their attr TTL (≤2s)': '已保存 — 活跃挂载将在其 attr TTL（≤2s）内感知',
  'API keys': 'API 密钥',
  'Mint a key': '铸造密钥',
  'mount — a client mount': 'mount — 客户端挂载',
  'mcp — an agent': 'mcp — 智能体',
  'admin — the governance plane': 'admin — 治理面',
  'any — every plane': 'any — 所有面',
  '(no view binding — every tree)': '（不绑定视图 — 可达所有树）',
  '(choose a view)': '（选择一个视图）',
  'view': '视图',
  'key rotated': '密钥已轮换',
  'Create key': '创建密钥',
  'a delegated account mints client keys only': '委派账号只能铸造客户端密钥',
  'This is the only time the secret is shown — GroveFS stores a digest.': '秘钥仅此一次显示 — GroveFS 只保存摘要。',
  'label ${l} · scope ${s} · owner ${o} · ${b}': '标签 ${l} · 范围 ${s} · 属主 ${o} · ${b}',
  'bound to ${v}': '绑定到 ${v}',
  'not bound to a view': '未绑定视图',
  'I have saved it': '我已保存',
  'an unbound key reaches every tree, so a delegated account must name one': '未绑定密钥可达所有树，因此委派账号必须指定一个视图',
  'any view': '任意视图',
  'created': '创建时间',
  'no keys you can see': '没有你可看见的密钥',
  'Make rw': '改为读写',
  'Make ro': '改为只读',
  'now read-write': '已改为读写',
  'now read-only': '已改为只读',
  'Revoke': '吊销',
  'Rotate': '轮换',
  'Rotate this key?': '轮换这把密钥？',
  '${l} (${i}) · scope ${s}\n\nThe id, label, scope, view binding and audit history are kept; only the secret is replaced. The old secret stops working at its NEXT handshake — a live mount on it stays open until it reconnects; to kill a credential outright, Revoke it.': '${l}（${i}）· 范围 ${s}\n\n保留 id、标签、范围、视图绑定与审计历史，只替换 secret。旧 secret 在下一次握手时失效 — 基于它的活跃挂载在重连前仍保持打开；要彻底封杀请直接吊销。',
  'Key rotated': '密钥已轮换',
  'Revoke this key?': '吊销这把密钥？',
  '${l} (${i}) · scope ${s} · ${note}\n\nLive mounts keep working until their next operation.': '${l}（${i}）· 范围 ${s} · ${note}\n\n活跃挂载在其下一次操作前继续可用。',
  'NOT bound to a view — it reaches every tree': '未绑定视图 — 可达所有树',

  /* accounts */
  'administrators act here': '管理员操作区',
  'Create an account': '创建账号',
  'a-z 0-9 . _ -, 2–32 chars': 'a-z 0-9 . _ -，2–32 个字符',
  'at least 10 characters': '至少 10 个字符',
  'role': '角色',
  'user — governs what it is delegated': 'user — 按委派治理',
  'admin — governs the deployment': 'admin — 治理全局',
  '${account} created as ${role} — ${note}': '${account} 已创建为 ${role} — ${note}',
  'Your account': '你的账号',
  'The account list is an administrator’s view: ${e}': '账号列表是管理员视图：${e}',
  'Change my password': '修改我的口令',
  'delegated bases': '委派的基视图',
  'none': '无',
  'last sign-in': '最近登录',
  'disabled': '已禁用',
  'enabled': '已启用',
  'My password': '我的口令',
  'Make user': '降为 user',
  'Make admin': '升为 admin',
  'Enable': '启用',
  'Disable': '禁用',
  'Disable sign-in?': '禁用登录？',
  'Enable sign-in?': '允许登录？',
  '${account} could no longer sign in; its open sessions die on their next call.': '${account} 将无法再登录；其打开的会话在下一次调用时失效。',
  '${account} may sign in again.': '${account} 可重新登录。',
  '${account} disabled': '${account} 已禁用',
  '${account} enabled': '${account} 已启用',
  'Delete': '删除',
  'Delete this account?': '删除这个账号？',
  '${account} — its delegations go with it.\nIts api keys DO NOT; the server reports how many are still live.': '${account} — 其委派随之删除。\n其 API 密钥不会删除；服务器会报告仍有几把有效。',
  '${account}: ${note}': '${account}：${note}',
  'no accounts': '没有账号',
  'Delegate a base view': '委派基视图',
  'base view': '基视图',
  'Grant': '授予',
  '${account} governs ${base}': '${account} 治理 ${base}',
  '${account} governs ${base} — that is what the name you chose resolves to': '${account} 治理 ${base} — 你选的名字实际解析到这里',
  'Delegations': '委派',
  'an account governs a base view AND every lens, fork and alias derived from it': '账号治理一个基视图及其全部透镜、fork 与别名派生',
  'perm': '权限',
  'granted': '授予时间',
  'Withdraw': '撤销',
  'Withdraw this delegation?': '撤销这条委派？',
  '${account} stops governing ${base} and everything derived from it, on its next call.': '${account} 将在下一次调用时停止治理 ${base} 及其全部派生。',
  'withdrawn': '已撤销',

  /* password / role dialogs */
  'Password for ${account}': '${account} 的口令',
  'new password': '新口令',
  'repeat it': '再输入一遍',
  'Change password': '修改口令',
  'the two entries differ': '两次输入不一致',
  'password changed': '口令已修改',
  'Make ${account} ${role}': '将 ${account} 改为 ${role}',
  'An administrator governs every view, every account, and where a new base view is placed.': '管理员治理所有视图、所有账号，并决定新基视图的落位。',
  'A user governs only the base views it is delegated, plus their forks, aliases, lenses and keys.': 'user 只治理被委派的基视图，及其 fork、别名、透镜与密钥。',
  'Make ${role}': '改为 ${role}',
  '${account} is now ${role}': '${account} 现在是 ${role}',

  /* audit page */
  'Only your own activity and your views’ keys; the full log is an administrator’s view.': '仅你的操作与你视图的密钥；完整日志是管理员视图。',
  'the fork tree is gone, but its definition could not be removed: ${e}': 'fork 树已删除，但其定义无法移除：${e}',
  'view created': '视图已创建',
  'New api key': '新的 API 密钥',
  'account': '账号',
  /* page heads */
  'Deployment snapshot, your reach and the latest governance activity.': '部署概要、你的权限范围与最近的治理动态。',
  'Base views, forks, aliases and lenses — select one to inspect.': '基视图、fork、别名与透镜 — 选择一个进行查看。',
  'Mint, rotate and revoke the keys clients mount and agents call with.': '铸造、轮换与吊销客户端挂载及智能体调用所用的密钥。',
  'Who governs what — accounts, roles and base-view delegations.': '谁治理什么 — 账号、角色与基视图委派。',
  'Every call the control plane answered — failures included.': '控制面应答的每一次调用 — 含失败记录。',
  /* filtering */
  'filter by name…': '按名称过滤…',
  'failures only': '仅看失败',
  'all': '全部',
  'forks': 'fork',
  'aliases': '别名',
  'lenses': '透镜',
  'no views match the filter': '没有符合过滤条件的视图',
  'no keys match the filter': '没有符合过滤条件的密钥',
  'no calls match the filter': '没有符合过滤条件的调用',
  'View list': '视图列表',

  /* 抽屉与过滤条：三个列表页共用的组件 */
  'Close': '关闭',
  'What the server said': '服务器的原话',
  'nothing recorded': '无记录',
  'no stored fingerprint': '无存储指纹',
  'first and last 4 hex digits of the secret; only its digest is stored, so the middle can never be shown again': 'secret 的首尾各 4 位十六进制；库中只存它的摘要，因此中段永远无法再次显示',
  '${n} in total': '共 ${n} 条',
  '${n} of ${m} shown': '显示 ${n} / ${m} 条',
  'rows to load': '载入行数',
  'time': '时间',
  'who': '发起者',
  'all roles': '所有角色',
  'any state': '所有状态',
  'every plane': '所有面',
  'every view': '所有视图',
  'every op': '所有操作',
  'every result': '所有结果',
  'all time': '全部时间',
  'last hour': '最近 1 小时',
  'last 24 hours': '最近 24 小时',
  'last 7 days': '最近 7 天',
  'only ok': '仅看成功',
  'unknown': '未知',
  'unassigned': '未指定',
  'by ${by}': '由 ${by}',

  /* 总览页：「本部署」区块 */
  'storage backend': '存储后端',
  'one store for metadata and content': '元数据与内容同库',
  'metadata root': '元数据根',
  'the process this console is talking to': '本控制台正在对话的进程',
  'namespace': '命名空间',
  '${n} base view(s)': '${n} 个基视图',
  'the trees mounts open': '挂载打开的树',
  '${n} names resident here': '${n} 个名字在此常驻',
  'credentials': '凭据',
  '${n} live': '${n} 把有效',
  '· ${n} expired · a revocation or expiry lands on the NEXT call': '· ${n} 把已过期 · 吊销与过期都在下一次调用落地',
  'governance accounts': '治理账号',
  'principles on the governance plane': '治理面上的原则',
  '${all} key(s) · ${live} live': '${all} 把密钥 · ${live} 把有效',
  'activity on this server': '此服务器上的活动',
  'governance call': '治理调用',
  'Governance calls': '治理调用',
  'read live from the meta-server that answered this page': '由应答本页的 meta-server 实时读取',
  'Every count is a snapshot of the instant this page loaded; anything created elsewhere appears after a refresh.': '每个计数都是本页加载那一刻的快照；其他地方的创建需刷新后才会出现。',
  'open the audit page': '打开审计页',
  '· the first owner was created by `meta-server --admin-user`': '· 首任管理员由 `meta-server --admin-user` 创建',
  '${n} expired and refused': '${n} 把已过期并被拒绝',
  'none expired': '无过期',

  /* 视图页：谱系面板 */
  'This name': '这个名字',
  'Lineage': '谱系',
  'Definition': '定义',
  'fork': 'fork',
  'alias': '别名',
  'lens': '透镜',
  'other': '其他',
  'names that resolve through this base': '经由该基解析的名字',
  'names derived directly from it': '直接派生自它的名字',
  'nothing is derived from it yet': '尚无东西派生自它',
  '${n} name(s) hang below it': '其下有 ${n} 个名字',
  '${n} name(s) resolve through this base — ${d} directly below it': '${n} 个名字经由该基解析 — 其中 ${d} 个直接位于其下',
  'deeper than ${d} levels — expand it from its own page': '深于 ${d} 层 — 请从其自身页面展开',
  'resolves through': '经由…解析',
  'open this base view': '打开该基视图',
  'forks, aliases and lenses': 'fork、别名与透镜',

  /* 密钥页：台账列与有效期 */
  'note': '备注',
  'valid for': '有效期',
  'valid until': '有效至',
  'Expires': '有效期',
  'never expires': '永不过期',
  'keep unchanged': '保持不变',
  'no deadline': '无期限',
  'no deadline now': '当前无期限',
  'no note': '无备注',
  'expired': '已过期',
  'expiring soon': '即将到期',
  '1 hour': '1 小时',
  '12 hours': '12 小时',
  '1 day': '1 天',
  '7 days': '7 天',
  '30 days': '30 天',
  '90 days': '90 天',
  '1 year': '1 年',
  'expired ${when}': '${when} 过期',
  'expires ${when}': '${when} 到期',
  'revoked ${at}': '${at} 被吊销',
  'it expired at ${at}': '它于 ${at} 过期',
  'it expires at ${at}': '它于 ${at} 到期',
  'already past': '已过期',
  'less than a minute': '不足 1 分钟',
  'in ${left}': '${left} 之后',
  'now ${at} — ${left}': '现在 ${at} — 距此 ${left}',
  'minted by': '铸造者',
  'minted by ${who}': '由 ${who} 铸造',
  'unknown — this row predates the bookkeeping columns': '未知 — 该行早于登记信息列的建立',
  'not revoked': '未吊销',
  'filter by label, id, note, owner or view…': '按标签、ID、备注、属主或视图过滤…',
  'unbound keys': '未绑定密钥',
  'unbound — every tree': '未绑定 — 可达所有树',
  'key ${id}': '密钥 ${id}',
  'key updated': '密钥已更新',
  'Edit': '编辑',
  'Edit ${label}': '编辑 ${label}',
  'Credential': '凭据',
  'Bookkeeping': '登记信息',
  'Last calls with it': '它最近的调用',
  'no calls recorded for it yet': '尚未记录到它的调用',
  'the newest ${n} of them, whatever the filter shows': '其中最新的 ${n} 条，与过滤显示无关',
  'the newest ${n} loaded; the server sweeps rows older than ${w}': '已加载最新 ${n} 条；服务端会清掉早于 ${w} 的记录',
  'the server keeps every row until an operator purges them (`grovefs apikey purge`)': '服务端会一直保留全部记录，直到有人手工清理（`grovefs apikey purge`）',
  'a fingerprint identifies a key; it does not authorize anything': '指纹用于认出密钥；它不授予任何权限',
  'What each action does — and what GroveFS keeps': '每个操作做什么 — 以及 GroveFS 保存什么',
  'Make ro / Make rw': '改为只读 / 改为读写',
  'the key column': '密钥列',
  'what this key is for…': '这把密钥的用途…',
  '${n} characters at most; empty clears it': '最多 ${n} 个字符；留空即清除',
  'shown in the list and searchable; it never affects what the key can do': '在列表中显示并可搜索；它绝不影响密钥能做什么',
  'a key past its deadline is refused at the next handshake': '过了期限的密钥在下一次握手时被拒绝',
  'a mount with it cannot write': '用它挂载则不可写',
  'it can write': '它可写',
  'Save it now. From here on the list shows its fingerprint ${h}; Rotate is the only way to obtain a usable secret again.': '现在就保存。此后列表只显示它的指纹 ${h}；轮换是重新获得可用 secret 的唯一途径。',
  'A client presents `<id>:<secret>` (`mount --api-key`, or the `GROVEFS_API_KEY` environment variable). GroveFS stores only a digest, so this screen cannot read the secret back — that is what makes a leaked database harmless.': '客户端提交 `<id>:<secret>`（`mount --api-key`，或 `GROVEFS_API_KEY` 环境变量）。GroveFS 只保存摘要，因此本页面无法读回 secret — 这正是数据库泄露也无害的原因。',
  'A note and a deadline are bookkeeping: they change what the registry says about this key, never what it can open.': '备注与期限属于登记信息：它们只改变登记表对此密钥的记述，绝不改变它能打开的内容。',
  '`mount` authenticates a client mount, `mcp` an agent, `admin` the governance console, `any` everything. An `admin` key can change accounts and keys — treat it as a password.': '`mount` 认证客户端挂载，`mcp` 认证智能体，`admin` 认证治理控制台，`any` 覆盖全部。`admin` 密钥可改动账号与密钥 — 请把它当作口令对待。',
  'a bound key can only open that view (and what derives from it); an unbound one reaches every tree. Only an administrator may mint an unbound key.': '绑定的密钥只能打开该视图（及其派生）；未绑定的可达所有树。只有管理员能铸未绑定密钥。',
  'mints a 16-character id and a 64-hex-digit secret; the client uses `<id>:<secret>`. The secret exists once, in the dialog you are about to close.': '铸出一个 16 字符的 id 与一个 64 位十六进制的 secret；客户端使用 `<id>:<secret>`。secret 只存在一次，就在你即将关闭的这个对话框里。',
  'shows the stored fingerprint: the first and last four hex digits of the secret. GroveFS keeps a SHA-256 digest, NOT the secret and NOT anything reversible, so no screen, API or database dump can show the middle — that is the price of a leaked database being harmless.': '显示存储的指纹：secret 的首尾各四位十六进制。GroveFS 保存的是 SHA-256 摘要，不是 secret，也不可逆，因此任何界面、API 或数据库转储都拿不出中段 — 这正是数据库泄露无害所付的代价。',
  'replaces the secret for the SAME id: label, scope, view binding, note, deadline and audit history stay. The old secret stops working at its next handshake; a live mount on it stays open until it reconnects.': '为同一个 id 替换 secret：标签、范围、视图绑定、备注、期限与审计历史都保留。旧 secret 在下一次握手时失效；基于它的活跃挂载在重连前保持打开。',
  'retires the credential for good. Nothing can un-revoke it and a revoked row is not editable — create a new key instead. It takes effect on the key’s NEXT call, so a live mount keeps its open trees until then.': '彻底作废该凭据。吊销不可撤销，且已吊销的行不可编辑 — 请改为新建密钥。它在密钥的下一次调用生效，因此在那之前活跃挂载保持其已打开的树。',
  'flips the write permission without touching the secret, so a mount can be made read-only without handing anyone a new credential.': '翻转写权限而不动 secret，因此可把一个挂载改为只读，而无需交给任何人新凭据。',
  'changes the bookkeeping only: the note and the deadline. Scope, view binding and id never change here, because a key an application is already holding would otherwise acquire a reach its owner did not ask for.': '只改登记信息：备注与期限。范围、视图绑定与 id 在此永不改变，否则应用已持有的密钥就会获得其属主并未要求过的可达范围。',
  'the same spelling as `grovefs apikey create --ttl`: a count and `s`, `m`, `h` or `d`, or `never`. Past its deadline a key is refused at the next handshake, and a live mount is cut within about a second.': '与 `grovefs apikey create --ttl` 同一种写法：数字加 `s`、`m`、`h` 或 `d`，或 `never`。过了期限的密钥在下一次握手被拒绝，活跃挂载约一秒内被切断。',
  'the account that created the key. It is shown beside the owner, who is the account the key belongs to — the two differ whenever an administrator mints for a colleague.': '创建该密钥的账号。它与属主并列显示，属主是密钥所属的账号 — 管理员替同事铸造时两者不同。',
  'view binding': '视图绑定',

  /* 账号页：搜索、过滤与抽屉 */
  'Your own record': '你自己的记录',
  'What you govern': '你治理什么',
  'What it governs': '它治理什么',
  'Credentials in its name': '以其名义的密钥',
  'delegated base views': '委派的基视图',
  'names that reach through them': '经由它们可达的名字',
  'an administrator governs every view and every account': '管理员治理所有视图与所有账号',
  'a user governs only the base views it is delegated': 'user 只治理被委派的基视图',
  'a user is useless until it is delegated a base view below': '在下方向它委派基视图之前，user 治理不了任何东西',
  'It governs no base view: a user with no delegation can sign in and change its own password, nothing else.': '它不治理任何基视图：未被委派的 user 只能登录并修改自己的口令，别无其他。',
  'You govern no base view yet — ask an administrator for a delegation.': '你尚未治理任何基视图 — 请向管理员申请委派。',
  'no base view is delegated to anyone yet': '尚未向任何人委派基视图',
  'no base view is delegated to this account': '未向该账号委派任何基视图',
  'no delegations match the filter': '没有符合过滤条件的委派',
  'no accounts match the filter': '没有符合过滤条件的账号',
  'filter by account or base view…': '按账号或基视图过滤…',
  'filter by account, role or delegated view…': '按账号、角色或委派视图过滤…',
  'the key registry is an administrator’s view': '密钥登记表只有管理员可见',
  'show this account’s keys on the keys page': '在密钥页展示该账号的密钥',
  'joined ${when}': '${when} 加入',
  'disabled ${at}': '${at} 被禁用',
  'not disabled': '未禁用',
  'never': '从未',
  'signed in here': '在此登录',
  'this is you': '这是你',

  /* 审计页：按时间/发起者/操作检索 */
  'Every call the control plane answered, by any key or account. Failures are here too — a wrong credential included. Click a row for the whole message the server wrote.': '控制面应答的每一次调用，来自任何密钥或账号。失败也在其中 — 含凭据错误的调用。点击某行查看服务器写出的完整信息。',
  'filter by account, key, op or message…': '按账号、密钥、操作或消息过滤…',
  'no account on it': '其上没有账号',
  'never used': '从未使用',
  'clients reading trees now': '正在读取树的客户端',
  'and everything derived from them': '以及它们的全部派生',
  '· ${n} definition(s)': '· ${n} 个定义',
  '· ${n} derived name(s)': '· ${n} 个派生名字',
  '· ${n} tree(s) open in memory': '· ${n} 棵树在内存中打开',
};

const t = (k, vars) => {
  let s = (LANG === 'zh' && I18N_ZH[k]) || k;
  if (vars) for (const [a, b] of Object.entries(vars)) s = s.split('${' + a + '}').join(String(b));
  return s;
};

/** Fill every static element and re-title the document. Called at boot and on
    every switch; dynamic DOM is (re)built from t() by the pages themselves. */
function applyStatic() {
  for (const n of document.querySelectorAll('[data-i18n]')) n.textContent = t(n.getAttribute('data-i18n'));
  for (const n of document.querySelectorAll('[data-i18n-html]')) n.innerHTML = t(n.getAttribute('data-i18n-html'));
  document.documentElement.lang = LANG === 'zh' ? 'zh-CN' : 'en';
  document.title = LANG === 'zh' ? 'GroveFS 控制台' : 'GroveFS console';
  const lbl = LANG === 'zh' ? 'English' : '中文';
  for (const b of document.querySelectorAll('.lang-btn')) b.textContent = lbl;
}

function setLang(l) {
  LANG = l;
  try { localStorage.setItem('gfs.lang', l); } catch (e) { /* non-persistent is fine */ }
  applyStatic();
  // Re-render whichever surface is up: the signed-in pages build every label
  // from t(), and the sign-in card may be showing a server line of its own.
  if (ST.who) go(ST.page); else showLogin();
}
for (const b of document.querySelectorAll('.lang-btn')) {
  b.addEventListener('click', () => setLang(LANG === 'zh' ? 'en' : 'zh'));
}

/* ------------------------------------------------------------------ dom --- */

const $ = (id) => document.getElementById(id);

/** el('td', {class:'mono'}, 'text', node…) — text is never HTML. */
function el(tag, attrs, ...kids) {
  const n = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v === null || v === undefined || v === false) continue;
    if (k === 'class') n.className = v;
    else if (k === 'text') n.textContent = String(v);
    else if (k.startsWith('on') && typeof v === 'function') n.addEventListener(k.slice(2), v);
    else if (k === 'hidden') n.hidden = true;
    else n.setAttribute(k, v === true ? '' : String(v));
  }
  for (const kid of kids.flat(4)) {
    if (kid === null || kid === undefined || kid === false || kid === '') continue;
    n.appendChild(typeof kid === 'object' && kid.nodeType ? kid : document.createTextNode(String(kid)));
  }
  return n;
}

const clear = (n) => { while (n && n.firstChild) n.removeChild(n.firstChild); return n; };
/** Same contract as el(), for SVG namespaces — document.createElement('svg')
    would build an HTMLUnknownElement that renders nothing. */
function svgEl(tag, attrs, ...kids) {
  const n = document.createElementNS('http://www.w3.org/2000/svg', tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v === null || v === undefined || v === false) continue;
    n.setAttribute(k, v === true ? '' : String(v));
  }
  for (const kid of kids.flat(4)) if (kid) n.append(kid);
  return n;
}
const wrap = (...nodes) => el('div', null, ...nodes);

function toast(msg, kind) {
  const node = el('div', { class: 'toast ' + (kind || '') }, String(msg));
  $('toast-host').appendChild(node);
  setTimeout(() => node.remove(), kind === 'bad' ? 15000 : 5000);
}

/** Copy to the clipboard. The console usually runs on plain HTTP inside the
    cluster, where the async Clipboard API is unavailable (secure-context
    only) — fall back to the deprecated-but-working execCommand path, with
    the probe textarea styled by class (CSP forbids inline style). */
function copyText(txt) {
  const done = () => toast(t('copied to clipboard'), 'good');
  const fallback = () => {
    const ta = document.createElement('textarea');
    ta.value = txt;
    ta.className = 'copy-probe';
    document.body.appendChild(ta);
    ta.select();
    try { document.execCommand('copy'); done(); } catch { /* nothing else to try */ }
    ta.remove();
  };
  if (navigator.clipboard && window.isSecureContext) {
    navigator.clipboard.writeText(txt).then(done, fallback);
  } else fallback();
}

/**
 * One modal host, two shapes. Each footer button gets `(btn, err, close)`, so a
 * refusal can be shown INSIDE the dialog that caused it — the server's ACL
 * sentence about a missing delegation is only useful next to the button that
 * pressed for it, where the text the user just typed is still on screen.
 */
function modal(title, body, buttons, oncancel) {
  const host = $('modal-host');
  const err = el('p', { class: 'err', role: 'alert' });
  const close = () => { host.hidden = true; clear(host); };
  host.hidden = false;
  clear(host).appendChild(el('div', { class: 'modal' },
    el('h3', null, title),
    typeof body === 'string' ? el('pre', null, body) : body,
    err,
    el('div', { class: 'foot' },
      el('button', { type: 'button', onclick: () => { close(); if (oncancel) oncancel(); } }, t('Cancel')),
      (buttons || []).map((b) => el('button', {
        type: 'button', class: b.class || 'primary',
        onclick: (ev) => b.onclick(ev.currentTarget, err, close),
      }, b.t)))));
  return { close, err };
}

/** The irreversible-action prompt. Shows the NAME, because "Are you sure?" is
    answered without reading it. */
function ask(title, lines, confirmText) {
  return new Promise((resolve) => {
    modal(title, lines, [{
      t: confirmText || t('Confirm'), class: 'danger',
      onclick: (btn, err, close) => { close(); resolve(true); },
    }], () => resolve(false));
  });
}

/** The shared "nothing here" glyph: a stroke inbox, tinted by its cell. */
const EMPTY_ICON = () => svgEl('svg', {
  viewBox: '0 0 24 24', width: 26, height: 26, fill: 'none',
  stroke: 'currentColor', 'stroke-width': 1.5, 'stroke-linejoin': 'round', 'aria-hidden': 'true',
},
  svgEl('path', { d: 'M5 6h14l3 7v6a1 1 0 0 1-1 1H3a1 1 0 0 1-1-1v-6l3-7z' }),
  svgEl('path', { d: 'M2 13h5l1.5 2.5h7L17 13h5' }));

function tableNode(cols, rows, emptyText, rowAttrs) {
  const node = el('table');
  node.appendChild(el('thead', null, el('tr', null, cols.map((c) => el('th', null, c.t)))));
  const body = el('tbody');
  if (!rows || !rows.length) {
    body.appendChild(el('tr', null, el('td', { colspan: cols.length, class: 'empty' },
      EMPTY_ICON(), el('div', null, emptyText || t('nothing here')))));
  }
  for (const r of rows || []) {
    const attrs = Object.assign({}, (rowAttrs ? rowAttrs(r) : null) || {});
    const open = attrs.onclick;
    if (open) {
      // A whole row is a target, but its buttons still mean their own verb:
      // a click that starts on a control never opens the drawer.
      attrs.onclick = (ev) => { if (!ev.target.closest('button, a, select, input, textarea, label')) open(); };
      attrs.class = (attrs.class ? attrs.class + ' ' : '') + 'click';
      attrs.tabindex = '0';
      attrs.role = 'button';
      attrs.onkeydown = (ev) => {
        if (ev.key === 'Enter' || ev.key === ' ') { ev.preventDefault(); open(); }
      };
    }
    body.appendChild(el('tr', attrs, cols.map((c) => {
      const v = c.render ? c.render(r) : r[c.k];
      if (v === null || v === undefined || v === false) return el('td', { class: c.class || '' }, '—');
      if (typeof v === 'object' && v.nodeType) return el('td', { class: c.class || '' }, v);
      return el('td', { class: c.class || '' }, String(v));
    })));
  }
  node.appendChild(body);
  return el('div', { class: 'tbl-wrap' }, node);
}

const field = (label, ...nodes) => el('div', { class: 'field' }, el('label', null, label), ...nodes);
const hint = (txt) => el('span', { class: 'muted' }, txt);
const bar = (...nodes) => el('div', { class: 'bar' }, ...nodes);
/** Page head: one big title + one-line description per page, actions right.
    The .bar sections below it stay the small in-page headings. */
const pageHead = (title, desc, ...actions) => el('div', { class: 'page-head' },
  el('div', { class: 'ph-t' }, el('h2', null, title), desc ? el('p', { class: 'muted' }, desc) : null),
  actions.length ? el('div', { class: 'ph-a' }, ...actions) : null);
/** The view picker's client-side filter: a name box plus kind chips. Both are
    re-applied on every keystroke by renderViewList; ST.vFilter keeps them
    sticky while the user walks the detail pane. */
function viewFilterBar() {
  const q = el('input', {
    type: 'search', placeholder: t('filter by name…'),
    oninput: () => { ST.vFilter.q = q.value; renderViewList(); },
  });
  // Chip labels reuse real dictionary keys ('base views' is the overview card
  // label too); the KEY of a chip is the kind value it filters by.
  const KIND_CHIP = { all: 'all', base: 'base views', fork: 'forks', alias: 'aliases', derived: 'lenses' };
  const chips = el('div', { class: 'chips' }, ...Object.keys(KIND_CHIP).map((k) =>
    el('button', {
      type: 'button', 'data-kind': k, class: 'chip' + (ST.vFilter.kind === k ? ' on' : ''),
      onclick: (ev) => {
        ST.vFilter.kind = k;
        for (const c of ev.currentTarget.parentElement.children) c.classList.toggle('on', c === ev.currentTarget);
        renderViewList();
      },
    }, t(KIND_CHIP[k]))));
  return el('div', { class: 'filterbox' }, q, chips);
}
const when = (cond, ...nodes) => (cond ? nodes : null);

/* ------------------------------------------------------------ components --- */

const mono = (txt) => el('span', { class: 'mono' }, String(txt ?? ''));
/** Two facts in one cell: the one you scan, then the one you read. */
const cell2 = (top, sub) => el('div', { class: 'cell2' },
  el('div', null, top), sub ? el('div', { class: 'c-sub' }, sub) : null);

/**
 * The row drawer: a record's full field set in a second pane, so a list can
 * stay narrow (the columns you compare) while every long value (a note, a
 * timestamp chain, a refusal's detail text) is one click away and scroll
 * position and filters survive. It is NOT a dialog: nothing is confirmed here,
 * so no focus trap, closed by `Esc`, the scrim or its own X. Only one drawer
 * exists at a time — opening a second row replaces the first.
 */
let CLOSE_DRAWER = null;
function drawer(opts) {
  const host = $('drawer-host');
  const close = () => {
    host.hidden = true;
    clear(host);
    CLOSE_DRAWER = null;
    if (opts.onclose) opts.onclose();
  };
  CLOSE_DRAWER = close;
  const body = el('div', { class: 'db' });
  for (const sec of opts.sections || []) {
    const node = el('section', null, sec.h ? el('h3', { class: 'det-h' }, sec.h) : null);
    if (sec.kv && sec.kv.length) {
      // ONE dl per section, so the label column stays aligned down the block.
      const dl = el('dl', { class: 'kv' });
      for (const [k, v] of sec.kv) {
        dl.appendChild(el('dt', null, k));
        dl.appendChild(el('dd', null, (v === null || v === undefined || v === '') ? '—' : v));
      }
      node.appendChild(dl);
    }
    if (sec.node) node.appendChild(sec.node);
    body.appendChild(node);
  }
  const foot = el('div', { class: 'df' },
    opts.left ? el('span', { class: 'left' }, opts.left) : null,
    (opts.buttons || []).map((b) => b));
  host.hidden = false;
  clear(host).appendChild(el('div', { class: 'drawer-back', onclick: close }));
  host.appendChild(el('div', { class: 'drawer', role: 'dialog', 'aria-modal': 'false',
    'aria-label': String(opts.title || '') },
    el('div', { class: 'dh' },
      el('h2', { class: opts.titleClass || '' }, opts.title || ''),
      (opts.badges || []).map((b) => b),
      el('button', { type: 'button', class: 'x', onclick: close, 'aria-label': t('Close') }, '✕')),
    body, foot));
  return { close };
}
function closeDrawer() { if (CLOSE_DRAWER) CLOSE_DRAWER(); }
// `Esc` is the only keyboard affordance a second pane needs, and it belongs to
// the drawer itself rather than to whichever page happens to be open.
document.addEventListener('keydown', (ev) => { if (ev.key === 'Escape') closeDrawer(); });

/** "What does this button do", in the page. Collapsed by default: an operator
    who knows the verbs never pays for the explanation in scroll distance, and
    one who does not can search it (it is text in the DOM, not a title
    attribute nobody hovers). */
function helpPanel(summary, pairs) {
  const dl = el('dl', { class: 'gloss' });
  for (const [k, v] of pairs) {
    dl.appendChild(el('dt', null, k));
    dl.appendChild(el('dd', null, v));
  }
  return el('details', { class: 'help' }, el('summary', null, summary), el('div', { class: 'body' }, dl));
}

/**
 * The stored credential fingerprint: `a1b2…c3d4`. The elision is drawn, not
 * typed, so it cannot be mistaken for part of the value — and the title says
 * WHY the middle is missing: GroveFS stores a SHA-256 digest, so no view, no
 * API and no database dump can produce it. `apikey rotate` is the way to get a
 * usable secret back.
 */
function maskCell(hint) {
  if (!hint) return el('span', { class: 'muted' }, t('no stored fingerprint'));
  const cut = String(hint).indexOf('…');
  if (cut < 0) return el('span', { class: 'mask' }, String(hint));
  return el('span', { class: 'mask', title: t('first and last 4 hex digits of the secret; only its digest is stored, so the middle can never be shown again') },
    hint.slice(0, cut), el('i', null, '…'), hint.slice(cut + 1));
}

/** A chip row for a filter that has a fixed set of values. `dot` on an item
    names a COLOUR FAMILY (a view kind); a plain filter gets none, so the dot
    keeps meaning "kind" and nothing else. */
function chipGroup(items, cur, onPick) {
  return el('div', { class: 'chips' }, ...items.map((it) => el('button', {
    type: 'button',
    class: 'chip' + (it.dot ? '' : ' dotless') + (cur === it.v ? ' on' : ''),
    'data-kind': it.dot || null,
    onclick: () => onPick(it.v),
  }, it.t, it.n === undefined || it.n === null ? null : el('span', { class: 'c-sub' }, String(it.n)))));
}

/** One labelled control inside a filter bar. */
const fld = (label, ...nodes) => el('div', { class: 'fld' }, el('label', null, label), ...nodes);
/** The filter row every list page opens with. `count` is the live result
    count; the first control is the search box, which shares the top row with
    that count, while every labelled filter sits on its own row below — so a
    page's query and its facets never compete for one line. */
function filterBar(count, ...controls) {
  const q = controls.shift();
  const c = el('span', { class: 'count' }, count || '');
  const n = el('div', { class: 'filterbar' },
    el('div', { class: 'filterbox filter-top' }, q, c),
    controls.length ? el('div', { class: 'filterbox filter-opts' }, ...controls) : null);
  n.setCount = (txt) => { c.textContent = txt; };
  return n;
}
const counted = (shown, total) => (shown === total
  ? t('${n} in total', { n: total })
  : t('${n} of ${m} shown', { n: shown, m: total }));

/** Run an async action from a button: disabled while in flight, and a refusal
    becomes a toast (which wraps, so a two-line ACL sentence stays readable).
    Every mutating control in this file goes through it — a rejection that only
    reaches the developer console is a silent failure for the user. */
function action(label, fn, attrs) {
  return el('button', Object.assign({
    type: 'button',
    onclick: async (ev) => {
      const btn = ev.currentTarget;
      btn.disabled = true;
      try { await fn(btn); }
      catch (e) { toast(e.message || String(e), 'bad'); }
      finally { btn.disabled = false; }
    } }, attrs || {}), label);
}

/* ------------------------------------------------------------ transport --- */

async function req(path, opts) {
  const r = await fetch(path, Object.assign({ credentials: 'same-origin' }, opts));
  let j = null;
  try { j = await r.json(); } catch (e) { /* an empty or non-JSON answer */ }
  return { status: r.status, j };
}

/** One governance call. Throws with the SERVER's sentence, which callers show
    verbatim: a paraphrased refusal is a refusal nobody can act on. */
async function api(op, args) {
  if (!can(op)) throw new Error(`the console does not route '${op}' — it stays an operator CLI command`);
  const { status, j } = await req('/api/call', {
    method: 'POST',
    headers: { 'content-type': 'application/json', 'x-grovefs-csrf': ST.csrf || '' },
    body: JSON.stringify({ op, args: args || {} }),
  });
  if (status === 401) { signedOut(); throw new Error(t('signed out')); }
  if (!j) throw new Error(t('the gateway answered HTTP ${s} without a JSON body', { s: status }));
  if (j.ok === false) throw new Error(j.error || `HTTP ${status}`);
  return j.result;
}

/** A call whose refusal is worth rendering inline instead of as a toast (the
    overview page loads several things at once). */
async function soft(op, args) {
  try { return { ok: true, r: await api(op, args) }; }
  catch (e) { return { ok: false, error: String(e.message || e) }; }
}

const can = (op) => ST.ops.includes(op);
const isSuper = () => !!(ST.who && ST.who.super);

/* ----------------------------------------------------------------- boot --- */

async function boot() {
  const { status, j } = await req('/api/session');
  if (status === 200 && j && j.signedIn) {
    ST.csrf = j.csrf;
    ST.who = j.who;
    ST.ops = j.ops || [];
    ST.views = [];
    $('app').hidden = false;
    $('login').hidden = true;
    $('who-account').textContent = ST.who.account;
    const role = $('who-role');
    role.textContent = ST.who.role;
    role.className = 'badge ' + (ST.who.role === 'admin' ? 'admin' : '');
    await go(ST.page);
  } else {
    showLogin(j && j.error);
  }
}

function showLogin(msg) {
  $('app').hidden = true;
  $('login').hidden = false;
  $('login-btn').disabled = false;
  if (msg) $('login-error').textContent = msg;
  $('login-account').focus();
}

function signedOut() {
  ST.csrf = null;
  ST.who = null;
  showLogin(t('the session ended — sign in again'));
}

$('login-form').addEventListener('submit', async (ev) => {
  ev.preventDefault();
  const btn = $('login-btn');
  const err = $('login-error');
  err.textContent = '';
  btn.disabled = true;
  const body = {
    account: $('login-account').value.trim(),
    password: $('login-password').value,
  };
  const { status, j } = await req('/api/login', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body),
  });
  if (status === 200 && j && j.ok) {
    $('login-password').value = '';
    // Re-read the session rather than trusting the sign-in reply: `/api/session`
    // asks the server `whoami`, so the role, the delegations and the op list the
    // page is built from all come from one authenticated answer.
    await boot();
    return;
  }
  btn.disabled = false;
  err.textContent = (j && j.error) || t('sign-in failed (HTTP ${s})', { s: status });
});

$('logout').addEventListener('click', async () => {
  try { await req('/api/logout', { method: 'POST', headers: { 'x-grovefs-csrf': ST.csrf || '' } }); }
  catch (e) { /* the session is gone either way */ }
  ST.csrf = null;
  ST.who = null;
  showLogin();
  toast(t('signed out'));
});

$('nav').addEventListener('click', (ev) => {
  const b = ev.target.closest('button[data-page]');
  if (b) go(b.dataset.page);
});

async function go(page) {
  ST.page = page;
  for (const b of $('nav').querySelectorAll('button')) b.classList.toggle('on', b.dataset.page === page);
  for (const s of document.querySelectorAll('.page')) s.hidden = s.id !== 'page-' + page;
  const root = clear($('page-' + page));
  root.appendChild(hint(t('loading…')));
  const fn = { overview: pageOverview, views: pageViews, keys: pageKeys, accounts: pageAccounts, audit: pageAudit }[page];
  try {
    clear(root);
    await fn(root);
  } catch (e) {
    clear(root).appendChild(el('p', { class: 'err' }, String(e.message || e)));
  }
}

const reload = () => {
  // A drawer is a snapshot of ONE row; after any reload its record may be
  // edited, revoked or gone, and a stale pane beside fresh rows is worse than
  // no pane.
  closeDrawer();
  return go(ST.page);
};

/** The view list, needed by three pages (bindings, delegations, the picker).
    Cached in ST.views and refreshed on every page load, because a base view
    created in another tab is a thing people expect to see immediately. */
async function loadViews(force) {
  if (ST.views.length && !force) return ST.views;
  const [phys, defs] = await Promise.all([api('physical.list', {}), api('def.list', {})]);
  ST.views = mergeViews(phys || [], defs || []);
  return ST.views;
}

/** physical.list and def.list are two views of one namespace: a name can have a
    tree, a definition, or (a fork) both. `view.stats` is the authority on the
    kind, but it costs a round trip per name — this merge is the cheap
    approximation the LIST uses, refined for the selected row. */
function mergeViews(phys, defs) {
  const out = new Map();
  for (const p of phys) out.set(p, { name: p, kind: 'base', base: p });
  for (const d of defs) {
    const [name, text] = Array.isArray(d) ? d : [d.name, d.text];
    const kind = defKind(text);
    out.set(name, { name, kind, base: defBase(text) });
  }
  return [...out.values()].sort((a, b) => a.name.localeCompare(b.name));
}

function defFirst(text, kw) {
  for (const line of String(text || '').split('\n')) {
    const ln = line.trim();
    if (ln.startsWith(kw + ' ') || ln.startsWith(kw + '\t')) return ln.split(/\s+/)[1];
  }
  return null;
}
const defKind = (text) => (defFirst(text, 'alias') ? 'alias'
  : defFirst(text, 'fork') ? 'fork'
  : defFirst(text, 'base') ? 'derived' : 'other');
const defBase = (text) => defFirst(text, 'alias') || defFirst(text, 'fork') || defFirst(text, 'base');

/* -------------------------------------------------------------- overview --- */

async function pageOverview(root) {
  root.appendChild(pageHead(t('Overview'), t('Deployment snapshot, your reach and the latest governance activity.')));
  const info = await api('info', {});
  const cards = el('div', { class: 'cards' });
  /** One deployment figure with its name and what it is measured against. The
      grid puts FOUR of these per row, so the eight numbers an operator scans
      land in two rows instead of a width-dependent ribbon. `tone` is STATE
      only (amber = act on it) — colour never decorates a number here. */
  const card = (k, v, sub, page, tone) => cards.appendChild(el('div', {
    // A count gets the display figure; a NAME (the backend, the signed-in
    // account) is set as a heading so the card grid reads with one rhythm.
    class: 'card' + (page ? ' link-card' : '') + (tone ? ' ' + tone : '')
      + (typeof v === 'string' && !/^[\d.,%\s—–-]+$/.test(v) ? ' text' : ''),
    ...(page ? { onclick: () => go(page), onkeydown: (ev) => { if (ev.key === 'Enter') go(page); }, tabindex: '0', role: 'button' } : {}),
  },
    el('div', { class: 'k' }, k),
    el('div', { class: 'n' }, (v === null || v === undefined) ? '—' : String(v)),
    sub ? el('div', { class: 's' }, sub) : null));

  card(t('backend'), info.backend, t('one store for metadata and content'), null, 'mute');
  card(t('signed in as'), info.actor || ST.who.account, info.role || '', null, 'mute');

  if (isSuper()) {
    const si = await api('store.info', {});
    const expired = si.expiredKeys || 0;
    card(t('base views'), si.views, t('the trees mounts open'), 'views');
    card(t('definitions'), si.definitions, t('forks, aliases and lenses'), 'views');
    card(t('accounts'), si.accounts, t('principles on the governance plane'), 'accounts');
    card(t('live keys'), si.liveKeys, expired
      ? t('${n} expired and refused', { n: expired }) : t('none expired'), 'keys', expired ? 'warn' : 'ok');
    card(t('mount sessions'), si.liveSessions, t('clients reading trees now'), 'views');
    card(t('open trees'), si.residentEngines, t('${n} names resident here', { n: si.residentViews }), 'views');
    root.appendChild(cards);

    // The deployment panel. The old version was four bare sentences; this one
    // is a table of the facts an operator actually quotes in a ticket — the
    // backend WITH its describe line, the namespace SPLIT into trees and
    // definitions, credentials SPLIT into live and expired — plus the two
    // things a reader always asked about the old wording: which process this
    // answer came from, and where the first account came from.
    root.appendChild(bar(el('h2', null, t('This deployment')),
      hint(t('read live from the meta-server that answered this page'))));
    const line = (lbl, ...val) => el('div', { class: 'stat-line' },
      el('span', { class: 'lbl' }, lbl), el('span', { class: 'val' }, ...val));
    root.appendChild(el('div', { class: 'detail' },
      el('div', { class: 'grid2' },
        line(t('storage backend'), el('b', null, String(info.backend || '—')),
          el('span', { class: 'muted' }, '· ' + String(si.describe || ''))),
        line(t('metadata root'), el('code', null, String(si.base || '—'))),
        line(t('server role'), el('b', null, String(si.role || '—')),
          el('span', { class: 'muted' }, t('the process this console is talking to'))),
        line(t('namespace'), el('b', null, t('${n} base view(s)', { n: si.views })),
          el('span', { class: 'muted' }, t('· ${n} definition(s)', { n: si.definitions }))),
        line(t('credentials'), el('b', null, t('${n} live', { n: si.liveKeys })),
          el('span', { class: 'muted' }, t('· ${n} expired · a revocation or expiry lands on the NEXT call', { n: expired }))),
        line(t('governance accounts'), el('b', null, String(si.accounts ?? '—')),
          el('span', { class: 'muted' }, t('· the first owner was created by `meta-server --admin-user`'))),
        line(t('activity on this server'), el('b', null, t('${n} mount session(s)', { n: si.liveSessions })),
          el('span', { class: 'muted' }, t('· ${n} tree(s) open in memory', { n: si.residentEngines })))),
      el('p', { class: 'muted' },
        t('Every count is a snapshot of the instant this page loaded; anything created elsewhere appears after a refresh.'))));
  } else {
    card(t('base views you govern'), (ST.who.bases || []).length, t('and everything derived from them'), 'views');
    root.appendChild(cards);
    root.appendChild(bar(el('h2', null, t('Your reach'))));
    root.appendChild(el('p', { class: 'muted' },
      t('Deployment-wide counts belong to an administrator. You govern these base views and everything derived from them:')));
    const list = el('ul', { class: 'list' });
    for (const b of (ST.who.bases || []).slice(0, 50)) {
      const s = await soft('view.stats', { view: b });
      list.appendChild(el('li', { onclick: () => { ST.sel = b; go('views'); } },
        el('span', { class: 'mono' }, b),
        el('span', { class: 'kind' }, s.ok ? t('${n} derived', { n: (s.r.family || []).length }) : t('unreachable'))));
    }
    root.appendChild(list);
  }

  const calls = await soft('apikey.calls', { limit: 8 });
  root.appendChild(bar(el('h2', null, t('Recent governance calls')),
    when(calls.ok, el('button', { type: 'button', class: 'mini', onclick: () => go('audit') }, t('open the audit page')))));
  root.appendChild(calls.ok ? auditTable(calls.r) : el('p', { class: 'err' }, calls.error));
}

/** ISO stamps are half a table wide; show MM-DD HH:MM:SS and keep the full
    stamp one hover away (title). */
const shortTime = (ts) => (typeof ts === 'string' && ts.length >= 19
  ? ts.slice(5, 19).replace('T', ' ') : String(ts ?? ''));
const shortStampCell = (ts) => el('span', { title: ts || '' }, shortTime(ts));
/** A cell that must stay one line tall even when the server's sentence is not.
    The whole text is one click away in the drawer, so nothing is lost — it is
    just not allowed to be six lines tall in a table of forty rows. */
const clip = (txt, n) => {
  const s = String(txt ?? '');
  return s.length > n ? s.slice(0, n) + '…' : s;
};

/**
 * WHO made the call. The audit row itself has no account column: the control
 * plane prefixes a signed-in call's `detail` with `account=<who>@<ip>`, so the
 * person is parsed back out here — "who did this" is the first question anyone
 * asks of an audit log, and a table that cannot answer it is a text file.
 * A call made by a credential instead (a mount, an agent) has no account, and
 * says so with the key that signed.
 */
function auditActor(r) {
  const m = /account=([^\s@]+)/.exec(String(r.detail || ''));
  if (m) return m[1];
  return r.key_label || r.key_id || null;
}
const auditSig = (r) => (auditActor(r)
  ? cell2(auditActor(r), r.key_id ? t('key ${id}', { id: r.key_id }) : null)
  : el('span', { class: 'muted' }, t('no account on it')));

function auditTable(rows, emptyText) {
  return tableNode([
    { t: t('when'), render: (r) => shortStampCell(r.ts), class: 'mono nowrap' },
    { t: t('who'), render: auditSig, class: 'nowrap' },
    { t: t('plane'), k: 'method', class: 'mono' },
    { t: t('op'), k: 'tool', class: 'mono' },
    {
      t: t('result'), class: 'nowrap',
      render: (r) => el('span', { class: 'badge ' + (r.ok ? 'good' : 'bad'), title: r.ok ? '' : clip(r.detail, 400) },
        r.ok ? t('ok') : t('failed')),
    },
    { t: t('detail'), render: (r) => clip(r.detail, 110), class: 'wrap' },
  ], rows, emptyText || t('no governance calls yet'),
  (r) => ({ onclick: () => auditDrawer(r) }));
}

/** An audit row's full record: the whole message the server wrote (its line
    breaks and its indentation separate "why" from "what to do next", so it is
    shown as text, not wrapped into one grey paragraph), and who/what/where. */
function auditDrawer(r) {
  return drawer({
    title: r.tool || r.method || t('governance call'),
    badges: [el('span', { class: 'badge ' + (r.ok ? 'good' : 'bad') }, r.ok ? t('ok') : t('failed'))],
    sections: [
      {
        kv: [
          [t('when'), el('span', { class: 'mono' }, String(r.ts || '—'))],
          [t('who'), auditActor(r) || el('span', { class: 'muted' }, t('no account on it'))],
          [t('plane'), mono(r.method)],
          [t('op'), mono(r.tool)],
          [t('key'), r.key_label ? cell2(r.key_label, mono(r.key_id)) : (r.key_id ? mono(r.key_id) : null)],
        ],
      },
      { h: t('What the server said'), node: el('pre', { class: 'plain' }, String(r.detail || t('nothing recorded'))) },
    ],
  });
}

/* ------------------------------------------------------------------ views --- */

async function pageViews(root) {
  await loadViews(true);
  root.appendChild(pageHead(t('Views'), t('Base views, forks, aliases and lenses — select one to inspect.'),
    action(t('refresh'), reload, { class: 'link' })));
  root.appendChild(el('div', { class: 'split' },
    el('div', { class: 'col-narrow' },
      bar(el('h2', null, t('View list')), el('span', { class: 'count-pill', id: 'view-count' })),
      viewFilterBar(),
      el('ul', { id: 'view-list', class: 'list' }),
      el('div', { id: 'view-new', class: 'panel' })),
    el('div', { class: 'col-wide', id: 'view-detail' })));
  await renderViewList();
  await renderDetail(ST.sel);
}

/** The API's kind spelling is the JSON's; the console's own noun for a
    `derived` view is a LENS (the chip row, the badges and the help text all say
    so), so the list and the tree say what the rest of the page says. */
const KIND_LABEL = { base: 'base view', fork: 'fork', alias: 'alias', derived: 'lens', other: 'definition' };
const kindName = (k) => t(KIND_LABEL[k] || k || 'other');

/** Selecting a view re-renders both panes: the list keeps the selection stripe
    and the detail pane is what the click was for. */
const pickView = (name) => { ST.sel = name; closeDrawer(); renderViewList().then(() => renderDetail(name)); };

/** DIRECT children in the merged namespace: the parent reference written into
    the definition (`alias X`, `fork Y`, `base Z`). `view.stats.family` is the
    server's authoritative flat list of every name that RESOLVES through a base
    — this is the EDGE list, which is what turns that pile of names into a tree
    with a level per derivation. */
const viewChildren = (name) => ST.views.filter((v) => v.name !== name && v.base === name);

/** The chain ABOVE `name` (a lens on a fork on a base). Cycle-guarded, because
    two hand-written definitions can point at each other, and a console that
    spins on a data bug is worse than one that draws the tree shallowly. */
function viewAncestry(name) {
  const chain = [];
  const seen = new Set([name]);
  let cur = name;
  while (chain.length < 12) {
    const v = ST.views.find((x) => x.name === cur);
    const parent = v ? v.base : null;
    if (!parent || parent === cur || seen.has(parent)) break;
    chain.push(parent);
    seen.add(parent);
    cur = parent;
  }
  return chain;
}

const TREE_DEPTH = 6;
/** One level of the descent, as nested `<ul>` in the `.tree` language. `seen`
    is shared down the whole walk, so a name reached by two edges is drawn once
    (a real possibility: an alias and a lens over the same fork). */
function lineageDown(name, seen, depth, cur) {
  const kids = depth >= TREE_DEPTH ? [] : viewChildren(name).filter((v) => !seen.has(v.name));
  if (!kids.length) {
    return depth >= TREE_DEPTH && viewChildren(name).length
      ? el('ul', null, el('li', { class: 'muted' }, t('deeper than ${d} levels — expand it from its own page', { d: TREE_DEPTH })))
      : null;
  }
  for (const k of kids) seen.add(k.name);
  return el('ul', null, ...kids.map((v) => el('li', { class: 'k-' + v.kind + ' clickable' },
    el('span', { class: 't-name' + (v.name === cur ? ' t-cur' : ''), onclick: () => pickView(v.name) }, v.name),
    el('span', { class: 't-meta' }, kindName(v.kind)),
    lineageDown(v.name, seen, depth + 1, cur))));
}

/** The lineage panel of the detail pane: this view, its parents above it and
    everything derived from it below. Answering "what breaks if I move this
    base" is a shape on screen instead of a mental model of the namespace. */
function lineagePanel(s) {
  const name = s.name;
  const up = viewAncestry(name);
  const seen = new Set([name]);
  const root = el('li', { class: 'k-' + s.kind },
    el('span', { class: 't-name t-cur' }, name),
    el('span', { class: 't-meta' }, kindName(s.kind)));
  const kids = lineageDown(name, seen, 1, null);
  if (kids) root.appendChild(kids);
  const fam = (s.family || []).length;
  const out = el('div', null);
  if (up.length) {
    out.appendChild(el('div', { class: 'cell-badges' },
      el('span', { class: 'muted' }, t('resolves through')), el('span', null, '←'),
      ...up.map((n) => el('button', { type: 'button', class: 'mini mono', onclick: () => pickView(n) }, n))));
  }
  out.appendChild(el('ul', { class: 'tree' }, root));
  out.appendChild(el('p', { class: 'muted' }, s.kind === 'base' && fam
    ? t('${n} name(s) resolve through this base — ${d} directly below it', { n: fam, d: kids ? kids.childNodes.length : 0 })
    : (kids ? t('${n} name(s) hang below it', { n: seen.size - 1 }) : t('nothing is derived from it yet'))));
  return out;
}

async function renderViewList() {
  const ul = $('view-list');
  if (!ul) return;
  clear(ul);
  const total = ST.views.length;
  if (!total) {
    ul.appendChild(el('li', { class: 'muted' }, t('no views you can see')));
    $('view-count').textContent = counted(0, 0);
    return;
  }
  const q = (ST.vFilter.q || '').trim().toLowerCase();
  const rows = ST.views.filter((v) =>
    (ST.vFilter.kind === 'all' || v.kind === ST.vFilter.kind)
    && (!q || v.name.toLowerCase().includes(q)));
  if (!rows.length) {
    ul.appendChild(el('li', { class: 'muted' }, t('no views match the filter')));
  }
  for (const v of rows) {
    // A base view's badge counts the names that hang DIRECTLY below it: the
    // number an operator needs before touching or deleting anything.
    const below = v.kind === 'base' ? viewChildren(v.name).length : 0;
    ul.appendChild(el('li', {
      class: (ST.sel === v.name ? 'sel ' : '') + 'k-' + v.kind,
      onclick: () => pickView(v.name),
    }, el('span', { class: 'mono' }, v.name),
      below ? el('span', { class: 'count-pill', title: t('names derived directly from it') }, String(below)) : null,
      el('span', { class: 'kind' }, kindName(v.kind))));
  }
  $('view-count').textContent = counted(rows.length, total);
}

async function renderDetail(name) {
  const host = $('view-detail');
  if (!host) return;
  clear(host);
  await renderNewBase();
  if (!name) {
    host.appendChild(hint(t('pick a view on the left')));
    return;
  }
  const s = await api('view.stats', { view: name });
  const box = el('div', { class: 'detail' });
  box.appendChild(el('div', { class: 'head' },
    el('h2', { class: 'mono' }, s.name),
    el('span', { class: 'badge k-' + s.kind }, kindName(s.kind)),
    when(s.sessions > 0, el('span', { class: 'badge good' }, t('${n} mount session(s)', { n: s.sessions }))),
    when(s.kind === 'base' && (s.family || []).length,
      el('span', { class: 'count-pill', title: t('names that resolve through this base') },
        t('${n} derived', { n: s.family.length })))));

  // Two blocks side by side: what this name IS, and what hangs off it. The old
  // page put the derived list in the same flat dl as the stamp, where it read
  // as one more string instead of the reason to be careful.
  box.appendChild(el('div', { class: 'grid2' },
    el('div', null,
      el('h3', { class: 'det-h' }, t('This name')),
      el('dl', { class: 'kv' },
        el('dt', null, t('base')), el('dd', { class: 'mono' }, s.base || '—'),
        el('dt', null, t('stamp')), el('dd', { class: 'mono' }, stampText(s.stamp)),
        el('dt', null, t('tree')), el('dd', null, s.tree ? t('physical tree') : s.forkTree ? t('fork tree') : t('none of its own')),
        el('dt', null, t('on this server')), el('dd', { class: 'muted' }, s.resident ? t('open') : t('not open')),
        el('dt', null, t('keys')), el('dd', null, (s.keys === null || s.keys === undefined) ? '—' : t('${n} bound to this name', { n: s.keys })))),
    el('div', null,
      el('h3', { class: 'det-h' }, t('Lineage')),
      lineagePanel(s))));

  if (s.definition) {
    box.appendChild(el('h3', { class: 'det-h' }, t('Definition')));
    box.appendChild(el('pre', null, s.definition));
  }
  await renderViewActions(box, s);
  host.appendChild(box);
}

function stampText(st) {
  if (st === null || st === undefined) return '—';
  if (typeof st === 'object') return Object.entries(st).map(([k, v]) => `${k}=${v}`).join('  ');
  return String(st);
}

/**
 * The per-view buttons. Anything the ACL can refuse is OFFERED and let the
 * server refuse: a hidden button teaches nothing about why, and the refusal
 * names the delegation that would change it.
 *
 * Two of these are two-step verbs, ordered so that the intermediate state is
 * the harmless one:
 *  - drop a fork: TREE FIRST, then the definition. Reversing it would strand a
 *    tree whose name no longer resolves to a base — and a delegated owner would
 *    lose the rights that justify the second call before it is made.
 *  - create a fork: ONE call. `with_def` makes the server record `fork <base>`
 *    beside seeding the tree, so closing a tab mid-dialog cannot leave a tree
 *    that no name resolves to.
 */
async function renderViewActions(box, s) {
  const name = s.name;
  box.appendChild(el('div', { class: 'row' },
    el('button', {
      type: 'button',
      onclick: () => { window.location = `/api/export?view=${encodeURIComponent(name)}&csrf=${encodeURIComponent(ST.csrf)}`; },
    }, t('Export dump')),
    when(can('fork.create'), el('button', { type: 'button', onclick: () => forkDialog(s) }, t('Fork…'))),
    when(can('def.put'), el('button', { type: 'button', onclick: () => defineDialog(s) }, t('Lens / alias…'))),
    when(s.kind === 'fork' && can('fork.drop'), el('button', {
      type: 'button', class: 'danger',
      onclick: async () => {
        if (!(await ask(t('Drop this fork?'),
          t('${name}: its own tree is deleted. Its base ${b} and every shared object stay.', { name, b: s.base || '' }), t('Drop fork')))) return;
        await api('fork.drop', { name });
        try { await api('def.remove', { name }); }
        catch (e) { toast(t('the fork tree is gone, but its definition could not be removed: ${e}', { e: e.message }), 'bad'); }
        ST.sel = null;
        await reload();
      },
    }, t('Drop fork'))),
    when(s.kind !== 'base' && can('def.remove'), el('button', {
      type: 'button', class: 'danger',
      onclick: async () => {
        if (!(await ask(t('Remove this definition?'),
          t('${name} =\n${def}\n\nOnly the name goes away — no tree and no object is deleted.', { name, def: s.definition || '' }), t('Remove definition')))) return;
        await api('def.remove', { name });
        toast(t('definition removed'));
        await reload();
      },
    }, t('Remove definition')))));

  await renderImport(box, s);
  if (can('apikey.create')) await renderKeyForView(box, s);
  if (can('def.put') && s.kind !== 'base') await renderDefEdit(box, s);
}

function forkDialog(s) {
  const nameI = el('input', { placeholder: `${s.name}-exp1`, size: 24 });
  const seed = el('input', { type: 'checkbox' });
  seed.checked = true;
  const m = modal(t('Fork ${name}', { name: s.name }), wrap(
    hint(t('A fork copies the METADATA tree into a new writable view; content bytes stay shared until something in the fork rewrites them.')),
    field(t('new view name'), nameI),
    el('label', null, seed, ' ' + t('copy the current tree (unchecked: an empty fork to graft into)'))),
  [{
    t: t('Create fork'),
    onclick: async (btn, err) => {
      err.textContent = '';
      try {
        const r = await api('fork.create', { base: s.name, name: nameI.value.trim(), seed: seed.checked, with_def: true });
        m.close();
        toast(t('forked ${a} → ${b} (${n} nodes, ${m} blobs)', { a: s.name, b: nameI.value.trim(), n: r.nodes, m: r.blobs }));
        ST.sel = nameI.value.trim();
        await reload();
      } catch (e) { err.textContent = e.message; }
      finally { btn.disabled = false; }
    },
  }]);
}

function defineDialog(s) {
  const nameI = el('input', { placeholder: `${s.name}-lens`, size: 24 });
  const kindS = el('select', null,
    el('option', { value: 'derived' }, t('derived lens (read-only shape over a base)')),
    el('option', { value: 'alias' }, t('alias (another name for it)')));
  const textT = el('textarea', { rows: 8, spellcheck: 'false' });
  const preset = () => {
    textT.value = kindS.value === 'alias'
      ? `alias ${s.name}\n`
      : `base ${s.name}\n# hide /subtree\n# only /shown\n# map /src/path /shown/path\n`;
  };
  kindS.addEventListener('change', preset);
  preset();
  const m = modal(t('Define a view over ${name}', { name: s.name }), wrap(
    hint(t('The definition text is exactly what `grovefs view define --file` takes.')),
    field(t('kind'), kindS),
    field(t('new view name'), nameI),
    field(t('definition'), textT)),
  [{
    t: t('Save'),
    onclick: async (btn, err) => {
      err.textContent = '';
      try {
        await api('def.put', { name: nameI.value.trim(), text: textT.value });
        m.close();
        toast(t('definition saved'));
        ST.sel = nameI.value.trim();
        await reload();
      } catch (e) { err.textContent = e.message; }
      finally { btn.disabled = false; }
    },
  }]);
}

/** The administrator-only "new base view" box, under the picker. */
async function renderNewBase() {
  const panel = $('view-new');
  if (!panel) return;
  clear(panel);
  if (!can('view.create')) {
    panel.appendChild(hint(t('creating a BASE view is an administrator’s act — it chooses where bytes live.')));
    return;
  }
  const nameI = el('input', { placeholder: 'project-x', size: 16 });
  const instI = el('input', { placeholder: '(least loaded)', size: 14 });
  panel.appendChild(el('h3', null, t('New base view')));
  panel.appendChild(field(t('name'), nameI));
  panel.appendChild(field(t('PG instance'), instI));
  panel.appendChild(action(t('Create'), async (btn) => {
    const args = { view: nameI.value.trim() };
    if (instI.value.trim()) args.pg_instance = instI.value.trim();
    try {
      const r = await api('view.create', args);
      toast(typeof r === 'string' ? r : t('view created'));
      ST.sel = nameI.value.trim();
      await reload();
    } catch (e) {
      // The server's own reason ("already has a definition — remove it first")
      // is the useful part, so it goes in the toast rather than the dialog.
      btn.disabled = false;
      throw e;
    }
  }, { class: 'primary' }));
  panel.appendChild(hint(t('a base view owns physical deletion; lenses, aliases and forks reshape one')));
}

/**
 * Upload a dump. A delegated account is pinned to `as_fork` of a base it
 * governs: importing WITHOUT that argument creates a base view, which the server
 * refuses for anyone but an administrator — the UI says so up front instead of
 * letting the refusal arrive after a 40 MB upload.
 */
async function renderImport(box, s) {
  const row = el('div', { class: 'row' });
  const fileI = el('input', { type: 'file', accept: '.txt,.json,.ndjson,text/plain' });
  const target = el('input', { placeholder: `${s.name}-copy`, size: 18 });
  const repl = el('input', { type: 'checkbox' });
  const asBase = el('input', { type: 'checkbox' });
  row.appendChild(field(t('metadata dump'), fileI));
  row.appendChild(field(t('as view'), target));
  row.appendChild(el('label', null, repl, ' ' + t('replace an existing tree')));
  if (isSuper()) row.appendChild(el('label', null, asBase, ' ' + t('import as a BASE view')));
  row.appendChild(action(t('Import'), async (btn) => {
    const f = fileI.files && fileI.files[0];
    if (!f) throw new Error(t('choose a dump file first'));
    const q = new URLSearchParams({ view: target.value.trim(), csrf: ST.csrf });
    if (!isSuper() || !asBase.checked) q.set('as_fork', s.base || s.name);
    if (repl.checked) q.set('replace', '1');
    const r = await fetch('/api/import?' + q.toString(), {
      method: 'POST',
      credentials: 'same-origin',
      headers: { 'content-type': 'application/octet-stream', 'x-grovefs-csrf': ST.csrf },
      body: f,
    });
    let j = null;
    try { j = await r.json(); } catch (e) { /* a proxy error page */ }
    if (!j || j.ok === false) throw new Error((j && j.error) || t('import failed (HTTP ${s})', { s: r.status }));
    toast(t('imported ${n} nodes, ${m} blobs as ${kind}', { n: j.result.nodes, m: j.result.blobs, kind: j.result.kind }));
    ST.sel = target.value.trim();
    await reload();
  }));
  box.appendChild(bar(el('h3', null, t('Import a dump'))));
  box.appendChild(row);
  box.appendChild(hint(isSuper()
    ? t('a dump becomes a BASE view, or a fork of a view you govern — content bytes are never in a dump')
    : t('imported as a FORK of ${name}; only an administrator can import a new base view', { name: s.base || s.name })));
}

/** Keys bound to THIS view: a view owner can rotate what its mounts use without
    walking the deployment-wide list. */
async function renderKeyForView(box, s) {
  const all = await api('apikey.list', {});
  const mine = (all || []).filter((k) => k.view === s.name);
  box.appendChild(bar(el('h3', null, t('Keys bound to ${name}', { name: s.name }))));
  // The SAME column set and verbs as the keys page: a view owner who lands here
  // is looking at the registry from the other side, not at a smaller app.
  box.appendChild(tableNode(keyCols(true), mine, t('no keys bound to this view yet')));
}

async function renderDefEdit(box, s) {
  const ta = el('textarea', { rows: 6, spellcheck: 'false' });
  ta.value = s.definition || '';
  box.appendChild(bar(el('h3', null, t('Edit this definition'))));
  box.appendChild(ta);
  box.appendChild(action(t('Save'), async () => {
    await api('def.put', { name: s.name, text: ta.value });
    toast(t('saved — live mounts pick it up within their attr TTL (≤2s)'));
    await renderDetail(s.name);
  }));
}

/* ------------------------------------------------------------------ keys --- */

/**
 * Where a credential stands, as ONE word. Four buckets, three colours:
 * live (green), live-but-close-to-its-deadline (amber), refused (red).
 * `expired` is the SERVER's verdict — the same instant comparison the
 * handshake makes — so this page can never call a key live that its next
 * mount will refuse, and a browser in another timezone cannot disagree with
 * itself about an RFC 3339 string.
 */
function keyBucket(r) {
  if (r.revoked_at) return 'revoked';
  if (r.expired) return 'expired';
  if (r.expires_at && Date.parse(r.expires_at) - Date.now() < 7 * 864e5) return 'expiring';
  return 'live';
}
const KEY_BUCKETS = ['all', 'live', 'expiring', 'expired', 'revoked'];
function keyState(r) {
  const b = keyBucket(r);
  const tone = b === 'revoked' || b === 'expired' ? 'bad' : b === 'expiring' ? 'warn' : 'good';
  const label = b === 'expired' ? t('expired') : b === 'revoked' ? t('revoked')
    : b === 'expiring' ? t('expiring soon') : t('live');
  const why = b === 'revoked' ? t('revoked ${at}', { at: r.revoked_at })
    : b === 'expired' ? t('it expired at ${at}', { at: r.expires_at })
    : b === 'expiring' ? t('it expires at ${at}', { at: r.expires_at })
    : r.expires_at ? t('it expires at ${at}', { at: r.expires_at }) : t('no deadline');
  return el('span', { class: 'badge ' + tone, title: why }, label);
}
/** The second line under the badge: the deadline itself, in words. */
const expiryLine = (r) => (r.revoked_at ? t('revoked ${at}', { at: shortTime(r.revoked_at) })
  : !r.expires_at ? t('never expires')
  : r.expired ? t('expired ${when}', { when: shortTime(r.expires_at) })
  : t('expires ${when}', { when: shortTime(r.expires_at) }));

/** An account as a person, not a bare token: initial, name, and the one fact
    that distinguishes two people (what it did here). */
function whoCell(name, sub, off) {
  if (!name) return el('span', { class: 'muted' }, t('unassigned'));
  return el('div', { class: 'who-cell' },
    el('span', { class: 'avatar' + (off ? ' off' : '') }, String(name).slice(0, 2).toUpperCase()),
    el('div', null, el('div', { class: 'nm mono' }, name), sub ? el('div', { class: 'sub2' }, sub) : null));
}

/** The ttl choices, in the CLI's own spelling (`--ttl 7d`). Weeks are absent
    because the parser has no `w` — offering `1w` here would be offering an
    error message. */
const TTL_CHOICES = [['', 'never expires'], ['1h', '1 hour'], ['12h', '12 hours'], ['24h', '1 day'],
  ['7d', '7 days'], ['30d', '30 days'], ['90d', '90 days'], ['365d', '1 year']];
/** Same limit as `apikeys::NOTE_MAX` — the server still refuses a longer note
    with a named limit, this only stops a paste from looking accepted. */
const NOTE_MAX = 500;
const ttlSelect = (withKeep) => {
  const s = el('select', null,
    ...(withKeep ? [el('option', { value: '__keep' }, t('keep unchanged'))] : []),
    ...TTL_CHOICES.map(([v, label]) => el('option', { value: v }, t(label))));
  return s;
};

/** What the table will show for this secret from now on: the same head and
    tail the store computes server-side. Reproduced here ONLY in the one-time
    dialog, where the plaintext is already on the screen — it proves to the
    reader which fingerprint their row will carry. */
const maskOf = (full) => {
  const s = String(full);
  const secret = s.slice(s.indexOf(':') + 1);
  return secret.slice(0, 4) + '…' + secret.slice(-4);
};

/** The key table's columns, in the order an operator reads them: what it is
    called and what it is for, which credential it IS (a fingerprint, never a
    secret), what it can open, whose it is, whether it still works, when it was
    last seen, then the verbs. `renderKeyForView` on the views page reuses
    this list, so a key never looks different depending on which page found it
    — only the column that would repeat the page's own heading is dropped. */
function keyCols(scopedToView) {
  const cols = [
    {
      t: t('label'), class: 'nowrap',
      render: (r) => cell2(el('span', { class: 'nm' }, r.label), mono(r.id)),
    },
    { t: t('key'), render: (r) => maskCell(r.secret_hint) },
    {
      t: t('note'), class: 'wrap',
      render: (r) => (r.note ? r.note : el('span', { class: 'muted' }, t('no note'))),
    },
    { t: t('plane'), k: 'scope', class: 'mono' },
    { t: t('view'), render: (r) => (r.view ? mono(r.view) : el('span', { class: 'muted' }, t('any view'))) },
    {
      t: t('mode'),
      render: (r) => el('span', { class: 'badge' + (r.ro ? ' warn' : ''), title: r.ro ? t('a mount with it cannot write') : t('it can write') },
        r.ro ? t('read-only') : t('read-write')),
    },
    { t: t('owner'), render: (r) => whoCell(r.owner, r.created_by ? t('minted by ${who}', { who: r.created_by }) : null, !!r.revoked_at) },
    { t: t('state'), class: 'nowrap', render: (r) => cell2(keyState(r), expiryLine(r)) },
    { t: t('last used'), render: (r) => shortStampCell(r.last_used_at), class: 'muted nowrap' },
    {
      t: '', class: 'actions',
      render: (r) => (r.revoked_at ? null : el('div', { class: 'row' },
        roBtn(r), when(can('apikey.update'), keyEditBtn(r)), rotateBtn(r), revokeBtn(r))),
    },
  ];
  return scopedToView ? cols.filter((c) => c.t !== t('view')) : cols;
}

/** "3 d", "19 h", "40 m": how far off a deadline is, for a reader deciding
    whether today is the day to act. Negative and unparseable both answer in
    words rather than a number, because both mean the key is already refused. */
function humanLeft(iso) {
  const ms = Date.parse(iso) - Date.now();
  if (!isFinite(ms)) return t('unknown');
  if (ms <= 0) return t('already past');
  for (const [unit, size] of [['d', 864e5], ['h', 36e5], ['m', 6e4]]) {
    if (ms >= size) return Math.floor(ms / size) + ' ' + unit;
  }
  return t('less than a minute');
}

async function pageKeys(root) {
  await loadViews();
  root.appendChild(pageHead(t('API keys'), t('Mint, rotate and revoke the keys clients mount and agents call with.'),
    action(t('refresh'), reload, { class: 'link' })));

  const labelI = el('input', { placeholder: 'etl job', size: 16 });
  const noteI = el('input', { type: 'text', placeholder: t('what this key is for…'), size: 24, maxlength: NOTE_MAX });
  const scopeS = el('select', null, ...(isSuper()
    ? [{ v: 'mount', t: t('mount — a client mount') }, { v: 'mcp', t: t('mcp — an agent') }, { v: 'admin', t: t('admin — the governance plane') }, { v: 'any', t: t('any — every plane') }]
    : [{ v: 'mount', t: t('mount — a client mount') }, { v: 'mcp', t: t('mcp — an agent') }]).map((o) => el('option', { value: o.v }, o.t)));
  const viewS = el('select', null,
    el('option', { value: '' }, isSuper() ? t('(no view binding — every tree)') : t('(choose a view)')),
    ...ST.views.map((v) => el('option', { value: v.name }, `${v.name} [${v.kind}]`)));
  const ttlS = ttlSelect(false);
  const ro = el('input', { type: 'checkbox' });
  const panel = el('div', { class: 'panel' });
  panel.appendChild(el('h3', null, t('Mint a key')));
  panel.appendChild(field(t('label'), labelI));
  panel.appendChild(field(t('note'), noteI, hint(t('shown in the list and searchable; it never affects what the key can do'))));
  panel.appendChild(field(t('plane'), scopeS, hint(isSuper() ? '' : t('a delegated account mints client keys only'))));
  panel.appendChild(field(t('view'), viewS));
  panel.appendChild(field(t('valid for'), ttlS, hint(t('a key past its deadline is refused at the next handshake'))));
  panel.appendChild(el('label', null, ro, ' ' + t('read-only')));
  panel.appendChild(action(t('Create key'), async () => {
    const args = { label: labelI.value.trim(), scope: scopeS.value, ro: ro.checked };
    if (viewS.value) args.view = viewS.value;
    if (noteI.value.trim()) args.note = noteI.value.trim();
    if (ttlS.value) args.ttl = ttlS.value;
    const r = await api('apikey.create', args);
    labelI.value = '';
    noteI.value = '';
    // The reply carries the plaintext `<id>:<secret>` once; `view` is what THIS
    // request asked for, because the create reply reports the key, not the row.
    modal(t('New api key'), wrap(
      el('p', null, t('This is the only time the secret is shown — GroveFS stores a digest.')),
      el('div', { class: 'secret-card' },
        el('code', { class: 'secret' }, r.key),
        el('button', { type: 'button', class: 'link', onclick: () => copyText(r.key) }, t('copy'))),
      el('dl', { class: 'kv' },
        el('dt', null, t('id')), el('dd', { class: 'mono' }, r.id),
        el('dt', null, t('label')), el('dd', null, r.label),
        el('dt', null, t('owner')), el('dd', null, r.owner || '—'),
        el('dt', null, t('minted by')), el('dd', null, r.created_by || '—'),
        el('dt', null, t('scope')), el('dd', { class: 'mono' }, r.scope),
        el('dt', null, t('view')), el('dd', null, args.view ? mono(args.view) : t('not bound to a view')),
        el('dt', null, t('valid until')), el('dd', null, r.expires_at || t('never expires')),
        el('dt', null, t('note')), el('dd', null, r.note || '—')),
      el('p', { class: 'muted' },
        t('Save it now. From here on the list shows its fingerprint ${h}; Rotate is the only way to obtain a usable secret again.', { h: maskOf(r.key) }))),
    [{ t: t('I have saved it'), onclick: (btn, err, close) => close() }]);
    await reload();
  }, { class: 'primary' }));
  if (!isSuper()) panel.appendChild(hint(t('an unbound key reaches every tree, so a delegated account must name one')));
  root.appendChild(panel);

  // Requirement 4c: a page of verbs the user has to guess at is a page that
  // gets used wrong. Every control below is spelled out here, including the
  // one thing the SECURITY model refuses (why the middle of the key is gone).
  root.appendChild(helpPanel(t('What each action does — and what GroveFS keeps'), [
    [t('Create key'), t('mints a 16-character id and a 64-hex-digit secret; the client uses `<id>:<secret>`. The secret exists once, in the dialog you are about to close.')],
    [t('the key column'), t('shows the stored fingerprint: the first and last four hex digits of the secret. GroveFS keeps a SHA-256 digest, NOT the secret and NOT anything reversible, so no screen, API or database dump can show the middle — that is the price of a leaked database being harmless.')],
    [t('Rotate'), t('replaces the secret for the SAME id: label, scope, view binding, note, deadline and audit history stay. The old secret stops working at its next handshake; a live mount on it stays open until it reconnects.')],
    [t('Revoke'), t('retires the credential for good. Nothing can un-revoke it and a revoked row is not editable — create a new key instead. It takes effect on the key’s NEXT call, so a live mount keeps its open trees until then.')],
    [t('Make ro / Make rw'), t('flips the write permission without touching the secret, so a mount can be made read-only without handing anyone a new credential.')],
    [t('Edit'), t('changes the bookkeeping only: the note and the deadline. Scope, view binding and id never change here, because a key an application is already holding would otherwise acquire a reach its owner did not ask for.')],
    [t('Expires'), t('the same spelling as `grovefs apikey create --ttl`: a count and `s`, `m`, `h` or `d`, or `never`. Past its deadline a key is refused at the next handshake, and a live mount is cut within about a second.')],
    [t('plane'), t('`mount` authenticates a client mount, `mcp` an agent, `admin` the governance console, `any` everything. An `admin` key can change accounts and keys — treat it as a password.')],
    [t('view binding'), t('a bound key can only open that view (and what derives from it); an unbound one reaches every tree. Only an administrator may mint an unbound key.')],
    [t('minted by'), t('the account that created the key. It is shown beside the owner, who is the account the key belongs to — the two differ whenever an administrator mints for a colleague.')],
  ]));

  const rows = await api('apikey.list', {});
  const f = ST.keyFilter;
  const cols = keyCols();
  const keyHost = el('div');
  const qI = el('input', {
    type: 'search', placeholder: t('filter by label, id, note, owner or view…'),
    value: f.q, oninput: (ev) => { f.q = ev.target.value; renderKeysTable(); },
  });
  const stateChips = el('div');
  const scopeSel = el('select', { onchange: () => { f.scope = scopeSel.value; renderKeysTable(); } },
    el('option', { value: 'all' }, t('every plane')), ...['mount', 'mcp', 'admin', 'any'].map((v) => el('option', { value: v }, v)));
  scopeSel.value = f.scope;
  const viewSel = el('select', { onchange: () => { f.view = viewSel.value; renderKeysTable(); } },
    el('option', { value: 'all' }, t('every view')), el('option', { value: '' }, t('unbound keys')),
    ...ST.views.map((v) => el('option', { value: v.name }, v.name)));
  viewSel.value = f.view;
  const fbar = filterBar('', qI,
    fld(t('state'), stateChips),
    fld(t('plane'), scopeSel),
    fld(t('view'), viewSel));
  root.appendChild(fbar);
  root.appendChild(keyHost);
  renderKeysTable();

  function renderKeysTable() {
    clear(stateChips);
    const bucket = (b) => (b === 'all' ? rows.length : rows.filter((r) => keyBucket(r) === b).length);
    stateChips.appendChild(chipGroup(KEY_BUCKETS.map((b) => ({
      v: b,
      // The chips carry their own counts, so "how many are expired" is
      // answerable before anything is clicked.
      t: b === 'all' ? t('all') : t(b === 'expired' ? 'expired' : b === 'revoked' ? 'revoked' : b === 'expiring' ? 'expiring soon' : 'live'),
      n: bucket(b),
    })), f.state, (v) => { f.state = v; renderKeysTable(); }));
    const hit = rows.filter((r) => {
      const q = f.q.trim().toLowerCase();
      if (q && ![r.label, r.id, r.owner, r.created_by, r.view, r.note, r.secret_hint]
        .some((x) => x && String(x).toLowerCase().includes(q))) return false;
      if (f.state !== 'all' && keyBucket(r) !== f.state) return false;
      if (f.scope !== 'all' && r.scope !== f.scope) return false;
      if (f.view !== 'all' && (r.view || '') !== f.view) return false;
      return true;
    });
    fbar.setCount(counted(hit.length, rows.length));
    clear(keyHost);
    keyHost.appendChild(tableNode(cols, hit,
      rows.length ? t('no keys match the filter') : t('no keys you can see'),
      (r) => ({ class: r.id === ST.keyCur ? 'cur' : '', onclick: () => openKey(r) })));
  }

  function openKey(r) {
    ST.keyCur = r.id;
    renderKeysTable();
    keyDrawer(r, () => { ST.keyCur = null; renderKeysTable(); });
  }
}

/**
 * One credential's whole record in the side pane. The table shows the six
 * facts you compare rows on; everything else about a key — its full timestamps,
 * who minted it, whether it has been used at all, what it was last used FOR —
 * lives here, with the verbs that act on it in the footer, where the record
 * they are about to change is on screen while you read it.
 */
async function keyDrawer(r, onClose) {
  const recent = await soft('apikey.calls', { limit: 300 });
  const mine = recent.ok ? (recent.r || []).filter((c) => c.key_id === r.id).slice(0, 8) : [];
  return drawer({
    title: r.label,
    titleClass: '',
    badges: [keyState(r), el('span', { class: 'badge' + (r.ro ? ' warn' : '') }, r.ro ? t('read-only') : t('read-write'))],
    onclose: onClose,
    left: t('a fingerprint identifies a key; it does not authorize anything'),
    sections: [
      {
        h: t('Credential'),
        kv: [
          [t('id'), el('span', { class: 'cell-badges' }, mono(r.id),
            el('button', { type: 'button', class: 'mini', onclick: () => copyText(r.id) }, t('copy')))],
          [t('key'), maskCell(r.secret_hint)],
          [t('plane'), mono(r.scope)],
          [t('view'), r.view ? mono(r.view) : el('span', { class: 'muted' }, t('unbound — every tree'))],
        ],
        node: el('p', { class: 'muted' },
          t('A client presents `<id>:<secret>` (`mount --api-key`, or the `GROVEFS_API_KEY` environment variable). GroveFS stores only a digest, so this screen cannot read the secret back — that is what makes a leaked database harmless.')),
      },
      {
        h: t('Bookkeeping'),
        kv: [
          [t('note'), r.note || el('span', { class: 'muted' }, t('no note'))],
          [t('owner'), r.owner || el('span', { class: 'muted' }, t('unassigned'))],
          [t('minted by'), r.created_by || el('span', { class: 'muted' }, t('unknown — this row predates the bookkeeping columns'))],
          [t('created'), shortStampCell(r.created_at)],
          [t('last used'), r.last_used_at ? shortStampCell(r.last_used_at) : el('span', { class: 'muted' }, t('never used'))],
          [t('valid until'), r.expires_at
            ? el('span', null, mono(r.expires_at), ' ',
              el('span', { class: 'muted' }, r.expired ? t('expired') : t('in ${left}', { left: humanLeft(r.expires_at) })))
            : t('never expires')],
          [t('revoked'), r.revoked_at ? shortStampCell(r.revoked_at) : el('span', { class: 'muted' }, t('not revoked'))],
        ],
      },
      {
        h: t('Last calls with it'),
        node: recent.ok
          ? (mine.length ? auditTable(mine, t('no calls recorded for it yet'))
            : el('p', { class: 'muted' }, t('no calls recorded for it yet')))
          : el('p', { class: 'err' }, recent.error),
      },
    ],
    buttons: [
      r.revoked_at ? null : keyEditBtn(r),
      r.revoked_at ? null : roBtn(r),
      r.revoked_at ? null : rotateBtn(r),
      r.revoked_at ? null : revokeBtn(r),
    ].filter(Boolean),
  });
}

function keyEditBtn(r) {
  return el('button', { type: 'button', class: 'mini', onclick: () => keyEditDialog(r) }, t('Edit'));
}

/**
 * The ONLY two fields a key's row lets you edit are its remark and its
 * deadline, and that is deliberate: scope, view binding and id are what a
 * running application is holding, and moving them under it would either break
 * the mount or hand it a reach nobody asked for. So this dialog says what it
 * changes ("what the registry says about the key") and refuses nothing else.
 */
function keyEditDialog(r) {
  const noteI = el('textarea', { class: 'plain', rows: 3, maxlength: NOTE_MAX });
  noteI.value = r.note || '';
  const ttlS = ttlSelect(true);
  const m = modal(t('Edit ${label}', { label: r.label }), wrap(
    el('p', { class: 'muted' },
      t('A note and a deadline are bookkeeping: they change what the registry says about this key, never what it can open.')),
    field(t('note'), noteI, hint(t('${n} characters at most; empty clears it', { n: 500 }))),
    field(t('valid for'), ttlS, hint(r.expires_at
      ? t('now ${at} — ${left}', { at: shortTime(r.expires_at), left: r.expired ? t('already past') : t('in ${left}', { left: humanLeft(r.expires_at) }) })
      : t('no deadline now')))),
  [{
    t: t('Save'),
    onclick: async (btn, err) => {
      err.textContent = '';
      const args = { id: r.id, note: noteI.value };
      if (ttlS.value !== '__keep') args.ttl = ttlS.value || 'never';
      try {
        await api('apikey.update', args);
        m.close();
        toast(t('key updated'));
        await reload();
      } catch (e) {
        // "no ACTIVE row" (a revoked key) and a bad ttl both land here, in the
        // dialog the user is looking at, in the server's own words.
        err.textContent = e.message;
        btn.disabled = false;
      }
    },
  }]);
}

function roBtn(r) {
  return action(r.ro ? t('Make rw') : t('Make ro'), async () => {
    await api('apikey.set_ro', { id: r.id, ro: !r.ro });
    toast(r.ro ? t('now read-write') : t('now read-only'));
    await reload();
  }, { class: 'mini' });
}

function revokeBtn(r) {
  return action(t('Revoke'), async () => {
    const note = r.view ? t('bound to ${v}', { v: r.view }) : t('NOT bound to a view — it reaches every tree');
    if (!(await ask(t('Revoke this key?'),
      t('${l} (${i}) · scope ${s} · ${note}\n\nLive mounts keep working until their next operation.', { l: r.label, i: r.id, s: r.scope, note }), t('Revoke')))) return;
    await api('apikey.revoke', { id: r.id });
    toast(t('revoked'));
    await reload();
  }, { class: 'mini danger' });
}

// Rotate = "the secret is lost but the credential stays": same id, label,
// scope, view binding and audit chain; only the digest is replaced, and the
// new plaintext gets its one-time display — the same secret-card the create
// flow shows (old keys carry a longer secret; both formats stay valid).
function rotateBtn(r) {
  return action(t('Rotate'), async () => {
    if (!(await ask(t('Rotate this key?'),
      t('${l} (${i}) · scope ${s}\n\nThe id, label, scope, view binding and audit history are kept; only the secret is replaced. The old secret stops working at its NEXT handshake — a live mount on it stays open until it reconnects; to kill a credential outright, Revoke it.', { l: r.label, i: r.id, s: r.scope }), t('Rotate')))) return;
    const res = await api('apikey.rotate', { id: r.id });
    modal(t('Key rotated'), wrap(
      el('p', null, t('This is the only time the secret is shown — GroveFS stores a digest.')),
      el('div', { class: 'secret-card' },
        el('code', { class: 'secret' }, res.key),
        el('button', { type: 'button', class: 'link', onclick: () => copyText(res.key) }, t('copy'))),
      el('p', { class: 'muted' }, t('label ${l} · scope ${s} · owner ${o} · ${b}', {
        l: res.label, s: res.scope, o: res.owner || '—',
        b: r.view ? t('bound to ${v}', { v: r.view }) : t('not bound to a view'),
      }))),
      [{ t: t('I have saved it'), onclick: (btn, err, close) => close() }]);
    toast(t('key rotated'));
    await reload();
  }, { class: 'mini' });
}

/* -------------------------------------------------------------- accounts --- */

/** The columns an account's OWN key list uses inside a drawer: four, because a
    34rem pane cannot show ten and the other six are one click away on the keys
    page. */
const miniKeyCols = () => [
  { t: t('label'), render: (r) => cell2(r.label, mono(r.id)) },
  { t: t('plane'), k: 'scope', class: 'mono' },
  { t: t('view'), render: (r) => (r.view ? mono(r.view) : el('span', { class: 'muted' }, t('any view'))) },
  { t: t('state'), render: (r) => keyState(r) },
];

/** Base views as pills that JUMP: an account's reach is a list of names, and
    the next question is always what one of those names looks like. */
function baseCell(bases) {
  if (!bases || !bases.length) return el('span', { class: 'muted' }, t('none'));
  const shown = bases.slice(0, 3);
  return el('div', { class: 'cell-badges' },
    ...shown.map((b) => el('button', {
      type: 'button', class: 'count-pill', title: t('open this base view'),
      onclick: () => { ST.sel = b; go('views'); },
    }, b)),
    bases.length > shown.length
      ? el('span', { class: 'count-pill', title: bases.slice(3).join(', ') }, '+' + (bases.length - shown.length))
      : null);
}

/** What a principal can still do, in words: role first, then whether it is
    even allowed to sign in. Two badges that mean different things (a
    DISABLED admin is not the same as an enabled user) never merge into one
    colour. */
function accountState(a) {
  return el('div', { class: 'cell-badges' },
    a.disabled_at
      ? el('span', { class: 'badge bad', title: t('disabled ${at}', { at: a.disabled_at }) }, t('disabled'))
      : el('span', { class: 'badge good' }, t('enabled')));
}

async function pageAccounts(root) {
  await loadViews();
  const bases = ST.views.filter((v) => v.kind === 'base').map((v) => v.name);
  root.appendChild(pageHead(t('Accounts'), t('Who governs what — accounts, roles and base-view delegations.'),
    when(!isSuper(), el('span', { class: 'badge off' }, t('administrators act here'))),
    action(t('refresh'), reload, { class: 'link' })));

  const accI = el('input', { placeholder: 'bob', size: 14 });
  const pwI = el('input', { type: 'password', autocomplete: 'new-password', size: 20 });
  const roleS = el('select', null,
    el('option', { value: 'user' }, t('user — governs what it is delegated')),
    el('option', { value: 'admin' }, t('admin — governs the deployment')));
  const panel = el('div', { class: 'panel' });
  panel.appendChild(el('h3', null, t('Create an account')));
  panel.appendChild(field(t('account'), accI, hint(t('a-z 0-9 . _ -, 2–32 chars'))));
  panel.appendChild(field(t('password'), pwI, hint(t('at least 10 characters'))));
  panel.appendChild(field(t('role'), roleS, hint(t('a user is useless until it is delegated a base view below'))));
  panel.appendChild(action(t('Create'), async (btn) => {
    try {
      const r = await api('account.create', { account: accI.value.trim(), password: pwI.value, role: roleS.value });
      pwI.value = '';
      toast(t('${account} created as ${role} — ${note}', { account: r.account, role: r.role, note: r.note }));
      await reload();
    } catch (e) { btn.disabled = false; throw e; }
  }, { class: 'primary' }));
  // Presentation only, and the one place this page hides rather than refuses:
  // a `user` cannot create accounts at any point, so the form would be a
  // guaranteed error toast on every page it is shown on.
  if (isSuper()) root.appendChild(panel);

  const accts = await soft('account.list', {});
  if (!accts.ok) {
    // `account.list` is an administrator's op, so a `user` lands here. It still
    // holds `account.passwd` (the SelfAccount gate), and its own password is the
    // one thing this page must never make it hunt for.
    root.appendChild(bar(el('h2', null, t('Your account'))));
    const mine = el('div', { class: 'detail' },
      el('div', { class: 'head' }, whoCell(ST.who.account, t('signed in here')),
        el('span', { class: 'badge' + (ST.who.role === 'admin' ? ' admin' : '') }, ST.who.role)),
      el('p', { class: 'muted' }, t('The account list is an administrator’s view: ${e}', { e: accts.error })));
    const line = (lbl, ...val) => el('div', { class: 'stat-line' },
      el('span', { class: 'lbl' }, lbl), el('span', { class: 'val' }, ...val));
    mine.appendChild(el('div', { class: 'grid2' },
      el('div', null,
        el('h3', { class: 'det-h' }, t('Your own record')),
        line(t('account'), mono(ST.who.account)),
        line(t('role'), el('b', null, ST.who.role)),
        line(t('password'), action(t('Change my password'), async () => { await passwdDialog({ account: ST.who.account, role: ST.who.role }); }))),
      el('div', null,
        el('h3', { class: 'det-h' }, t('What you govern')),
        line(t('base views'), el('b', null, String((ST.who.bases || []).length)),
          (ST.who.bases || []).length ? el('span', { class: 'muted' }, t('· ${n} derived name(s)', {
            n: ST.views.filter((v) => (ST.who.bases || []).includes(v.base) && v.kind !== 'base').length })) : null),
        el('p', { class: 'muted' }, (ST.who.bases || []).length
          ? baseCell(ST.who.bases)
          : t('You govern no base view yet — ask an administrator for a delegation.')))));
    root.appendChild(mine);
    return;
  }
  const rows = accts.r || [];
  // Grants and keys are read up front, not on demand: the table's "3 keys" and
  // "2 base views" cells ARE the two facts that decide whether an account can
  // be deleted safely, and the drawer shows the rows behind those numbers.
  const [grantsR, keysR] = await Promise.all([soft('grant.list', {}), soft('apikey.list', {})]);
  const grants = grantsR.ok ? (grantsR.r || []) : [];
  const keyRows = keysR.ok ? (keysR.r || []) : null;
  const grantsOf = (name) => grants.filter((g) => g.account === name);
  const keysOf = (name) => (keyRows === null ? null : keyRows.filter((k) => k.owner === name || k.created_by === name));

  const f = ST.accFilter;
  const cols = [
    {
      t: t('account'), class: 'nowrap',
      render: (r) => whoCell(r.account, r.created_at ? t('joined ${when}', { when: shortTime(r.created_at) }) : null, !!r.disabled_at),
    },
    {
      t: t('role'),
      render: (r) => el('span', {
        class: 'badge' + (r.role === 'admin' ? ' admin' : ''),
        title: r.role === 'admin' ? t('an administrator governs every view and every account')
          : t('a user governs only the base views it is delegated'),
      }, r.role),
    },
    {
      t: t('delegated bases'),
      render: (r) => baseCell((grantsOf(r.account).map((g) => g.base_view)).length ? grantsOf(r.account).map((g) => g.base_view) : (r.bases || [])),
    },
    {
      t: t('keys'), class: 'nowrap',
      render: (r) => {
        const ks = keysOf(r.account);
        if (ks === null) return el('span', { class: 'muted' }, '—');
        const live = ks.filter((k) => keyBucket(k) === 'live' || keyBucket(k) === 'expiring').length;
        return el('button', {
          type: 'button', class: 'count-pill' + (ks.length ? ' on' : ''),
          title: t('show this account’s keys on the keys page'),
          onclick: () => { ST.keyFilter.q = r.account; ST.keyFilter.state = 'all'; go('keys'); },
        }, t('${all} key(s) · ${live} live', { all: ks.length, live }));
      },
    },
    {
      t: t('last sign-in'), class: 'nowrap',
      render: (r) => (r.last_login_at ? shortStampCell(r.last_login_at) : el('span', { class: 'muted' }, t('never'))),
    },
    { t: t('state'), render: accountState },
    {
      t: '', class: 'actions', render: (r) => el('div', { class: 'row' },
        el('button', { type: 'button', class: 'mini', onclick: () => passwdDialog(r) }, r.account === ST.who.account ? t('My password') : t('Password')),
        when(isSuper(), el('button', { type: 'button', class: 'mini', onclick: () => roleDialog(r) }, r.role === 'admin' ? t('Make user') : t('Make admin'))),
        when(isSuper() && r.account !== ST.who.account, el('button', {
          type: 'button', class: 'mini' + (r.disabled_at ? '' : ' danger'),
          onclick: async () => {
            const want = !r.disabled_at;
            if (!(await ask(want ? t('Disable sign-in?') : t('Enable sign-in?'),
              want ? t('${account} could no longer sign in; its open sessions die on their next call.', { account: r.account })
                   : t('${account} may sign in again.', { account: r.account }),
              want ? t('Disable') : t('Enable')))) return;
            await api('account.disable', { account: r.account, disabled: want });
            toast(want ? t('${account} disabled', { account: r.account }) : t('${account} enabled', { account: r.account }));
            await reload();
          },
        }, r.disabled_at ? t('Enable') : t('Disable'))),
        when(isSuper() && r.account !== ST.who.account, el('button', {
          type: 'button', class: 'mini danger',
          onclick: async () => {
            if (!(await ask(t('Delete this account?'),
              t('${account} — its delegations go with it.\nIts api keys DO NOT; the server reports how many are still live.', { account: r.account }), t('Delete')))) return;
            const res = await api('account.delete', { account: r.account });
            toast(t('${account}: ${note}', { account: res.account, note: res.note }));
            await reload();
          },
        }, t('Delete')))),
    },
  ];

  const qI = el('input', {
    type: 'search', placeholder: t('filter by account, role or delegated view…'),
    value: f.q, oninput: (ev) => { f.q = ev.target.value; renderAccTable(); },
  });
  const roleChips = el('div');
  const stateChips = el('div');
  const accHost = el('div');
  const fbar = filterBar('', qI, fld(t('role'), roleChips), fld(t('state'), stateChips));
  root.appendChild(fbar);
  root.appendChild(accHost);
  renderAccTable();

  function renderAccTable() {
    const bucket = (kind, v) => (v === 'all' ? rows.length
      : rows.filter((a) => (kind === 'role' ? a.role : a.disabled_at ? 'disabled' : 'enabled') === v).length);
    clear(roleChips);
    roleChips.appendChild(chipGroup([['all', t('all roles')], ['admin', 'admin'], ['user', 'user']]
      .map(([v, label]) => ({ v, t: label, n: bucket('role', v) })), f.role,
    (v) => { f.role = v; renderAccTable(); }));
    clear(stateChips);
    stateChips.appendChild(chipGroup([['all', t('any state')], ['enabled', t('enabled')], ['disabled', t('disabled')]]
      .map(([v, label]) => ({ v, t: label, n: bucket('state', v) })), f.state,
    (v) => { f.state = v; renderAccTable(); }));
    const hit = rows.filter((a) => {
      if (f.role !== 'all' && a.role !== f.role) return false;
      if (f.state !== 'all' && (a.disabled_at ? 'disabled' : 'enabled') !== f.state) return false;
      const q = f.q.trim().toLowerCase();
      if (q) {
        const hay = [a.account, a.role, ...(a.bases || []), ...grantsOf(a.account).map((g) => g.base_view)]
          .filter(Boolean);
        if (!hay.some((x) => String(x).toLowerCase().includes(q))) return false;
      }
      return true;
    });
    fbar.setCount(counted(hit.length, rows.length));
    clear(accHost);
    accHost.appendChild(tableNode(cols, hit, rows.length ? t('no accounts match the filter') : t('no accounts'),
      (a) => ({ class: a.account === ST.accCur ? 'cur' : '', onclick: () => openAccount(a) })));
  }

  function openAccount(a) {
    ST.accCur = a.account;
    renderAccTable();
    accountDrawer(a, {
      grants: grantsOf(a.account),
      keys: keysOf(a.account),
      views: ST.views,
      isSelf: a.account === ST.who.account,
    }, () => { ST.accCur = null; renderAccTable(); });
  }

  const gAcc = el('select', null, rows.map((a) => el('option', { value: a.account }, a.account)));
  const gBase = el('select', null, (bases.length ? bases : ST.views.map((v) => v.name)).map((n) => el('option', { value: n }, n)));
  const gp = el('div', { class: 'panel' });
  gp.appendChild(el('h3', null, t('Delegate a base view')));
  gp.appendChild(field(t('account'), gAcc));
  gp.appendChild(field(t('base view'), gBase));
  gp.appendChild(action(t('Grant'), async (btn) => {
    try {
      const r = await api('grant.add', { account: gAcc.value, base: gBase.value });
      toast(r.asNamed ? t('${account} governs ${base}', { account: r.account, base: r.base })
        : t('${account} governs ${base} — that is what the name you chose resolves to', { account: r.account, base: r.base }));
      await reload();
    } catch (e) { btn.disabled = false; throw e; }
  }, { class: 'primary' }));
  root.appendChild(bar(el('h2', null, t('Delegations')),
    hint(t('an account governs a base view AND every lens, fork and alias derived from it'))));
  if (isSuper()) root.appendChild(gp);

  const gCols = [
    { t: t('account'), render: (g) => whoCell(g.account, null, false), class: 'nowrap' },
    { t: t('base view'), render: (g) => el('button', { type: 'button', class: 'mini mono', onclick: () => { ST.sel = g.base_view; go('views'); } }, g.base_view) },
    { t: t('perm'), k: 'perm', class: 'mono' },
    { t: t('granted'), render: (g) => cell2(shortStampCell(g.granted_at), g.granted_by ? t('by ${by}', { by: g.granted_by }) : null), class: 'muted nowrap' },
    {
      t: '', class: 'actions', render: (g) => action(t('Withdraw'), async () => {
        if (!(await ask(t('Withdraw this delegation?'),
          t('${account} stops governing ${base} and everything derived from it, on its next call.', { account: g.account, base: g.base_view }), t('Withdraw')))) return;
        await api('grant.remove', { account: g.account, base: g.base_view });
        toast(t('withdrawn'));
        await reload();
      }, { class: 'mini danger' }),
    },
  ];
  const gHost = el('div');
  root.appendChild(gHost);
  const gq = el('input', {
    type: 'search', placeholder: t('filter by account or base view…'),
    oninput: () => renderGrantTable(gq.value),
  });
  root.insertBefore(el('div', { class: 'filterbar' }, el('div', { class: 'filterbox' }, gq)), gHost);
  renderGrantTable('');

  function renderGrantTable(qRaw) {
    clear(gHost);
    const q = (qRaw || '').trim().toLowerCase();
    const hit = !q ? grants : grants.filter((g) => [g.account, g.base_view, g.perm, g.granted_by]
      .some((x) => x && String(x).toLowerCase().includes(q)));
    gHost.appendChild(tableNode(gCols, hit, grants.length
      ? t('no delegations match the filter') : t('no base view is delegated to anyone yet')));
  }
}

/** The account's whole record: what it is, what it governs (with the derived
    names its delegations actually reach), and which credentials still carry
    its name — the last of these is the one an operator has to check BEFORE
    deleting, because deleting an account deliberately does not revoke them. */
function accountDrawer(a, ctx, onClose) {
  const reach = ctx.views.filter((v) => ctx.grants.some((g) => g.base_view === v.base) && v.kind !== 'base');
  return drawer({
    title: a.account,
    titleClass: 'mono',
    badges: [el('span', { class: 'badge' + (a.role === 'admin' ? ' admin' : '') }, a.role), accountState(a).firstChild],
    onclose: onClose,
    left: a.account === ST.who.account ? t('this is you') : null,
    sections: [
      {
        h: t('Account'),
        kv: [
          [t('account'), el('span', { class: 'cell-badges' }, mono(a.account),
            el('button', { type: 'button', class: 'mini', onclick: () => copyText(a.account) }, t('copy')))],
          [t('role'), a.role === 'admin' ? t('an administrator governs every view and every account')
            : t('a user governs only the base views it is delegated')],
          [t('created'), shortStampCell(a.created_at)],
          [t('last sign-in'), a.last_login_at ? shortStampCell(a.last_login_at) : el('span', { class: 'muted' }, t('never'))],
          [t('disabled'), a.disabled_at ? shortStampCell(a.disabled_at) : el('span', { class: 'muted' }, t('not disabled'))],
        ],
      },
      {
        h: t('What it governs'),
        kv: [[t('delegated base views'), String(ctx.grants.length)],
          [t('names that reach through them'), String(reach.length)]],
        node: ctx.grants.length
          ? tableNode([
            { t: t('base view'), k: 'base_view', class: 'mono' },
            { t: t('perm'), k: 'perm', class: 'mono' },
            { t: t('granted'), render: (g) => cell2(shortStampCell(g.granted_at), g.granted_by ? t('by ${by}', { by: g.granted_by }) : null), class: 'muted' },
          ], ctx.grants, t('no base view is delegated to this account'))
          : el('p', { class: 'muted' }, t('It governs no base view: a user with no delegation can sign in and change its own password, nothing else.')),
      },
      {
        h: t('Credentials in its name'),
        node: ctx.keys === null
          ? el('p', { class: 'muted' }, t('the key registry is an administrator’s view'))
          : (ctx.keys.length
            ? tableNode(miniKeyCols(), ctx.keys, t('no keys you can see'))
            : el('p', { class: 'muted' }, t('no keys bound to this view yet'))),
      },
    ],
    buttons: [
      el('button', { type: 'button', onclick: () => passwdDialog(a) }, a.isSelf ? t('My password') : t('Password')),
    ],
  });
}

function passwdDialog(r) {
  const pw1 = el('input', { type: 'password', autocomplete: 'new-password', size: 26 });
  const pw2 = el('input', { type: 'password', autocomplete: 'new-password', size: 26 });
  const m = modal(t('Password for ${account}', { account: r.account }), wrap(
    field(t('new password'), pw1, hint(t('at least 10 characters'))),
    field(t('repeat it'), pw2)),
  [{
    t: t('Change password'),
    onclick: async (btn, err) => {
      err.textContent = '';
      if (pw1.value !== pw2.value) { err.textContent = t('the two entries differ'); return; }
      try {
        await api('account.passwd', { account: r.account, password: pw1.value });
        m.close();
        toast(t('password changed'));
      } catch (e) { err.textContent = e.message; btn.disabled = false; }
    },
  }]);
}

function roleDialog(r) {
  const want = r.role === 'admin' ? 'user' : 'admin';
  const m = modal(t('Make ${account} ${role}', { account: r.account, role: want }), wrap(
    el('p', null, want === 'admin'
      ? t('An administrator governs every view, every account, and where a new base view is placed.')
      : t('A user governs only the base views it is delegated, plus their forks, aliases, lenses and keys.'))),
  [{
    t: t('Make ${role}', { role: want }),
    onclick: async (btn, err) => {
      err.textContent = '';
      try {
        await api('account.role', { account: r.account, role: want });
        m.close();
        toast(t('${account} is now ${role}', { account: r.account, role: want }));
        if (r.account === ST.who.account) await boot(); else await reload();
      } catch (e) {
        // "…is the only enabled administrator…" lands here, and it is the
        // refusal that keeps a deployment reachable.
        err.textContent = e.message;
        btn.disabled = false;
      }
    },
  }]);
}

/* ----------------------------------------------------------------- audit --- */

/** Time windows are the FIRST filter for a log, and they are relative: an
    operator asks "what happened in the last hour", not "what happened between
    two timestamps I have to type". The absolute range a real audit export
    needs belongs to the CLI (`grovefs apikey calls`), which has the same rows
    and a machine to diff them in. */
const WIN_MS = { '1h': 36e5, '24h': 864e5, '7d': 7 * 864e5 };
const WINDOWS = [['all', 'all time'], ['1h', 'last hour'], ['24h', 'last 24 hours'], ['7d', 'last 7 days']];
function withinWindow(ts, w) {
  if (!w || w === 'all') return true;
  const t0 = Date.parse(ts);
  return isFinite(t0) && Date.now() - t0 <= WIN_MS[w];
}
const distinct = (rows, k) => [...new Set(rows.map((r) => r[k]).filter(Boolean))].sort();

async function pageAudit(root) {
  root.appendChild(pageHead(t('Audit'), t('Every call the control plane answered — failures included.'),
    action(t('refresh'), reload, { class: 'link' })));
  const limitS = el('select', { onchange: () => { ST.auditLimit = Number(limitS.value); go('audit'); } },
    el('option', { value: '50' }, '50'), el('option', { value: '200' }, '200'), el('option', { value: '1000' }, '1000'));
  limitS.value = String(ST.auditLimit || 50);
  const rows = (await api('apikey.calls', { limit: Number(limitS.value) })) || [];
  // Where the trail ENDS is a property of the log, not of the activity in it:
  // "the oldest row here is three weeks old" reads very differently when the
  // server sweeps at 90d versus when nothing is ever deleted. `store.info`
  // answers that, and it is a SUPER gate — so a delegated operator's refusal
  // (soft, not api: this note must never break the page) simply shows no line.
  const life = await soft('store.info', {});
  const keep = life.ok && life.r ? life.r.auditRetention : undefined;
  root.appendChild(bar(el('h2', null, t('Governance calls')),
    fld(t('rows to load'), limitS),
    hint(keep === undefined
      ? t('the newest ${n} of them, whatever the filter shows', { n: rows.length })
      : keep
        ? t('the newest ${n} loaded; the server sweeps rows older than ${w}', { n: rows.length, w: keep })
        : t('the server keeps every row until an operator purges them (`grovefs apikey purge`)'))));

  const f = ST.auditFilter;
  const qI = el('input', {
    type: 'search', placeholder: t('filter by account, key, op or message…'),
    value: f.q, oninput: (ev) => { f.q = ev.target.value; renderAuditTable(); },
  });
  const winChips = el('div');
  const verdictChips = el('div');
  // The two DROPDOWNS are built from the rows actually loaded, not from a list
  // of verbs this file remembers: a new op appears in the filter the moment it
  // appears in the log, and a renamed one cannot be offered as an empty choice.
  const planeSel = el('select', { onchange: () => { f.plane = planeSel.value; renderAuditTable(); } },
    el('option', { value: 'all' }, t('every plane')), ...distinct(rows, 'method').map((m) => el('option', { value: m }, m)));
  planeSel.value = f.plane;
  const ops = distinct(rows, 'tool');
  const opSel = el('select', { onchange: () => { f.op = opSel.value; renderAuditTable(); } },
    el('option', { value: 'all' }, t('every op')), ...ops.map((o) => el('option', { value: o }, o)));
  opSel.value = ops.includes(f.op) ? f.op : 'all';
  if (opSel.value !== f.op) f.op = 'all';
  const fbar = filterBar('', qI,
    fld(t('time'), winChips),
    fld(t('result'), verdictChips),
    fld(t('plane'), planeSel),
    fld(t('op'), opSel));
  const auditHost = el('div');
  root.appendChild(fbar);
  root.appendChild(auditHost);
  root.appendChild(el('p', { class: 'muted' }, isSuper()
    ? t('Every call the control plane answered, by any key or account. Failures are here too — a wrong credential included. Click a row for the whole message the server wrote.')
    : t('Only your own activity and your views’ keys; the full log is an administrator’s view.')));
  renderAuditTable();

  function renderAuditTable() {
    const pass = (r) => {
      if (!withinWindow(r.ts, f.window)) return false;
      if (f.plane !== 'all' && r.method !== f.plane) return false;
      if (f.op !== 'all' && r.tool !== f.op) return false;
      if (f.verdict === 'ok' && !r.ok) return false;
      if (f.verdict === 'failed' && r.ok) return false;
      const q = f.q.trim().toLowerCase();
      if (q) {
        const hay = [auditActor(r), r.key_label, r.key_id, r.method, r.tool, r.detail];
        if (!hay.some((x) => x && String(x).toLowerCase().includes(q))) return false;
      }
      return true;
    };
    clear(winChips);
    winChips.appendChild(chipGroup(WINDOWS.map(([v, label]) => ({
      v, t: t(label), n: rows.filter((r) => withinWindow(r.ts, v)).length,
    })), f.window, (v) => { f.window = v; renderAuditTable(); }));
    clear(verdictChips);
    verdictChips.appendChild(chipGroup([['all', t('every result')], ['failed', t('failures only')], ['ok', t('only ok')]]
      .map(([v, label]) => ({ v, t: label, n: v === 'all' ? rows.length : rows.filter((r) => !!r.ok === (v === 'ok')).length })),
    f.verdict, (v) => { f.verdict = v; renderAuditTable(); }));
    const hit = rows.filter(pass);
    fbar.setCount(counted(hit.length, rows.length));
    clear(auditHost);
    auditHost.appendChild(auditTable(hit, rows.length
      ? (hit.length ? '' : t('no calls match the filter'))
      : t('no governance calls yet')));
  }
}

/* ---------------------------------------------------------------- start --- */

applyStatic();
boot();

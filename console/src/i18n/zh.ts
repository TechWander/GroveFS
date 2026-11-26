/*
  The Chinese table, carried over verbatim from the hand-written console.

  The contract is unchanged and is the reason this file exists at all: a
  sentence's KEY is its English text, only this one table needs maintenance, a
  missing entry degrades to the English instead of throwing, and interpolated
  sentences key on their template (`${var}` is substituted after lookup, never
  concatenated into a key). Server-refusal sentences stay verbatim: the exact
  wording is the answer to the user's question.

  Product vocabulary — base/fork/alias/lens, mount/mcp/admin/any — is
  deliberately NOT translated: those names are the API's own terms.
*/
export const ZH: Readonly<Record<string, string>> = {
  /* chrome: sign-in, nav, header */
  'Governance console': '治理控制台',
  'Sign in': '登录',
  'Sign out': '退出登录',
  Account: '账号',
  Password: '口令',
  password: '口令',
  copy: '复制',
  'copied to clipboard': '已复制到剪贴板',
  Overview: '总览',
  Views: '视图',
  Keys: '密钥',
  Accounts: '账号',
  Audit: '审计',
  // Page overlines: the small category printed above each page title.
  Deployment: '部署',
  Namespace: '命名空间',
  Credentials: '凭据',
  People: '成员',
  Ledger: '账目',
  // The header's <nav> is labelled for a screen reader; the hand-written page
  // had no landmark at all here.
  'Sections of the console': '控制台分区',
  'Skip to content': '跳到正文',

  /* shared */
  Cancel: '取消',
  Confirm: '确认',
  'nothing here': '这里还没有内容',
  refresh: '刷新',
  'loading…': '加载中…',
  'the session ended — sign in again': '会话已结束 — 请重新登录',
  'signed out': '已退出',
  'sign-in failed (HTTP ${s})': '登录失败（HTTP ${s}）',
  // Both are this rewrite's: the old file let a refused connection reject and
  // left a blank page, which is the one failure a management console must not
  // have.
  'the console could not read its own session: ${e}': '控制台无法读取自身会话：${e}',
  'the console could not reach the gateway: ${e}': '控制台无法连到网关：${e}',
  'the gateway answered HTTP ${s} without a JSON body': '网关返回 HTTP ${s}，响应体不是 JSON',

  /* overview */
  backend: '后端',
  'signed in as': '登录身份',
  'base views': '基视图',
  definitions: '定义',
  accounts: '账号',
  'live keys': '有效密钥',
  'mount sessions': '挂载会话',
  'open trees': '常驻树',
  'This deployment': '本部署',
  'server role': '服务器角色',
  'base views you govern': '你治理的基视图',
  'Your reach': '你的权限范围',
  'Deployment-wide counts belong to an administrator. You govern these base views and everything derived from them:':
    '全局计数属于管理员。你治理以下基视图及其全部派生：',
  '${n} derived': '${n} 个派生',
  unreachable: '不可达',
  // The empty case of the delegated account's reach list; the old page simply
  // rendered an empty box, which reads as a loading failure rather than as
  // "nobody has delegated you anything".
  'no base views delegated to you': '没有委托给你治理的基视图',
  'Recent governance calls': '最近的治理调用',

  /* audit table */
  when: '时间',
  key: '密钥',
  plane: '面',
  op: '操作',
  result: '结果',
  detail: '详情',
  ok: '成功',
  failed: '失败',
  'no governance calls yet': '暂无治理调用',

  /* views page */
  'no views you can see': '没有你可看见的视图',
  'pick a view on the left': '请在左侧选择一个视图',
  '${n} mount session(s)': '${n} 个挂载会话',
  base: '基',
  stamp: '戳记',
  tree: '树',
  'on this server': '在此服务器上',
  keys: '密钥',
  'physical tree': '物理树',
  'fork tree': 'fork 树',
  'none of its own': '无自有树',
  open: '已打开',
  'not open': '未打开',
  '${n} bound to this name': '${n} 把绑定到此名',
  'Export dump': '导出转储',
  'Fork…': 'Fork…',
  'Lens / alias…': '透镜 / 别名…',
  'Drop fork': '删除 fork',
  'Remove definition': '移除定义',
  'Drop this fork?': '删除这个 fork？',
  '${name}: its own tree is deleted. Its base ${b} and every shared object stay.':
    '${name}：其自有树将被删除。它的基 ${b} 与全部共享对象保留。',
  'Remove this definition?': '移除这个定义？',
  '${name} =\n${def}\n\nOnly the name goes away — no tree and no object is deleted.':
    '${name} =\n${def}\n\n仅移除名字 — 不删除任何树与对象。',
  'definition removed': '定义已移除',

  /* fork dialog */
  'Fork ${name}': 'Fork ${name}',
  'A fork copies the METADATA tree into a new writable view; content bytes stay shared until something in the fork rewrites them.':
    'fork 会把元数据树复制成一个新的可写视图；内容字节保持共享，直到 fork 内有改写发生。',
  'new view name': '新视图名',
  'copy the current tree (unchecked: an empty fork to graft into)':
    '复制当前树（不勾选：留一个空 fork 用于接枝）',
  'Create fork': '创建 fork',
  'forked ${a} → ${b} (${n} nodes, ${m} blobs)': '已 fork ${a} → ${b}（${n} 个节点、${m} 个 blob）',

  /* define dialog */
  'derived lens (read-only shape over a base)': '派生透镜（在基之上的只读形状）',
  'alias (another name for it)': '别名（它的另一个名字）',
  'Define a view over ${name}': '为 ${name} 定义视图',
  'The definition text is exactly what `grovefs view define --file` takes.':
    '定义文本与 `grovefs view define --file` 的输入完全一致。',
  kind: '种类',
  definition: '定义',
  Save: '保存',
  'definition saved': '定义已保存',

  /* new base view */
  'creating a BASE view is an administrator’s act — it chooses where bytes live.':
    '创建基视图是管理员操作——它决定字节落在哪个实例。',
  'New base view': '新建基视图',
  name: '名称',
  'PG instance': 'PG 实例',
  'auto — least loaded': '自动 — 选最少负载的实例',
  'auto — currently ${n} (${v} view(s))': '自动 — 当前落 ${n}（${v} 个视图）',
  'the new view’s tree is created on this instance; later writes always follow it':
    '新视图的树建在所选实例上；后续写入始终跟随该实例',
  Create: '创建',
  'a base view owns physical deletion; lenses, aliases and forks reshape one':
    '基视图拥有物理删除权；透镜、别名与 fork 是对基的重塑',

  /* import */
  'metadata dump': '元数据转储',
  'as view': '作为视图',
  'replace an existing tree': '替换已存在的树',
  'import as a BASE view': '作为基视图导入',
  Import: '导入',
  'choose a dump file first': '请先选择转储文件',
  'import failed (HTTP ${s})': '导入失败（HTTP ${s}）',
  'imported ${n} nodes, ${m} blobs as ${kind}': '已导入 ${n} 个节点、${m} 个 blob，作为 ${kind}',
  'Import a dump': '导入转储',
  'a dump becomes a BASE view, or a fork of a view you govern — content bytes are never in a dump':
    '转储会成为基视图，或你治理视图的一个 fork — 转储中不含内容字节',
  'imported as a FORK of ${name}; only an administrator can import a new base view':
    '将作为 ${name} 的 fork 导入；只有管理员能导入新基视图',

  /* keys */
  'Keys bound to ${name}': '绑定到 ${name} 的密钥',
  label: '标签',
  id: 'ID',
  scope: '范围',
  mode: '模式',
  owner: '属主',
  'last used': '最近使用',
  state: '状态',
  'read-only': '只读',
  'read-write': '读写',
  revoked: '已吊销',
  live: '有效',
  'no keys bound to this view yet': '此视图尚无绑定密钥',
  'Edit this definition': '编辑此定义',
  'saved — live mounts pick it up within their attr TTL (≤2s)':
    '已保存 — 活跃挂载将在其 attr TTL（≤2s）内感知',
  'API keys': 'API 密钥',
  'Mint a key': '铸造密钥',
  'mount — a client mount': 'mount — 客户端挂载',
  'mcp — an agent': 'mcp — 智能体',
  'admin — the governance plane': 'admin — 治理面',
  'any — every plane': 'any — 所有面',
  '(no view binding — every tree)': '（不绑定视图 — 可达所有树）',
  '(choose a view)': '（选择一个视图）',
  view: '视图',
  'key rotated': '密钥已轮换',
  'Create key': '创建密钥',
  'a delegated account mints client keys only': '委派账号只能铸造客户端密钥',
  'The registry kept a recoverable copy of this secret, wrapped under its store key — Reveal reads it back.':
    '登记表已用其存储密钥封装了这份 secret 的可恢复副本 — “读出” 可随时取回。',
  'This is the copy the registry keeps, opened under its store key. The call is written to the audit trail with this id.':
    '这就是登记表保存的副本，已用其存储密钥解开。本次调用会连同该 id 一并写入审计记录。',
  'This store keeps no recoverable copy of it, so this is the only time the secret is shown — Rotate is the way to get a new one.':
    '本存储不保存它的可恢复副本，因此这是 secret 唯一一次显示 — 要获得新的请用 “轮换”。',
  'Stored secret': '存储的 secret 副本',
  'no recoverable copy of this secret': '此 secret 没有可恢复副本',
  'label ${l} · scope ${s} · owner ${o} · ${b}': '标签 ${l} · 范围 ${s} · 属主 ${o} · ${b}',
  'bound to ${v}': '绑定到 ${v}',
  'not bound to a view': '未绑定视图',
  'I have saved it': '我已保存',
  'an unbound key reaches every tree, so a delegated account must name one':
    '未绑定密钥可达所有树，因此委派账号必须指定一个视图',
  'any view': '任意视图',
  created: '创建时间',
  'no keys you can see': '没有你可看见的密钥',
  'Make rw': '改为读写',
  'Make ro': '改为只读',
  'now read-write': '已改为读写',
  'now read-only': '已改为只读',
  Revoke: '吊销',
  Rotate: '轮换',
  Reveal: '读出',
  'Rotate this key?': '轮换这把密钥？',
  '${l} (${i}) · scope ${s}\n\nThe id, label, scope, view binding and audit history are kept; only the secret is replaced. The old secret stops working at its NEXT handshake — a live mount on it stays open until it reconnects; to kill a credential outright, Revoke it.':
    '${l}（${i}）· 范围 ${s}\n\n保留 id、标签、范围、视图绑定与审计历史，只替换 secret。旧 secret 在下一次握手时失效 — 基于它的活跃挂载在重连前仍保持打开；要彻底封杀请直接吊销。',
  'Key rotated': '密钥已轮换',
  'Revoke this key?': '吊销这把密钥？',
  '${l} (${i}) · scope ${s} · ${note}\n\nLive mounts keep working until their next operation.':
    '${l}（${i}）· 范围 ${s} · ${note}\n\n活跃挂载在其下一次操作前继续可用。',
  'Delete this key?': '删除这把密钥？',
  '${l} (${i}) · scope ${s}\n\nThis removes the credential from the store. A live mount stops at its next operation. The audit rows it wrote are kept. This cannot be undone — use Revoke instead if you only want to disable it.':
    '${l}（${i}）· 范围 ${s}\n\n这会把凭据从存储中移除；活跃挂载会在下一次操作时停止。它写下的审计记录会保留。此操作不可撤销 — 若只想禁用，请改用「吊销」。',
  'key deleted': '密钥已删除',
  'NOT bound to a view — it reaches every tree': '未绑定视图 — 可达所有树',

  /* accounts */
  'administrators act here': '管理员操作区',
  'Create an account': '创建账号',
  'a-z 0-9 . _ -, 2–32 chars': 'a-z 0-9 . _ -，2–32 个字符',
  'at least 10 characters': '至少 10 个字符',
  role: '角色',
  'user — governs what it is delegated': 'user — 按委派治理',
  'admin — governs the deployment': 'admin — 治理全局',
  '${account} created as ${role} — ${note}': '${account} 已创建为 ${role} — ${note}',
  'Your account': '你的账号',
  'The account list is an administrator’s view: ${e}': '账号列表是管理员视图：${e}',
  'Change my password': '修改我的口令',
  'delegated bases': '委派的基视图',
  none: '无',
  'last sign-in': '最近登录',
  disabled: '已禁用',
  enabled: '已启用',
  'My password': '我的口令',
  'Make user': '降为 user',
  'Make admin': '升为 admin',
  Enable: '启用',
  Disable: '禁用',
  'Disable sign-in?': '禁用登录？',
  'Enable sign-in?': '允许登录？',
  '${account} could no longer sign in; its open sessions die on their next call.':
    '${account} 将无法再登录；其打开的会话在下一次调用时失效。',
  '${account} may sign in again.': '${account} 可重新登录。',
  '${account} disabled': '${account} 已禁用',
  '${account} enabled': '${account} 已启用',
  Delete: '删除',
  'Delete this account?': '删除这个账号？',
  '${account} — its delegations go with it.\nIts api keys DO NOT; the server reports how many are still live.':
    '${account} — 其委派随之删除。\n其 API 密钥不会删除；服务器会报告仍有几把有效。',
  '${account}: ${note}': '${account}：${note}',
  'no accounts': '没有账号',
  'Delegate a base view': '委派基视图',
  'base view': '基视图',
  Grant: '授予',
  '${account} governs ${base}': '${account} 治理 ${base}',
  '${account} governs ${base} — that is what the name you chose resolves to':
    '${account} 治理 ${base} — 你选的名字实际解析到这里',
  Delegations: '委派',
  'an account governs a base view AND every lens, fork and alias derived from it':
    '账号治理一个基视图及其全部透镜、fork 与别名派生',
  perm: '权限',
  granted: '授予时间',
  Withdraw: '撤销',
  'Withdraw this delegation?': '撤销这条委派？',
  '${account} stops governing ${base} and everything derived from it, on its next call.':
    '${account} 将在下一次调用时停止治理 ${base} 及其全部派生。',
  withdrawn: '已撤销',

  /* password / role dialogs */
  'Password for ${account}': '${account} 的口令',
  'new password': '新口令',
  'repeat it': '再输入一遍',
  'Change password': '修改口令',
  'the two entries differ': '两次输入不一致',
  'password changed': '口令已修改',
  'Make ${account} ${role}': '将 ${account} 改为 ${role}',
  'An administrator governs every view, every account, and where a new base view is placed.':
    '管理员治理所有视图、所有账号，并决定新基视图的落位。',
  'A user governs only the base views it is delegated, plus their forks, aliases, lenses and keys.':
    'user 只治理被委派的基视图，及其 fork、别名、透镜与密钥。',
  'Make ${role}': '改为 ${role}',
  '${account} is now ${role}': '${account} 现在是 ${role}',

  /* audit page */
  'Only your own activity and your views’ keys; the full log is an administrator’s view.':
    '仅你的操作与你视图的密钥；完整日志是管理员视图。',
  'the fork tree is gone, but its definition could not be removed: ${e}':
    'fork 树已删除，但其定义无法移除：${e}',
  'view created': '视图已创建',
  'New api key': '新的 API 密钥',
  account: '账号',
  /* page heads */
  'Deployment snapshot, your reach and the latest governance activity.':
    '部署概要、你的权限范围与最近的治理动态。',
  'Base views, forks, aliases and lenses — select one to inspect.':
    '基视图、fork、别名与透镜 — 选择一个进行查看。',
  'Mint, rotate, revoke and delete the keys clients mount and agents call with.':
    '铸造、轮换、吊销与删除客户端挂载及智能体调用所用的密钥。',
  'Who governs what — accounts, roles and base-view delegations.':
    '谁治理什么 — 账号、角色与基视图委派。',
  'Every call the control plane answered — failures included.':
    '控制面应答的每一次调用 — 含失败记录。',
  /* filtering */
  'filter by name…': '按名称过滤…',
  'failures only': '仅看失败',
  all: '全部',
  forks: 'fork',
  aliases: '别名',
  lenses: '透镜',
  'no views match the filter': '没有符合过滤条件的视图',
  'no keys match the filter': '没有符合过滤条件的密钥',
  'no calls match the filter': '没有符合过滤条件的调用',
  'View list': '视图列表',

  /* 抽屉与过滤条：三个列表页共用的组件 */
  Close: '关闭',
  'What the server said': '服务器的原话',
  'nothing recorded': '无记录',
  'no stored fingerprint': '无存储指纹',
  'first and last 4 hex digits of the secret; a list never shows the middle, and only Reveal can — audited, from the store’s wrapped copy':
    'secret 的首尾各 4 位十六进制；列表从不显示中段，只有 “读出” 能 — 从存储的封装副本取回，并计入审计',
  '${n} in total': '共 ${n} 条',
  '${n} of ${m} shown': '显示 ${n} / ${m} 条',
  'rows to load': '载入行数',
  time: '时间',
  who: '发起者',
  'all roles': '所有角色',
  'any state': '所有状态',
  'every plane': '所有面',
  'every view': '所有视图',
  'every op': '所有操作',
  'every key': '所有密钥',
  'key trail': '密钥谱系',
  minted: '铸出',
  '${n} minted by this key, in the rows loaded here': '本次载入的行里，这把密钥铸出 ${n} 把',
  'nothing in these rows was minted by this key': '这些行里没有这把密钥铸出的密钥',
  'show this key’s calls': '查看这把密钥的调用',
  'no minted key recorded on this row': '此行未记录铸出的密钥',
  'A row’s key is the credential that signed the call. From this console that is always the gateway’s own admin key, because the gateway holds one and forwards what you click: filtering by key finds the door, searching by account finds who walked through it.':
    '行上的密钥是签署这次调用的凭据。从本控制台来的永远是网关自己那把 admin 密钥——它持有一把，并转发你的点击：按密钥筛出的是入口，按账号搜出的才是操作者。',
  'every result': '所有结果',
  'all time': '全部时间',
  'last hour': '最近 1 小时',
  'last 24 hours': '最近 24 小时',
  'last 7 days': '最近 7 天',
  'last 30 days': '最近 30 天',
  'last 180 days': '最近 180 天',
  'last 360 days': '最近 360 天',
  'custom range': '自定义区间',
  'time range': '时间范围',
  from: '起',
  to: '止',
  'only ok': '仅看成功',
  unknown: '未知',
  unassigned: '未指定',
  'by ${by}': '由 ${by}',

  /* 总览页：「本部署」区块 */
  'storage backend': '存储后端',
  'one store for metadata and content': '元数据与内容同库',
  'metadata root': '元数据根',
  'local anchor (--meta-dir)': '本机锚点（--meta-dir）',
  'control plane': '控制面',
  'placement count unreadable': '放置计数不可读',
  '${v} view(s) placed': '已放置 ${v} 个视图',
  '${n} view(s) have no placement row and are hash-routed':
    '${n} 个视图没有放置行，按哈希路由',
  '${n} view(s) point at an instance the registry no longer defines — their mount is refused until the registry or placement is fixed':
    '${n} 个视图指向注册表已不再定义的实例 — 在修正注册表或放置前其挂载会被拒绝',
  'the process this console is talking to': '本控制台正在对话的进程',
  'audit trail': '审计留痕',
  'rows older than ${w} are swept': '早于 ${w} 的行会被清理',
  'every row is kept until an operator purges it': '全部保留，直到操作员手动清除',
  namespace: '命名空间',
  '${n} base view(s)': '${n} 个基视图',
  'the trees mounts open': '挂载打开的树',
  '${n} names resident here': '${n} 个名字在此常驻',
  credentials: '凭据',
  '${n} live': '${n} 把有效',
  '· ${n} expired · a revocation or expiry lands on the NEXT call':
    '· ${n} 把已过期 · 吊销与过期都在下一次调用落地',
  'governance accounts': '治理账号',
  'principles on the governance plane': '治理面上的原则',
  '${all} key(s) · ${live} live': '${all} 把密钥 · ${live} 把有效',
  'activity on this server': '此服务器上的活动',
  'governance call': '治理调用',
  'Governance calls': '治理调用',
  'read live from the meta-server that answered this page': '由应答本页的 meta-server 实时读取',
  'Every count is a snapshot of the instant this page loaded; anything created elsewhere appears after a refresh.':
    '每个计数都是本页加载那一刻的快照；其他地方的创建需刷新后才会出现。',
  'open the audit page': '打开审计页',
  'open this view': '打开该视图',
  'open this account': '打开该账号',
  'open in keys page': '在密钥页中打开',
  'this view’s calls': '该视图的调用',
  '· the first owner was created by `meta-server --admin-user`':
    '· 首任管理员由 `meta-server --admin-user` 创建',
  '${n} expired and refused': '${n} 把已过期并被拒绝',
  'none expired': '无过期',

  /* 视图页：谱系面板 */
  'This name': '这个名字',
  Lineage: '谱系',
  Definition: '定义',
  fork: 'fork',
  alias: '别名',
  lens: '透镜',
  other: '其他',
  'names that resolve through this base': '经由该基解析的名字',
  'names derived directly from it': '直接派生自它的名字',
  'nothing is derived from it yet': '尚无东西派生自它',
  'derived names': '派生名字',
  '${n} total': '共 ${n} 个',
  'via ${v}': '经由 ${v}',
  '${n} name(s) resolve through this base — listed side by side; “via” names the edge':
    '${n} 个名字经由该基解析 — 同级并列展示；“经由”标注所在定义边',
  '${n} name(s) hang below it — listed side by side': '其下有 ${n} 个名字 — 同级并列展示',
  '${n} name(s) hang below it': '其下有 ${n} 个名字',
  '${n} name(s) resolve through this base — ${d} directly below it':
    '${n} 个名字经由该基解析 — 其中 ${d} 个直接位于其下',
  'deeper than ${d} levels — expand it from its own page': '深于 ${d} 层 — 请从其自身页面展开',
  'resolves through': '经由…解析',
  'open this base view': '打开该基视图',
  'forks, aliases and lenses': 'fork、别名与透镜',

  /* 密钥页：台账列与有效期 */
  note: '备注',
  'valid for': '有效期',
  'valid until': '有效至',
  Expires: '有效期',
  'never expires': '永不过期',
  'keep unchanged': '保持不变',
  'no deadline': '无期限',
  'no deadline now': '当前无期限',
  'no note': '无备注',
  expired: '已过期',
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
  Edit: '编辑',
  'Edit ${label}': '编辑 ${label}',
  Credential: '凭据',
  Bookkeeping: '登记信息',
  'Last calls with it': '它最近的调用',
  'no calls recorded for it yet': '尚未记录到它的调用',
  'the newest ${n} of them, whatever the filter shows': '其中最新的 ${n} 条，与过滤显示无关',
  'the newest ${n} loaded; the server sweeps rows older than ${w}':
    '已加载最新 ${n} 条；服务端会清掉早于 ${w} 的记录',
  'the server keeps every row until an operator purges them (`grovefs apikey purge`)':
    '服务端会一直保留全部记录，直到有人手工清理（`grovefs apikey purge`）',
  'a fingerprint identifies a key; it does not authorize anything':
    '指纹用于认出密钥；它不授予任何权限',
  'What each action does — and what GroveFS keeps': '每个操作做什么 — 以及 GroveFS 保存什么',
  'Make ro / Make rw': '改为只读 / 改为读写',
  'the key column': '密钥列',
  'what this key is for…': '这把密钥的用途…',
  '${n} characters at most; empty clears it': '最多 ${n} 个字符；留空即清除',
  'shown in the list and searchable; it never affects what the key can do':
    '在列表中显示并可搜索；它绝不影响密钥能做什么',
  'a key past its deadline is refused at the next handshake': '过了期限的密钥在下一次握手时被拒绝',
  'a mount with it cannot write': '用它挂载则不可写',
  'it can write': '它可写',
  'Save it now. From here on the list shows its fingerprint ${h}; Rotate is the only way to obtain a usable secret again.':
    '现在就保存。此后列表只显示它的指纹 ${h}；轮换是重新获得可用 secret 的唯一途径。',
  'The list shows its fingerprint ${h}; Reveal reads this secret back for as long as the store holds its wrapping material.':
    '列表只显示它的指纹 ${h}；只要存储还持有封装材料，“读出” 就能取回这份 secret。',
  'A client presents `<id>:<secret>` (`mount --api-key`, or the `GROVEFS_API_KEY` environment variable).':
    '客户端提交 `<id>:<secret>`（`mount --api-key`，或 `GROVEFS_API_KEY` 环境变量）。',
  'A handshake reads only the stored digest; the secret itself lives wrapped under the store key, so Reveal can hand it back — and that read is audited with the id it opened.':
    '握手只校验存储的摘要；secret 本身以存储密钥封装保存，因此 “读出” 可以把它交回 — 而这次读取会连同它解开的 id 一起计入审计。',
  'No recoverable copy of this secret is stored, so nothing here can read it back: Rotate is the way to obtain a usable one.':
    '此 secret 未存储可恢复副本，因此这里无法读回它：请用 “轮换” 获得可用的新 secret。',
  'A note and a deadline are bookkeeping: they change what the registry says about this key, never what it can open.':
    '备注与期限属于登记信息：它们只改变登记表对此密钥的记述，绝不改变它能打开的内容。',
  '`mount` authenticates a client mount, `mcp` an agent, `admin` the governance console, `any` everything. An `admin` key can change accounts and keys — treat it as a password.':
    '`mount` 认证客户端挂载，`mcp` 认证智能体，`admin` 认证治理控制台，`any` 覆盖全部。`admin` 密钥可改动账号与密钥 — 请把它当作口令对待。',
  'a bound key can only open that view (and what derives from it); an unbound one reaches every tree. Only an administrator may mint an unbound key.':
    '绑定的密钥只能打开该视图（及其派生）；未绑定的可达所有树。只有管理员能铸未绑定密钥。',
  'mints a 16-character id and a 32-hex-digit secret; the client uses `<id>:<secret>`. The dialog that follows is the first display of it — whether the registry also keeps a copy an operator can read back is said there, and depends on the store.':
    '铸出一个 16 字符的 id 与一个 32 位十六进制的 secret；客户端使用 `<id>:<secret>`。随后的对话框是它第一次显示 — 登记表是否也留有运维可读回的副本，那里会说明，取决于存储。',
  'shows the stored fingerprint: the first and last four hex digits of the secret. The digest GroveFS authenticates against is one-way and gives no hint back, so no list or dump assembles the middle from this — the full string comes back only through Reveal.':
    '显示存储的指纹：secret 的首尾各四位十六进制。GroveFS 用于认证的摘要不可逆、也反推不出任何提示，因此任何列表或转储都无法由此拼出中段 — 完整字符串只能经 “读出” 取回。',
  'reads the stored copy of a secret back and shows it, unwrapped under the store’s own key, to the key’s owner or a super administrator. The call is written to the audit trail naming the id it opened. Only offered where a copy exists: a row minted before the store had wrapping material has none, and Rotate is how a usable secret is obtained then.':
    '把 secret 的存储副本读回并显示，用存储自身的密钥解开，仅对该密钥的属主或超级管理员可见。本次调用会连同它解开的 id 写入审计记录。只在存在副本时才提供：存储尚无封装材料时铸出的行没有副本，那时要用 “轮换” 获得可用 secret。',
  'replaces the secret for the SAME id: label, scope, view binding, note, deadline and audit history stay. The old secret stops working at its next handshake; a live mount on it stays open until it reconnects.':
    '为同一个 id 替换 secret：标签、范围、视图绑定、备注、期限与审计历史都保留。旧 secret 在下一次握手时失效；基于它的活跃挂载在重连前保持打开。',
  'retires the credential for good. Nothing can un-revoke it and a revoked row is not editable — delete it if you also want it gone from the registry. It takes effect on the key’s NEXT call, so a live mount keeps its open trees until then.':
    '彻底作废该凭据。吊销不可撤销，已吊销的行也不可编辑 — 若还想从登记簿中移除，请使用删除。它在密钥的下一次调用生效，因此在那之前活跃挂载保持其已打开的树。',
  'removes the credential row entirely; the id and secret stop authenticating at the next handshake. Only the key’s owner or a super administrator may delete it, and the console asks first. Audit rows the key wrote stay in the log with their original attribution — deleting a key never shortens the trail.':
    '彻底移除凭据行；id 与 secret 在下一次握手时不再通过认证。只有密钥所有者或超级管理员可以删除，且控制台会先确认。该密钥写下的审计记录仍以原归属保留在日志中 — 删除密钥绝不会缩短审计链。',
  'flips the write permission without touching the secret, so a mount can be made read-only without handing anyone a new credential.':
    '翻转写权限而不动 secret，因此可把一个挂载改为只读，而无需交给任何人新凭据。',
  'changes the bookkeeping only: the note and the deadline. Scope, view binding and id never change here, because a key an application is already holding would otherwise acquire a reach its owner did not ask for.':
    '只改登记信息：备注与期限。范围、视图绑定与 id 在此永不改变，否则应用已持有的密钥就会获得其属主并未要求过的可达范围。',
  'the same spelling as `grovefs apikey create --ttl`: a count and `s`, `m`, `h` or `d`, or `never`. Past its deadline a key is refused at the next handshake, and a live mount is cut within about a second.':
    '与 `grovefs apikey create --ttl` 同一种写法：数字加 `s`、`m`、`h` 或 `d`，或 `never`。过了期限的密钥在下一次握手被拒绝，活跃挂载约一秒内被切断。',
  'the account that created the key. It is shown beside the owner, who is the account the key belongs to — the two differ whenever an administrator mints for a colleague.':
    '创建该密钥的账号。它与属主并列显示，属主是密钥所属的账号 — 管理员替同事铸造时两者不同。',
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
  'a user is useless until it is delegated a base view below':
    '在下方向它委派基视图之前，user 治理不了任何东西',
  'It governs no base view: a user with no delegation can sign in and change its own password, nothing else.':
    '它不治理任何基视图：未被委派的 user 只能登录并修改自己的口令，别无其他。',
  'You govern no base view yet — ask an administrator for a delegation.':
    '你尚未治理任何基视图 — 请向管理员申请委派。',
  'no base view is delegated to anyone yet': '尚未向任何人委派基视图',
  'no base view is delegated to this account': '未向该账号委派任何基视图',
  'no delegations match the filter': '没有符合过滤条件的委派',
  'no accounts match the filter': '没有符合过滤条件的账号',
  'filter by account or base view…': '按账号或基视图过滤…',
  'filter by account, role or delegated view…': '按账号、角色或委派视图过滤…',
  'the key registry is an administrator’s view': '密钥登记表只有管理员可见',
  // The old drawer repeated the views page's sentence here and said "this
  // view" about an account; this one names what the section is about.
  'no keys in its name': '其名下没有密钥',
  'show this account’s keys on the keys page': '在密钥页展示该账号的密钥',
  'joined ${when}': '${when} 加入',
  'disabled ${at}': '${at} 被禁用',
  'not disabled': '未禁用',
  never: '从未',
  'signed in here': '在此登录',
  'this is you': '这是你',

  /* 审计页：按时间/发起者/操作检索 */
  'Every call the control plane answered, by any key or account. Failures are here too — a wrong credential included. Click a row for the whole message the server wrote.':
    '控制面应答的每一次调用，来自任何密钥或账号。失败也在其中 — 含凭据错误的调用。点击某行查看服务器写出的完整信息。',
  'filter by account, key, op or message…': '按账号、密钥、操作或消息过滤…',
  'no account on it': '其上没有账号',
  'never used': '从未使用',
  'clients reading trees now': '正在读取树的客户端',
  'and everything derived from them': '以及它们的全部派生',
  '· ${n} definition(s)': '· ${n} 个定义',
  '· ${n} derived name(s)': '· ${n} 个派生名字',
  '· ${n} tree(s) open in memory': '· ${n} 棵树在内存中打开',

  /* 存储页：后端、实例与置放 */
  Storage: '存储',
  Metadata: '元数据',
  'Where the namespace physically lives — the backend, its instances and the placement of each view.':
    '命名空间实际存放的位置 — 后端、实例，以及每个视图的置放。',
  'Embedded SQLite': '嵌入式 SQLite',
  'base directory': '库目录',
  views: '视图',
  'resident engines': '常驻引擎',
  'resident views': '常驻视图',
  'One database file per view under <base>/meta/. High availability ships VACUUM INTO snapshots between nodes — there is no instance to place a view on.':
    '每个视图在 <base>/meta/ 下各有一个数据库文件。高可用通过节点间 VACUUM INTO 快照完成 — 没有可供置放视图的实例。',
  'Experimental tree engine': '实验性树引擎',
  address: '地址',
  'local SQLite': '本地 SQLite',
  'Trees live in the treedb server; definitions, keys and accounts stay in the local SQLite control plane. This backend is experimental and runs as a single instance.':
    '树存放在 treedb 服务中；定义、密钥与账号仍在本地 SQLite 控制平面。该后端为实验性，单实例运行。',
  '${n} view(s) across ${i} instance(s)': '${n} 个视图，分布在 ${i} 个实例',
  'keys, definitions, accounts, audit and placement live here':
    '密钥、定义、账号、审计与置放表都在此实例',
  connection: '连接串',
  'contains credentials': '含凭据',
  'placed views': '已置放视图',
  'on this instance': '此实例上',
  'no view is placed here yet': '尚无视图置放在此实例',
  'open the filtered list for the rest': '打开筛选列表查看其余视图',
  'Open these views': '查看这些视图',
  'New base here': '在此新建 base',
  '${n} view(s) have no placement row and route by name hash.':
    '${n} 个视图没有置放记录，按名称哈希路由。',
  Placement: '置放规则',
  'A new view lands on the instance with the fewest placed views unless the caller names one. A view’s tree lives on exactly one instance; forks and grafts cannot cross instances.':
    '新视图默认落在已置放视图最少的实例，除非调用方指定实例。一个视图的树只存在于一个实例；fork 与 graft 不能跨实例。',
  'Instance details are an administrator’s view.': '实例详情仅管理员可见。',
  Show: '显示',
  Hide: '隐藏',
  close: '关闭',
  Capacity: '容量',
  'live sessions': '在线会话',
  'audit retention': '审计保留',
  'all instances': '全部实例',
  'metadata instance hosting this view': '承载该视图的元数据实例',
  'Home instance': '宿主实例',
  instance: '实例',
  'hash routed': '哈希路由',
  yes: '是',
  no: '否',
  'The schema holding this view’s tree lives on this instance. The connection string is shown in full — it contains the database password.':
    '承载该视图树的 schema 位于此实例。连接串完整展示 — 其中包含数据库密码。',
  'Keys of this view…': '该视图的密钥…',
  'Accounts…': '相关账号…',
  'open this key': '打开该密钥',
  'All keys in this account’s name…': '该账号名下的全部密钥…',
  'delegated view': '委派视图',
  'clear this filter': '清除该筛选',
  describe: '描述',
  /* 存储页改版：统计条、负载计与新交互 */
  retry: '重试',
  instances: '实例',
  placed: '已置放',
  unplaced: '未置放',
  'view(s) here': '个视图在此',
  'Refresh failed — showing the last loaded snapshot.': '刷新失败 — 正显示上次加载的快照。',
  /* 存储页：实例增删改查与守卫流程 */
  serving: '主控服务',
  'the address this meta-server routes its trees to': '本元数据服务器将树路由到的地址',
  experimental: '实验性',
  'treedb is an EXPERIMENTAL backend': 'treedb 是实验性后端',
  'control plane: local SQLite': '控制平面：本地 SQLite',
  'the control plane URI moves by handover, not edit': '控制平面连接串需通过交接（handover）变更，不可直接编辑',
  'the serving address changes with a restart, not here': '服务地址随 --meta-store 重启生效，不在此处更改',
  'Edit URI': '编辑连接串',
  Relocate: '迁移视图',
  'move this instance’s views onto another': '将此实例上的视图迁移到其他实例',
  'nothing to relocate': '没有需要迁移的视图',
  'Set as control': '设为控制平面',
  'Edit ${name}': '编辑 ${name}',
  'connection string': '连接串',
  'views placed on this name now resolve here; the database must already hold their schemas':
    '置放在此名称下的视图将解析到新地址；目标数据库必须已包含它们的 schema',
  'treedb routes to one --meta-store address; this only updates the inventory record':
    'treedb 只路由到单一 --meta-store 地址；此处仅更新清单记录',
  'Register an instance': '注册实例',
  'the placement identity — later views are balanced onto this name':
    '置放标识 — 后续视图会均衡落到此名称',
  'the meta-server dials it once before saving; an unreachable URI is refused':
    '元数据服务器保存前会拨号一次；不可达的连接串会被拒绝',
  'treedb routes to one --meta-store address; this records an inventory entry':
    'treedb 只路由到单一 --meta-store 地址；此处登记一条清单记录',
  Register: '注册',
  cancel: '取消',
  'Delete instance ${name}': '删除实例 ${name}',
  'this instance carries the control plane — promote another first':
    '此实例承载控制平面 — 请先提升其他实例',
  'this is the address the meta-server routes to': '这正是元数据服务器路由到的地址',
  'Deleting removes the registry line only; content bytes live on the data servers and are never touched here.':
    '删除仅移除注册表中的一行；内容字节位于数据服务器，此处绝不触碰。',
  'views placed here': '置放在此的视图',
  nodes: '节点数',
  'distinct refs': '去重引用数',
  'schema not found on this instance': '此实例上未找到该 schema',
  'the engine could not be reached, so its views could not be verified':
    '引擎不可达，故无法核对其视图',
  'no view is placed here': '尚无视图置放在此',
  'grovefs_refs consistency': 'grovefs_refs 一致性',
  'zero-holder rows (GC candidates)': '零持有者行（GC 候选）',
  holders: '持有者',
  zero: '零引用',
  'dangling routes': '悬空路由',
  'unverified instances (control plane was unreachable):': '未验证实例（控制平面不可达）：',
  'the guard refuses this removal': '守卫拒绝了此次删除',
  'Relocate ${name}': '迁移 ${name}',
  Done: '完成',
  'every view has moved off this instance — it can now be deleted.':
    '所有视图都已迁出此实例 — 现在可以删除它了。',
  'Move each placed view onto another instance. A view mounted on this host must be unmounted first.':
    '将每个已置放视图迁移到其他实例。挂在本机上的视图必须先卸载。',
  ' (control)': '（控制面）',
  Move: '迁移',
  'Make ${name} the control plane': '将 ${name} 设为控制平面',
  'Stage handover': '登记交接',
  'This stages a handover of grovefs_control — keys, definitions, accounts, audit and the placement ledger — to ${name}.':
    '这将登记一次 grovefs_control 的交接 — 密钥、定义、账号、审计与置放表 — 迁往 ${name}。',
  'Nothing moves yet: the copy runs the next time the meta-server starts, before any request is served. Restart it to finish, and the control badge moves then.':
    '目前尚未搬迁：副本会在元数据服务器下次启动、处理任何请求之前执行。请重启以完成，届时控制面标记随之迁移。',
  'The current plane must be reachable and hold no foreign-key constraints, or the server refuses and explains why.':
    '当前控制平面必须可达且不含外键约束，否则服务器会拒绝并说明原因。',
  'A control-plane handover to ${name} is staged — restart the meta-server to apply it.':
    '已登记向 ${name} 的控制平面交接 — 请重启元数据服务器以生效。',
  'treedb is EXPERIMENTAL: the control plane (definitions, keys, accounts) stays in local SQLite and this registry is inventory, not a router.':
    'treedb 为实验性：控制平面（定义、密钥、账号）仍在本地 SQLite，此注册表是清单而非路由器。',
  'No instances are registered yet.': '尚未注册任何实例。',
  'Register instance': '注册实例',
  'This registry is owned by a --meta-store file the operator edits by hand — the page is read-only here.':
    '此注册表由操作员手编的 --meta-store 文件所有 — 本页在此为只读。',
  'control plane: ${where}': '控制平面：${where}',
  /* 审计保留面板：窗口设置与立即清理 */
  'Audit retention': '审计保留',
  'retention window': '保留窗口',
  'rows older than this are swept; “never” keeps them until purged by hand':
    '早于此窗口的行会被清理；“never” 则保留，直到手动清除',
  'use a duration like 45m, 24h, 90d, or `never`': '请用 45m、24h、90d 这类时长，或 `never`',
  'window set to ${w}; ${n} row(s) older than that were trimmed now':
    '窗口已设为 ${w}；早于该窗口的 ${n} 行已立即清理',
  'sweep disarmed — rows are kept until an operator purges them':
    '已停用自动清理 — 记录将保留，直到操作员手动清除',
  '${n} row(s) are older than ${b} (nothing deleted yet)': '早于 ${b} 的记录有 ${n} 行（尚未删除）',
  'deleted ${n} row(s) older than ${b}': '已删除早于 ${b} 的 ${n} 行记录',
  'currently: rows older than ${w} are swept': '当前：早于 ${w} 的行会被清理',
  'currently: every row is kept until an operator purges it': '当前：全部保留，直到操作员手动清除',
  'preview cut': '预览清理量',
  'count rows older than the drafted window, delete nothing': '统计早于所填窗口的行数，不做删除',
  'purge now': '立即清理',
  'delete rows older than the drafted window': '删除早于所填窗口的记录',
  'save window': '保存窗口',
  'apply the retention policy (a duration also trims now)':
    '应用保留策略（设为时长会同时立即清理）',
}

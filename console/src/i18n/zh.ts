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
  // The header's <nav> is labelled for a screen reader; the hand-written page
  // had no landmark at all here.
  'Sections of the console': '控制台分区',

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
  'This is the only time the secret is shown — GroveFS stores a digest.':
    '秘钥仅此一次显示 — GroveFS 只保存摘要。',
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
  'Rotate this key?': '轮换这把密钥？',
  '${l} (${i}) · scope ${s}\n\nThe id, label, scope, view binding and audit history are kept; only the secret is replaced. The old secret stops working at its NEXT handshake — a live mount on it stays open until it reconnects; to kill a credential outright, Revoke it.':
    '${l}（${i}）· 范围 ${s}\n\n保留 id、标签、范围、视图绑定与审计历史，只替换 secret。旧 secret 在下一次握手时失效 — 基于它的活跃挂载在重连前仍保持打开；要彻底封杀请直接吊销。',
  'Key rotated': '密钥已轮换',
  'Revoke this key?': '吊销这把密钥？',
  '${l} (${i}) · scope ${s} · ${note}\n\nLive mounts keep working until their next operation.':
    '${l}（${i}）· 范围 ${s} · ${note}\n\n活跃挂载在其下一次操作前继续可用。',
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
  'Mint, rotate and revoke the keys clients mount and agents call with.':
    '铸造、轮换与吊销客户端挂载及智能体调用所用的密钥。',
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
  'first and last 4 hex digits of the secret; only its digest is stored, so the middle can never be shown again':
    'secret 的首尾各 4 位十六进制；库中只存它的摘要，因此中段永远无法再次显示',
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
  'every result': '所有结果',
  'all time': '全部时间',
  'last hour': '最近 1 小时',
  'last 24 hours': '最近 24 小时',
  'last 7 days': '最近 7 天',
  'only ok': '仅看成功',
  unknown: '未知',
  unassigned: '未指定',
  'by ${by}': '由 ${by}',

  /* 总览页：「本部署」区块 */
  'storage backend': '存储后端',
  'one store for metadata and content': '元数据与内容同库',
  'metadata root': '元数据根',
  'the process this console is talking to': '本控制台正在对话的进程',
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
  'A client presents `<id>:<secret>` (`mount --api-key`, or the `GROVEFS_API_KEY` environment variable). GroveFS stores only a digest, so this screen cannot read the secret back — that is what makes a leaked database harmless.':
    '客户端提交 `<id>:<secret>`（`mount --api-key`，或 `GROVEFS_API_KEY` 环境变量）。GroveFS 只保存摘要，因此本页面无法读回 secret — 这正是数据库泄露也无害的原因。',
  'A note and a deadline are bookkeeping: they change what the registry says about this key, never what it can open.':
    '备注与期限属于登记信息：它们只改变登记表对此密钥的记述，绝不改变它能打开的内容。',
  '`mount` authenticates a client mount, `mcp` an agent, `admin` the governance console, `any` everything. An `admin` key can change accounts and keys — treat it as a password.':
    '`mount` 认证客户端挂载，`mcp` 认证智能体，`admin` 认证治理控制台，`any` 覆盖全部。`admin` 密钥可改动账号与密钥 — 请把它当作口令对待。',
  'a bound key can only open that view (and what derives from it); an unbound one reaches every tree. Only an administrator may mint an unbound key.':
    '绑定的密钥只能打开该视图（及其派生）；未绑定的可达所有树。只有管理员能铸未绑定密钥。',
  'mints a 16-character id and a 64-hex-digit secret; the client uses `<id>:<secret>`. The secret exists once, in the dialog you are about to close.':
    '铸出一个 16 字符的 id 与一个 64 位十六进制的 secret；客户端使用 `<id>:<secret>`。secret 只存在一次，就在你即将关闭的这个对话框里。',
  'shows the stored fingerprint: the first and last four hex digits of the secret. GroveFS keeps a SHA-256 digest, NOT the secret and NOT anything reversible, so no screen, API or database dump can show the middle — that is the price of a leaked database being harmless.':
    '显示存储的指纹：secret 的首尾各四位十六进制。GroveFS 保存的是 SHA-256 摘要，不是 secret，也不可逆，因此任何界面、API 或数据库转储都拿不出中段 — 这正是数据库泄露无害所付的代价。',
  'replaces the secret for the SAME id: label, scope, view binding, note, deadline and audit history stay. The old secret stops working at its next handshake; a live mount on it stays open until it reconnects.':
    '为同一个 id 替换 secret：标签、范围、视图绑定、备注、期限与审计历史都保留。旧 secret 在下一次握手时失效；基于它的活跃挂载在重连前保持打开。',
  'retires the credential for good. Nothing can un-revoke it and a revoked row is not editable — create a new key instead. It takes effect on the key’s NEXT call, so a live mount keeps its open trees until then.':
    '彻底作废该凭据。吊销不可撤销，且已吊销的行不可编辑 — 请改为新建密钥。它在密钥的下一次调用生效，因此在那之前活跃挂载保持其已打开的树。',
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
}

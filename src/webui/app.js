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
  "Accounts live in the meta-server's control plane. The first owner is created at install time with <code>meta-server --admin-user</code>; more come from the <em>Accounts</em> page or <code>grovefs web account create</code>.":
    '账号存于 meta-server 的控制面。首任管理员在安装时由 <code>meta-server --admin-user</code> 创建；后续账号经「账号」页或 <code>grovefs web account create</code> 添加。',
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
  'engines this server is holding': '此服务器持有的引擎数',
  'This deployment': '本部署',
  'store': '存储',
  'meta root': '元数据根',
  'server role': '服务器角色',
  'first owner': '首任管理员',
  'created by `meta-server --admin-user` while the account store was empty; the rest come from the Accounts page': '由 `meta-server --admin-user` 在账号库为空时创建；其余经「账号」页添加',
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
  'derived': '派生',
  'keys': '密钥',
  'physical tree': '物理树',
  'fork tree': 'fork 树',
  'none of its own': '无自有树',
  'open': '已打开',
  'not open': '未打开',
  'nothing derived from it': '无派生视图',
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
  'Create key': '创建密钥',
  'a delegated account mints client keys only': '委派账号只能铸造客户端密钥',
  'This is the only time the secret is shown — GroveFS stores a digest.': '秘钥仅此一次显示 — GroveFS 只保存摘要。',
  'label ${l} · scope ${s} · owner ${o} · ${b}': '标签 ${l} · 范围 ${s} · 属主 ${o} · ${b}',
  'bound to ${v}': '绑定到 ${v}',
  'not bound to a view': '未绑定视图',
  'I have saved it': '我已保存',
  'an unbound key reaches every tree, so a delegated account must name one': '未绑定密钥可达所有树，因此委派账号必须指定一个视图',
  'any view': '任意视图',
  'ro': '只读',
  'rw': '读写',
  'created': '创建时间',
  'no keys you can see': '没有你可看见的密钥',
  'A key’s secret is shown once, at creation. Revoke is the only way back, and it takes effect on the key’s NEXT call — a live mount keeps its open trees until then.': '密钥的秘钥只在创建时显示一次。吊销是唯一回退手段，且在密钥的下一次调用才生效 — 在那之前，活跃挂载保持其已打开的树。',
  'Make rw': '改为读写',
  'Make ro': '改为只读',
  'now read-write': '已改为读写',
  'now read-only': '已改为只读',
  'Revoke': '吊销',
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
  'You govern ${b} — ask an administrator for a delegation.': '你治理：${b} — 请向管理员申请委派。',
  'no base view yet': '尚无基视图',
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
  '${at} by ${by}': '${at} 由 ${by}',
  'Withdraw': '撤销',
  'Withdraw this delegation?': '撤销这条委派？',
  '${account} stops governing ${base} and everything derived from it, on its next call.': '${account} 将在下一次调用时停止治理 ${base} 及其全部派生。',
  'withdrawn': '已撤销',
  'nothing delegated yet — a new account governs no view until it is': '尚无委派 — 新账号在获得委派前治理不了任何视图',

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
  'Every call the control plane answered, by any key or account. Failures are here too — a wrong credential included.': '控制面应答的每一次调用，任何密钥或账号。失败也在其中 — 包括凭据错误的调用。',
  'Only your own activity and your views’ keys; the full log is an administrator’s view.': '仅你的操作与你视图的密钥；完整日志是管理员视图。',
  'the fork tree is gone, but its definition could not be removed: ${e}': 'fork 树已删除，但其定义无法移除：${e}',
  'view created': '视图已创建',
  'New api key': '新的 API 密钥',
  'account': '账号',
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
const wrap = (...nodes) => el('div', null, ...nodes);

function toast(msg, kind) {
  const node = el('div', { class: 'toast ' + (kind || '') }, String(msg));
  $('toast-host').appendChild(node);
  setTimeout(() => node.remove(), kind === 'bad' ? 15000 : 5000);
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

function tableNode(cols, rows, emptyText) {
  const node = el('table');
  node.appendChild(el('thead', null, el('tr', null, cols.map((c) => el('th', null, c.t)))));
  const body = el('tbody');
  if (!rows || !rows.length) {
    body.appendChild(el('tr', null, el('td', { colspan: cols.length, class: 'muted' },
      emptyText || t('nothing here'))));
  }
  for (const r of rows || []) {
    body.appendChild(el('tr', null, cols.map((c) => {
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
const when = (cond, ...nodes) => (cond ? nodes : null);

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

const reload = () => go(ST.page);

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
  const info = await api('info', {});
  const cards = el('div', { class: 'cards' });
  const card = (k, v, sub) => cards.appendChild(el('div', { class: 'card' },
    el('div', { class: 'n' }, (v === null || v === undefined) ? '—' : String(v)),
    el('div', { class: 'k' }, k),
    sub ? hint(sub) : null));

  card(t('backend'), info.backend);
  card(t('signed in as'), info.actor || ST.who.account, info.role || '');

  if (isSuper()) {
    const si = await api('store.info', {});
    card(t('base views'), si.views);
    card(t('definitions'), si.definitions);
    card(t('accounts'), si.accounts);
    card(t('live keys'), si.liveKeys);
    card(t('mount sessions'), si.liveSessions);
    card(t('open trees'), si.residentEngines, t('engines this server is holding'));
    root.appendChild(cards);
    root.appendChild(bar(el('h2', null, t('This deployment'))));
    root.appendChild(el('dl', { class: 'kv' },
      el('dt', null, t('store')), el('dd', { class: 'mono' }, String(si.describe || '')),
      el('dt', null, t('meta root')), el('dd', { class: 'mono' }, String(si.base || '')),
      el('dt', null, t('server role')), el('dd', null, String(si.role || '')),
      el('dt', null, t('first owner')), el('dd', { class: 'muted' },
        t('created by `meta-server --admin-user` while the account store was empty; the rest come from the Accounts page'))));
  } else {
    card(t('base views you govern'), (ST.who.bases || []).length);
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
  root.appendChild(bar(el('h2', null, t('Recent governance calls'))));
  root.appendChild(calls.ok ? auditTable(calls.r) : el('p', { class: 'err' }, calls.error));
}

function auditTable(rows) {
  return tableNode([
    { t: t('when'), k: 'ts', class: 'mono' },
    { t: t('key'), render: (r) => r.key_label || r.key_id || null, class: 'mono' },
    { t: t('plane'), k: 'method', class: 'mono' },
    { t: t('op'), k: 'tool', class: 'mono' },
    { t: t('result'), render: (r) => el('span', { class: r.ok ? 'ok-text' : 'err' }, r.ok ? t('ok') : t('failed')) },
    { t: t('detail'), k: 'detail', class: 'wrap' },
  ], rows, t('no governance calls yet'));
}

/* ------------------------------------------------------------------ views --- */

async function pageViews(root) {
  await loadViews(true);
  root.appendChild(el('div', { class: 'split' },
    el('div', { class: 'col-narrow' },
      bar(el('h2', null, t('Views')), action(t('refresh'), reload, { class: 'link' })),
      el('ul', { id: 'view-list', class: 'list' }),
      el('div', { id: 'view-new', class: 'panel' })),
    el('div', { class: 'col-wide', id: 'view-detail' })));
  await renderViewList();
  await renderDetail(ST.sel);
}

async function renderViewList() {
  const ul = $('view-list');
  if (!ul) return;
  clear(ul);
  if (!ST.views.length) {
    ul.appendChild(el('li', { class: 'muted' }, t('no views you can see')));
    return;
  }
  for (const v of ST.views) {
    ul.appendChild(el('li', {
      class: ST.sel === v.name ? 'sel' : '',
      onclick: () => { ST.sel = v.name; renderViewList().then(() => renderDetail(v.name)); },
    }, el('span', { class: 'mono' }, v.name), el('span', { class: 'kind' }, v.kind)));
  }
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
    el('span', { class: 'badge' }, s.kind),
    when(s.sessions > 0, el('span', { class: 'badge on-state' }, t('${n} mount session(s)', { n: s.sessions })))));

  box.appendChild(el('dl', { class: 'kv' },
    el('dt', null, t('base')), el('dd', { class: 'mono' }, s.base || '—'),
    el('dt', null, t('stamp')), el('dd', { class: 'mono' }, stampText(s.stamp)),
    el('dt', null, t('tree')), el('dd', null, s.tree ? t('physical tree') : s.forkTree ? t('fork tree') : t('none of its own')),
    el('dt', null, t('on this server')), el('dd', { class: 'muted' }, s.resident ? t('open') : t('not open')),
    el('dt', null, t('derived')), el('dd', null, (s.family || []).length ? (s.family || []).join(', ') : t('nothing derived from it')),
    el('dt', null, t('keys')), el('dd', null, (s.keys === null || s.keys === undefined) ? '—' : t('${n} bound to this name', { n: s.keys }))));

  if (s.definition) box.appendChild(el('pre', null, s.definition));
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
  }));
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
  box.appendChild(tableNode([
    { t: t('label'), k: 'label' },
    { t: t('id'), k: 'id', class: 'mono' },
    { t: t('scope'), k: 'scope', class: 'mono' },
    { t: t('mode'), render: (r) => (r.ro ? t('read-only') : t('read-write')) },
    { t: t('owner'), k: 'owner' },
    { t: t('last used'), k: 'last_used_at' },
    { t: t('state'), render: (r) => (r.revoked_at ? el('span', { class: 'badge off' }, t('revoked')) : el('span', { class: 'badge on-state' }, t('live'))) },
    { t: '', class: 'actions', render: (r) => (r.revoked_at ? null : revokeBtn(r)) },
  ], mine, t('no keys bound to this view yet')));
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

async function pageKeys(root) {
  await loadViews();
  root.appendChild(bar(el('h2', null, t('API keys')), action(t('refresh'), reload, { class: 'link' })));

  const labelI = el('input', { placeholder: 'etl job', size: 16 });
  const scopeS = el('select', null, ...(isSuper()
    ? [{ v: 'mount', t: t('mount — a client mount') }, { v: 'mcp', t: t('mcp — an agent') }, { v: 'admin', t: t('admin — the governance plane') }, { v: 'any', t: t('any — every plane') }]
    : [{ v: 'mount', t: t('mount — a client mount') }, { v: 'mcp', t: t('mcp — an agent') }]).map((o) => el('option', { value: o.v }, o.t)));
  const viewS = el('select', null,
    el('option', { value: '' }, isSuper() ? t('(no view binding — every tree)') : t('(choose a view)')),
    ...ST.views.map((v) => el('option', { value: v.name }, `${v.name} [${v.kind}]`)));
  const ro = el('input', { type: 'checkbox' });
  const panel = el('div', { class: 'panel' });
  panel.appendChild(el('h3', null, t('Mint a key')));
  panel.appendChild(field(t('label'), labelI));
  panel.appendChild(field(t('plane'), scopeS, hint(isSuper() ? '' : t('a delegated account mints client keys only'))));
  panel.appendChild(field(t('view'), viewS));
  panel.appendChild(el('label', null, ro, ' ' + t('read-only')));
  panel.appendChild(action(t('Create key'), async () => {
    const args = { label: labelI.value.trim(), scope: scopeS.value, ro: ro.checked };
    if (viewS.value) args.view = viewS.value;
    const r = await api('apikey.create', args);
    labelI.value = '';
    // The reply carries the plaintext `<id>:<secret>` once; `view` is what THIS
    // request asked for, because the create reply reports the key, not the row.
    modal(t('New api key'), wrap(
      el('p', null, t('This is the only time the secret is shown — GroveFS stores a digest.')),
      el('p', { class: 'secret' }, r.key),
      el('p', { class: 'muted' }, t('label ${l} · scope ${s} · owner ${o} · ${b}', {
        l: r.label, s: r.scope, o: r.owner || '—',
        b: args.view ? t('bound to ${v}', { v: args.view }) : t('not bound to a view'),
      })),
      el('pre', null, r.key)),
    [{ t: t('I have saved it'), onclick: (btn, err, close) => close() }]);
    await reload();
  }));
  if (!isSuper()) panel.appendChild(hint(t('an unbound key reaches every tree, so a delegated account must name one')));
  root.appendChild(panel);

  const rows = await api('apikey.list', {});
  root.appendChild(tableNode([
    { t: t('label'), k: 'label' },
    { t: t('id'), k: 'id', class: 'mono' },
    { t: t('owner'), render: (r) => r.owner || null },
    { t: t('scope'), k: 'scope', class: 'mono' },
    { t: t('view'), render: (r) => r.view || el('span', { class: 'muted' }, t('any view')) },
    { t: t('mode'), render: (r) => (r.ro ? t('ro') : t('rw')) },
    { t: t('created'), k: 'created_at', class: 'muted' },
    { t: t('last used'), k: 'last_used_at', class: 'muted' },
    { t: t('state'), render: (r) => (r.revoked_at ? el('span', { class: 'badge off' }, t('revoked')) : el('span', { class: 'badge on-state' }, t('live'))) },
    {
      t: '', class: 'actions', render: (r) => (r.revoked_at ? null : el('div', { class: 'row' }, roBtn(r), revokeBtn(r))),
    },
  ], rows, t('no keys you can see')));
  root.appendChild(el('p', { class: 'muted' },
    t('A key’s secret is shown once, at creation. Revoke is the only way back, and it takes effect on the key’s NEXT call — a live mount keeps its open trees until then.')));
}

function roBtn(r) {
  return action(r.ro ? t('Make rw') : t('Make ro'), async () => {
    await api('apikey.set_ro', { id: r.id, ro: !r.ro });
    toast(r.ro ? t('now read-write') : t('now read-only'));
    await reload();
  });
}

function revokeBtn(r) {
  return action(t('Revoke'), async () => {
    const note = r.view ? t('bound to ${v}', { v: r.view }) : t('NOT bound to a view — it reaches every tree');
    if (!(await ask(t('Revoke this key?'),
      t('${l} (${i}) · scope ${s} · ${note}\n\nLive mounts keep working until their next operation.', { l: r.label, i: r.id, s: r.scope, note }), t('Revoke')))) return;
    await api('apikey.revoke', { id: r.id });
    toast(t('revoked'));
    await reload();
  }, { class: 'danger' });
}

/* -------------------------------------------------------------- accounts --- */

async function pageAccounts(root) {
  await loadViews();
  const bases = ST.views.filter((v) => v.kind === 'base').map((v) => v.name);
  root.appendChild(bar(el('h2', null, t('Accounts')),
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
  panel.appendChild(field(t('role'), roleS));
  panel.appendChild(action(t('Create'), async (btn) => {
    try {
      const r = await api('account.create', { account: accI.value.trim(), password: pwI.value, role: roleS.value });
      pwI.value = '';
      toast(t('${account} created as ${role} — ${note}', { account: r.account, role: r.role, note: r.note }));
      await reload();
    } catch (e) { btn.disabled = false; throw e; }
  }));
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
    root.appendChild(el('p', { class: 'muted' }, t('The account list is an administrator’s view: ${e}', { e: accts.error })));
    root.appendChild(el('div', { class: 'row' },
      el('span', { class: 'badge' }, ST.who.role),
      el('span', { class: 'mono' }, ST.who.account),
      action(t('Change my password'), async () => { await passwdDialog({ account: ST.who.account, role: ST.who.role }); })));
    root.appendChild(el('p', { class: 'muted' },
      t('You govern ${b} — ask an administrator for a delegation.', { b: (ST.who.bases || []).length ? ST.who.bases.join(', ') : t('no base view yet') })));
    return;
  }
  const rows = accts.r;
  root.appendChild(tableNode([
    { t: t('account'), k: 'account', class: 'mono' },
    { t: t('role'), render: (r) => el('span', { class: 'badge ' + (r.role === 'admin' ? 'admin' : '') }, r.role) },
    { t: t('delegated bases'), render: (r) => ((r.bases || []).length ? (r.bases || []).join(', ') : el('span', { class: 'muted' }, t('none'))) },
    { t: t('created'), k: 'created_at', class: 'muted' },
    { t: t('last sign-in'), k: 'last_login_at', class: 'muted' },
    { t: t('state'), render: (r) => (r.disabled_at ? el('span', { class: 'badge off' }, t('disabled')) : el('span', { class: 'badge on-state' }, t('enabled'))) },
    {
      t: '', class: 'actions', render: (r) => el('div', { class: 'row' },
        el('button', { type: 'button', onclick: () => passwdDialog(r) }, r.account === ST.who.account ? t('My password') : t('Password')),
        el('button', { type: 'button', onclick: () => roleDialog(r) }, r.role === 'admin' ? t('Make user') : t('Make admin')),
        action(r.disabled_at ? t('Enable') : t('Disable'), async () => {
          const want = !r.disabled_at;
          if (!(await ask(want ? t('Disable sign-in?') : t('Enable sign-in?'),
            want ? t('${account} could no longer sign in; its open sessions die on their next call.', { account: r.account })
                 : t('${account} may sign in again.', { account: r.account }),
            want ? t('Disable') : t('Enable')))) return;
          await api('account.disable', { account: r.account, disabled: want });
          toast(want ? t('${account} disabled', { account: r.account }) : t('${account} enabled', { account: r.account }));
          await reload();
        }),
        action(t('Delete'), async () => {
          if (!(await ask(t('Delete this account?'),
            t('${account} — its delegations go with it.\nIts api keys DO NOT; the server reports how many are still live.', { account: r.account }), t('Delete')))) return;
          const res = await api('account.delete', { account: r.account });
          toast(t('${account}: ${note}', { account: res.account, note: res.note }));
          await reload();
        }, { class: 'danger' })),
    },
  ], rows, t('no accounts')));

  const gAcc = el('select', null, (rows || []).map((a) => el('option', { value: a.account }, a.account)));
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
  }));
  root.appendChild(bar(el('h2', null, t('Delegations')),
    hint(t('an account governs a base view AND every lens, fork and alias derived from it'))));
  if (isSuper()) root.appendChild(gp);

  const grants = await api('grant.list', {});
  root.appendChild(tableNode([
    { t: t('account'), k: 'account', class: 'mono' },
    { t: t('base view'), k: 'base_view', class: 'mono' },
    { t: t('perm'), k: 'perm', class: 'mono' },
    { t: t('granted'), render: (r) => t('${at} by ${by}', { at: r.granted_at || '', by: r.granted_by || '—' }), class: 'muted' },
    {
      t: '', class: 'actions', render: (g) => action(t('Withdraw'), async () => {
        if (!(await ask(t('Withdraw this delegation?'),
          t('${account} stops governing ${base} and everything derived from it, on its next call.', { account: g.account, base: g.base_view }), t('Withdraw')))) return;
        await api('grant.remove', { account: g.account, base: g.base_view });
        toast(t('withdrawn'));
        await reload();
      }, { class: 'danger' }),
    },
  ], grants, t('nothing delegated yet — a new account governs no view until it is')));
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

async function pageAudit(root) {
  const limitS = el('select', { onchange: () => { ST.auditLimit = Number(limitS.value); go('audit'); } },
    el('option', { value: '50' }, '50'), el('option', { value: '200' }, '200'), el('option', { value: '1000' }, '1000'));
  limitS.value = String(ST.auditLimit || 50);
  root.appendChild(bar(el('h2', null, t('Recent governance calls')), limitS, action(t('refresh'), reload, { class: 'link' })));
  root.appendChild(auditTable(await api('apikey.calls', { limit: Number(limitS.value) })));
  root.appendChild(el('p', { class: 'muted' }, isSuper()
    ? t('Every call the control plane answered, by any key or account. Failures are here too — a wrong credential included.')
    : t('Only your own activity and your views’ keys; the full log is an administrator’s view.')));
}

/* ---------------------------------------------------------------- start --- */

applyStatic();
boot();

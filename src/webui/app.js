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
  const t = el('div', { class: 'toast ' + (kind || '') }, String(msg));
  $('toast-host').appendChild(t);
  setTimeout(() => t.remove(), kind === 'bad' ? 15000 : 5000);
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
      el('button', { type: 'button', onclick: () => { close(); if (oncancel) oncancel(); } }, 'Cancel'),
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
      t: confirmText || 'Confirm', class: 'danger',
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
      emptyText || 'nothing here')));
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
const hint = (t) => el('span', { class: 'muted' }, t);
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
  if (status === 401) { signedOut(); throw new Error('signed out'); }
  if (!j) throw new Error(`the gateway answered HTTP ${status} without a JSON body`);
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
  showLogin('the session ended — sign in again');
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
  err.textContent = (j && j.error) || `sign-in failed (HTTP ${status})`;
});

$('logout').addEventListener('click', async () => {
  try { await req('/api/logout', { method: 'POST', headers: { 'x-grovefs-csrf': ST.csrf || '' } }); }
  catch (e) { /* the session is gone either way */ }
  ST.csrf = null;
  ST.who = null;
  showLogin();
  toast('signed out');
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
  root.appendChild(hint('loading…'));
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
    const t = line.trim();
    if (t.startsWith(kw + ' ') || t.startsWith(kw + '\t')) return t.split(/\s+/)[1];
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

  card('backend', info.backend);
  card('signed in as', info.actor || ST.who.account, info.role || '');

  if (isSuper()) {
    const si = await api('store.info', {});
    card('base views', si.views);
    card('definitions', si.definitions);
    card('accounts', si.accounts);
    card('live keys', si.liveKeys);
    card('mount sessions', si.liveSessions);
    card('open trees', si.residentEngines, 'engines this server is holding');
    root.appendChild(cards);
    root.appendChild(bar(el('h2', null, 'This deployment')));
    root.appendChild(el('dl', { class: 'kv' },
      el('dt', null, 'store'), el('dd', { class: 'mono' }, String(si.describe || '')),
      el('dt', null, 'meta root'), el('dd', { class: 'mono' }, String(si.base || '')),
      el('dt', null, 'server role'), el('dd', null, String(si.role || '')),
      el('dt', null, 'first owner'), el('dd', { class: 'muted' },
        'created by `meta-server --admin-user` while the account store was empty; the rest come from the Accounts page')));
  } else {
    card('base views you govern', (ST.who.bases || []).length);
    root.appendChild(cards);
    root.appendChild(bar(el('h2', null, 'Your reach')));
    root.appendChild(el('p', { class: 'muted' },
      'Deployment-wide counts belong to an administrator. You govern these base views and everything derived from them:'));
    const list = el('ul', { class: 'list' });
    for (const b of (ST.who.bases || []).slice(0, 50)) {
      const s = await soft('view.stats', { view: b });
      list.appendChild(el('li', { onclick: () => { ST.sel = b; go('views'); } },
        el('span', { class: 'mono' }, b),
        el('span', { class: 'kind' }, s.ok ? `${(s.r.family || []).length} derived` : 'unreachable')));
    }
    root.appendChild(list);
  }

  const calls = await soft('apikey.calls', { limit: 8 });
  root.appendChild(bar(el('h2', null, 'Recent governance calls')));
  root.appendChild(calls.ok ? auditTable(calls.r) : el('p', { class: 'err' }, calls.error));
}

function auditTable(rows) {
  return tableNode([
    { t: 'when', k: 'ts', class: 'mono' },
    { t: 'key', render: (r) => r.key_label || r.key_id || null, class: 'mono' },
    { t: 'plane', k: 'method', class: 'mono' },
    { t: 'op', k: 'tool', class: 'mono' },
    { t: 'result', render: (r) => el('span', { class: r.ok ? 'ok-text' : 'err' }, r.ok ? 'ok' : 'failed') },
    { t: 'detail', k: 'detail', class: 'wrap' },
  ], rows, 'no governance calls yet');
}

/* ------------------------------------------------------------------ views --- */

async function pageViews(root) {
  await loadViews(true);
  root.appendChild(el('div', { class: 'split' },
    el('div', { class: 'col-narrow' },
      bar(el('h2', null, 'Views'), action('refresh', reload, { class: 'link' })),
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
    ul.appendChild(el('li', { class: 'muted' }, 'no views you can see'));
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
    host.appendChild(hint('pick a view on the left'));
    return;
  }
  const s = await api('view.stats', { view: name });
  const box = el('div', { class: 'detail' });
  box.appendChild(el('div', { class: 'head' },
    el('h2', { class: 'mono' }, s.name),
    el('span', { class: 'badge' }, s.kind),
    when(s.sessions > 0, el('span', { class: 'badge on-state' }, `${s.sessions} mount session(s)`))));

  box.appendChild(el('dl', { class: 'kv' },
    el('dt', null, 'base'), el('dd', { class: 'mono' }, s.base || '—'),
    el('dt', null, 'stamp'), el('dd', { class: 'mono' }, stampText(s.stamp)),
    el('dt', null, 'tree'), el('dd', null, s.tree ? 'physical tree' : s.forkTree ? 'fork tree' : 'none of its own'),
    el('dt', null, 'on this server'), el('dd', { class: 'muted' }, s.resident ? 'open' : 'not open'),
    el('dt', null, 'derived'), el('dd', null, (s.family || []).length ? (s.family || []).join(', ') : 'nothing derived from it'),
    el('dt', null, 'keys'), el('dd', null, (s.keys === null || s.keys === undefined) ? '—' : `${s.keys} bound to this name`)));

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
    }, 'Export dump'),
    when(can('fork.create'), el('button', { type: 'button', onclick: () => forkDialog(s) }, 'Fork…')),
    when(can('def.put'), el('button', { type: 'button', onclick: () => defineDialog(s) }, 'Lens / alias…')),
    when(s.kind === 'fork' && can('fork.drop'), el('button', {
      type: 'button', class: 'danger',
      onclick: async () => {
        if (!(await ask('Drop this fork?',
          `${name}: its own tree is deleted. Its base ${s.base || ''} and every shared object stay.`, 'Drop fork'))) return;
        await api('fork.drop', { name });
        try { await api('def.remove', { name }); }
        catch (e) { toast(`the fork tree is gone, but its definition could not be removed: ${e.message}`, 'bad'); }
        ST.sel = null;
        await reload();
      },
    }, 'Drop fork')),
    when(s.kind !== 'base' && can('def.remove'), el('button', {
      type: 'button', class: 'danger',
      onclick: async () => {
        if (!(await ask('Remove this definition?',
          `${name} =\n${s.definition || ''}\n\nOnly the name goes away — no tree and no object is deleted.`, 'Remove definition'))) return;
        await api('def.remove', { name });
        toast('definition removed');
        await reload();
      },
    }, 'Remove definition'))));

  await renderImport(box, s);
  if (can('apikey.create')) await renderKeyForView(box, s);
  if (can('def.put') && s.kind !== 'base') await renderDefEdit(box, s);
}

function forkDialog(s) {
  const nameI = el('input', { placeholder: `${s.name}-exp1`, size: 24 });
  const seed = el('input', { type: 'checkbox' });
  seed.checked = true;
  const m = modal(`Fork ${s.name}`, wrap(
    hint(`A fork copies the METADATA tree into a new writable view; content bytes stay shared until something in the fork rewrites them.`),
    field('new view name', nameI),
    el('label', null, seed, ' copy the current tree (unchecked: an empty fork to graft into)')),
  [{
    t: 'Create fork',
    onclick: async (btn, err) => {
      err.textContent = '';
      try {
        const r = await api('fork.create', { base: s.name, name: nameI.value.trim(), seed: seed.checked, with_def: true });
        m.close();
        toast(`forked ${s.name} → ${nameI.value.trim()} (${r.nodes} nodes, ${r.blobs} blobs)`);
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
    el('option', { value: 'derived' }, 'derived lens (read-only shape over a base)'),
    el('option', { value: 'alias' }, 'alias (another name for it)'));
  const textT = el('textarea', { rows: 8, spellcheck: 'false' });
  const preset = () => {
    textT.value = kindS.value === 'alias'
      ? `alias ${s.name}\n`
      : `base ${s.name}\n# hide /subtree\n# only /shown\n# map /src/path /shown/path\n`;
  };
  kindS.addEventListener('change', preset);
  preset();
  const m = modal(`Define a view over ${s.name}`, wrap(
    hint('The definition text is exactly what `grovefs view define --file` takes.'),
    field('kind', kindS),
    field('new view name', nameI),
    field('definition', textT)),
  [{
    t: 'Save',
    onclick: async (btn, err) => {
      err.textContent = '';
      try {
        await api('def.put', { name: nameI.value.trim(), text: textT.value });
        m.close();
        toast('definition saved');
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
    panel.appendChild(hint('creating a BASE view is an administrator’s act — it chooses where bytes live.'));
    return;
  }
  const nameI = el('input', { placeholder: 'project-x', size: 16 });
  const instI = el('input', { placeholder: '(least loaded)', size: 14 });
  panel.appendChild(el('h3', null, 'New base view'));
  panel.appendChild(field('name', nameI));
  panel.appendChild(field('PG instance', instI));
  panel.appendChild(action('Create', async (btn) => {
    const args = { view: nameI.value.trim() };
    if (instI.value.trim()) args.pg_instance = instI.value.trim();
    try {
      const r = await api('view.create', args);
      toast(typeof r === 'string' ? r : 'created');
      ST.sel = nameI.value.trim();
      await reload();
    } catch (e) {
      // The server's own reason ("already has a definition — remove it first")
      // is the useful part, so it goes in the toast rather than the dialog.
      btn.disabled = false;
      throw e;
    }
  }));
  panel.appendChild(hint('a base view owns physical deletion; lenses, aliases and forks reshape one'));
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
  row.appendChild(field('metadata dump', fileI));
  row.appendChild(field('as view', target));
  row.appendChild(el('label', null, repl, ' replace an existing tree'));
  if (isSuper()) row.appendChild(el('label', null, asBase, ' import as a BASE view'));
  row.appendChild(action('Import', async (btn) => {
    const f = fileI.files && fileI.files[0];
    if (!f) throw new Error('choose a dump file first');
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
    if (!j || j.ok === false) throw new Error((j && j.error) || `import failed (HTTP ${r.status})`);
    toast(`imported ${j.result.nodes} nodes, ${j.result.blobs} blobs as ${j.result.kind}`);
    ST.sel = target.value.trim();
    await reload();
  }));
  box.appendChild(bar(el('h3', null, 'Import a dump')));
  box.appendChild(row);
  box.appendChild(hint(isSuper()
    ? 'a dump becomes a BASE view, or a fork of a view you govern — content bytes are never in a dump'
    : `imported as a FORK of ${s.base || s.name}; only an administrator can import a new base view`));
}

/** Keys bound to THIS view: a view owner can rotate what its mounts use without
    walking the deployment-wide list. */
async function renderKeyForView(box, s) {
  const all = await api('apikey.list', {});
  const mine = (all || []).filter((k) => k.view === s.name);
  box.appendChild(bar(el('h3', null, `Keys bound to ${s.name}`)));
  box.appendChild(tableNode([
    { t: 'label', k: 'label' },
    { t: 'id', k: 'id', class: 'mono' },
    { t: 'scope', k: 'scope', class: 'mono' },
    { t: 'mode', render: (r) => (r.ro ? 'read-only' : 'read-write') },
    { t: 'owner', k: 'owner' },
    { t: 'last used', k: 'last_used_at' },
    { t: 'state', render: (r) => (r.revoked_at ? el('span', { class: 'badge off' }, 'revoked') : el('span', { class: 'badge on-state' }, 'live')) },
    { t: '', class: 'actions', render: (r) => (r.revoked_at ? null : revokeBtn(r)) },
  ], mine, 'no keys bound to this view yet'));
}

async function renderDefEdit(box, s) {
  const t = el('textarea', { rows: 6, spellcheck: 'false' });
  t.value = s.definition || '';
  box.appendChild(bar(el('h3', null, 'Edit this definition')));
  box.appendChild(t);
  box.appendChild(action('Save', async () => {
    await api('def.put', { name: s.name, text: t.value });
    toast('saved — live mounts pick it up within their attr TTL (≤2s)');
    await renderDetail(s.name);
  }));
}

/* ------------------------------------------------------------------ keys --- */

async function pageKeys(root) {
  await loadViews();
  root.appendChild(bar(el('h2', null, 'API keys'), action('refresh', reload, { class: 'link' })));

  const labelI = el('input', { placeholder: 'etl job', size: 16 });
  const scopeS = el('select', null, ...(isSuper()
    ? [{ v: 'mount', t: 'mount — a client mount' }, { v: 'mcp', t: 'mcp — an agent' }, { v: 'admin', t: 'admin — the governance plane' }, { v: 'any', t: 'any — every plane' }]
    : [{ v: 'mount', t: 'mount — a client mount' }, { v: 'mcp', t: 'mcp — an agent' }]).map((o) => el('option', { value: o.v }, o.t)));
  const viewS = el('select', null,
    el('option', { value: '' }, isSuper() ? '(no view binding — every tree)' : '(choose a view)'),
    ...ST.views.map((v) => el('option', { value: v.name }, `${v.name} [${v.kind}]`)));
  const ro = el('input', { type: 'checkbox' });
  const panel = el('div', { class: 'panel' });
  panel.appendChild(el('h3', null, 'Mint a key'));
  panel.appendChild(field('label', labelI));
  panel.appendChild(field('plane', scopeS, hint(isSuper() ? '' : 'a delegated account mints client keys only')));
  panel.appendChild(field('view', viewS));
  panel.appendChild(el('label', null, ro, ' read-only'));
  panel.appendChild(action('Create key', async () => {
    const args = { label: labelI.value.trim(), scope: scopeS.value, ro: ro.checked };
    if (viewS.value) args.view = viewS.value;
    const r = await api('apikey.create', args);
    labelI.value = '';
    // The reply carries the plaintext `<id>:<secret>` once; `view` is what THIS
    // request asked for, because the create reply reports the key, not the row.
    modal('New api key', wrap(
      el('p', null, 'This is the only time the secret is shown — GroveFS stores a digest.'),
      el('p', { class: 'secret' }, r.key),
      el('p', { class: 'muted' }, `label ${r.label} · scope ${r.scope} · owner ${r.owner || '—'} · ${args.view ? `bound to ${args.view}` : 'not bound to a view'}`),
      el('pre', null, r.key)),
    [{ t: 'I have saved it', onclick: (btn, err, close) => close() }]);
    await reload();
  }));
  if (!isSuper()) panel.appendChild(hint('an unbound key reaches every tree, so a delegated account must name one'));
  root.appendChild(panel);

  const rows = await api('apikey.list', {});
  root.appendChild(tableNode([
    { t: 'label', k: 'label' },
    { t: 'id', k: 'id', class: 'mono' },
    { t: 'owner', render: (r) => r.owner || null },
    { t: 'scope', k: 'scope', class: 'mono' },
    { t: 'view', render: (r) => r.view || el('span', { class: 'muted' }, 'any view') },
    { t: 'mode', render: (r) => (r.ro ? 'ro' : 'rw') },
    { t: 'created', k: 'created_at', class: 'muted' },
    { t: 'last used', k: 'last_used_at', class: 'muted' },
    { t: 'state', render: (r) => (r.revoked_at ? el('span', { class: 'badge off' }, 'revoked') : el('span', { class: 'badge on-state' }, 'live')) },
    {
      t: '', class: 'actions', render: (r) => (r.revoked_at ? null : el('div', { class: 'row' }, roBtn(r), revokeBtn(r))),
    },
  ], rows, 'no keys you can see'));
  root.appendChild(el('p', { class: 'muted' },
    'A key’s secret is shown once, at creation. Revoke is the only way back, and it takes effect on the key’s NEXT call — a live mount keeps its open trees until then.'));
}

function roBtn(r) {
  return action(r.ro ? 'Make rw' : 'Make ro', async () => {
    await api('apikey.set_ro', { id: r.id, ro: !r.ro });
    toast(r.ro ? 'now read-write' : 'now read-only');
    await reload();
  });
}

function revokeBtn(r) {
  return action('Revoke', async () => {
    const note = r.view ? `bound to ${r.view}` : 'NOT bound to a view — it reaches every tree';
    if (!(await ask('Revoke this key?',
      `${r.label} (${r.id}) · scope ${r.scope} · ${note}\n\nLive mounts keep working until their next operation.`, 'Revoke'))) return;
    await api('apikey.revoke', { id: r.id });
    toast('revoked');
    await reload();
  }, { class: 'danger' });
}

/* -------------------------------------------------------------- accounts --- */

async function pageAccounts(root) {
  await loadViews();
  const bases = ST.views.filter((v) => v.kind === 'base').map((v) => v.name);
  root.appendChild(bar(el('h2', null, 'Accounts'),
    when(!isSuper(), el('span', { class: 'badge off' }, 'administrators act here')),
    action('refresh', reload, { class: 'link' })));

  const accI = el('input', { placeholder: 'bob', size: 14 });
  const pwI = el('input', { type: 'password', autocomplete: 'new-password', size: 20 });
  const roleS = el('select', null,
    el('option', { value: 'user' }, 'user — governs what it is delegated'),
    el('option', { value: 'admin' }, 'admin — governs the deployment'));
  const panel = el('div', { class: 'panel' });
  panel.appendChild(el('h3', null, 'Create an account'));
  panel.appendChild(field('account', accI, hint('a-z 0-9 . _ -, 2–32 chars')));
  panel.appendChild(field('password', pwI, hint('at least 10 characters')));
  panel.appendChild(field('role', roleS));
  panel.appendChild(action('Create', async (btn) => {
    try {
      const r = await api('account.create', { account: accI.value.trim(), password: pwI.value, role: roleS.value });
      pwI.value = '';
      toast(`${r.account} created as ${r.role} — ${r.note}`);
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
    root.appendChild(bar(el('h2', null, 'Your account')));
    root.appendChild(el('p', { class: 'muted' }, `The account list is an administrator's view: ${accts.error}`));
    root.appendChild(el('div', { class: 'row' },
      el('span', { class: 'badge' }, ST.who.role),
      el('span', { class: 'mono' }, ST.who.account),
      action('Change my password', async () => { await passwdDialog({ account: ST.who.account, role: ST.who.role }); })));
    root.appendChild(el('p', { class: 'muted' },
      `You govern ${(ST.who.bases || []).length ? ST.who.bases.join(', ') : 'no base view yet'} — ask an administrator for a delegation.`));
    return;
  }
  const rows = accts.r;
  root.appendChild(tableNode([
    { t: 'account', k: 'account', class: 'mono' },
    { t: 'role', render: (r) => el('span', { class: 'badge ' + (r.role === 'admin' ? 'admin' : '') }, r.role) },
    { t: 'delegated bases', render: (r) => ((r.bases || []).length ? (r.bases || []).join(', ') : el('span', { class: 'muted' }, 'none')) },
    { t: 'created', k: 'created_at', class: 'muted' },
    { t: 'last sign-in', k: 'last_login_at', class: 'muted' },
    { t: 'state', render: (r) => (r.disabled_at ? el('span', { class: 'badge off' }, 'disabled') : el('span', { class: 'badge on-state' }, 'enabled')) },
    {
      t: '', class: 'actions', render: (r) => el('div', { class: 'row' },
        el('button', { type: 'button', onclick: () => passwdDialog(r) }, r.account === ST.who.account ? 'My password' : 'Password'),
        el('button', { type: 'button', onclick: () => roleDialog(r) }, r.role === 'admin' ? 'Make user' : 'Make admin'),
        action(r.disabled_at ? 'Enable' : 'Disable', async () => {
          const want = !r.disabled_at;
          if (!(await ask(want ? 'Disable sign-in?' : 'Enable sign-in?',
            `${r.account}${want ? ' could no longer sign in; its open sessions die on their next call.' : ' may sign in again.'}`,
            want ? 'Disable' : 'Enable'))) return;
          await api('account.disable', { account: r.account, disabled: want });
          toast(want ? `${r.account} disabled` : `${r.account} enabled`);
          await reload();
        }),
        action('Delete', async () => {
          if (!(await ask('Delete this account?',
            `${r.account} — its delegations go with it.\nIts api keys DO NOT; the server reports how many are still live.`, 'Delete'))) return;
          const res = await api('account.delete', { account: r.account });
          toast(`${res.account}: ${res.note}`);
          await reload();
        }, { class: 'danger' })),
    },
  ], rows, 'no accounts'));

  const gAcc = el('select', null, (rows || []).map((a) => el('option', { value: a.account }, a.account)));
  const gBase = el('select', null, (bases.length ? bases : ST.views.map((v) => v.name)).map((n) => el('option', { value: n }, n)));
  const gp = el('div', { class: 'panel' });
  gp.appendChild(el('h3', null, 'Delegate a base view'));
  gp.appendChild(field('account', gAcc));
  gp.appendChild(field('base view', gBase));
  gp.appendChild(action('Grant', async (btn) => {
    try {
      const r = await api('grant.add', { account: gAcc.value, base: gBase.value });
      toast(r.asNamed ? `${r.account} governs ${r.base}` : `${r.account} governs ${r.base} — that is what the name you chose resolves to`);
      await reload();
    } catch (e) { btn.disabled = false; throw e; }
  }));
  root.appendChild(bar(el('h2', null, 'Delegations'),
    hint('an account governs a base view AND every lens, fork and alias derived from it')));
  if (isSuper()) root.appendChild(gp);

  const grants = await api('grant.list', {});
  root.appendChild(tableNode([
    { t: 'account', k: 'account', class: 'mono' },
    { t: 'base view', k: 'base_view', class: 'mono' },
    { t: 'perm', k: 'perm', class: 'mono' },
    { t: 'granted', render: (r) => `${r.granted_at || ''} by ${r.granted_by || '—'}`, class: 'muted' },
    {
      t: '', class: 'actions', render: (g) => action('Withdraw', async () => {
        if (!(await ask('Withdraw this delegation?',
          `${g.account} stops governing ${g.base_view} and everything derived from it, on its next call.`, 'Withdraw'))) return;
        await api('grant.remove', { account: g.account, base: g.base_view });
        toast('withdrawn');
        await reload();
      }, { class: 'danger' }),
    },
  ], grants, 'nothing delegated yet — a new account governs no view until it is'));
}

function passwdDialog(r) {
  const pw1 = el('input', { type: 'password', autocomplete: 'new-password', size: 26 });
  const pw2 = el('input', { type: 'password', autocomplete: 'new-password', size: 26 });
  const m = modal(`Password for ${r.account}`, wrap(
    field('new password', pw1, hint('at least 10 characters')),
    field('repeat it', pw2)),
  [{
    t: 'Change password',
    onclick: async (btn, err) => {
      err.textContent = '';
      if (pw1.value !== pw2.value) { err.textContent = 'the two entries differ'; return; }
      try {
        await api('account.passwd', { account: r.account, password: pw1.value });
        m.close();
        toast('password changed');
      } catch (e) { err.textContent = e.message; btn.disabled = false; }
    },
  }]);
}

function roleDialog(r) {
  const want = r.role === 'admin' ? 'user' : 'admin';
  const m = modal(`Make ${r.account} ${want}`, wrap(
    el('p', null, want === 'admin'
      ? 'An administrator governs every view, every account, and where a new base view is placed.'
      : 'A user governs only the base views it is delegated, plus their forks, aliases, lenses and keys.')),
  [{
    t: `Make ${want}`,
    onclick: async (btn, err) => {
      err.textContent = '';
      try {
        await api('account.role', { account: r.account, role: want });
        m.close();
        toast(`${r.account} is now ${want}`);
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
  root.appendChild(bar(el('h2', null, 'Recent governance calls'), limitS, action('refresh', reload, { class: 'link' })));
  root.appendChild(auditTable(await api('apikey.calls', { limit: Number(limitS.value) })));
  root.appendChild(el('p', { class: 'muted' }, isSuper()
    ? 'Every call the control plane answered, by any key or account. Failures are here too — a wrong credential included.'
    : 'Only your own activity and your views’ keys; the full log is an administrator’s view.'));
}

/* ---------------------------------------------------------------- start --- */

boot();

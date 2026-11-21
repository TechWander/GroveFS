import type { AuditRow } from '@/types'

/**
 * WHO made the call. The audit row itself has no account column: the control
 * plane prefixes a signed-in call's `detail` with `account=<who>@<ip>`, so the
 * person is parsed back out here — "who did this" is the first question anyone
 * asks of an audit log, and a table that cannot answer it is a text file.
 * A call made by a credential instead (a mount, an agent) has no account, and
 * says so with the key that signed.
 */
export function auditActor(r: AuditRow): string | null {
  const m = /account=([^\s@]+)/.exec(String(r.detail ?? ''))
  if (m) return m[1]
  return r.key_label || r.key_id || null
}

/** The distinct values of one field, sorted — what builds the audit page's two
    dropdowns from the rows actually loaded, so a new op appears in the filter
    the moment it appears in the log, and a renamed one cannot be offered as an
    empty choice. */
export function distinct(rows: AuditRow[], key: 'method' | 'tool'): string[] {
  return [...new Set(rows.map((r) => r[key]).filter((v): v is string => !!v))].sort()
}

/**
 * How far back the audit page offers to look. The window is applied CLIENT-side
 * over the rows already loaded, because the server's `limit` is a row count and a
 * person asking "what happened today" wants the hours, not the newest 200 of an
 * idle weekend.
 */
export const WINDOWS: [string, string][] = [
  ['all', 'all time'],
  ['1h', 'last hour'],
  ['24h', 'last 24 hours'],
  ['7d', 'last 7 days'],
]

const WIN_MS: Record<string, number> = { '1h': 36e5, '24h': 864e5, '7d': 7 * 864e5 }

export function withinWindow(ts: string, w: string): boolean {
  if (!w || w === 'all') return true
  const t0 = Date.parse(ts)
  const span = WIN_MS[w]
  return span !== undefined && Number.isFinite(t0) && Date.now() - t0 <= span
}

/**
 * Which credential a row was signed with, as the filter offers it.
 *
 * `key_id` is `api_keys.id` — the store's PRIMARY KEY, 16 hex on a key minted
 * after the format was shortened and 32 hex on a legacy one, because
 * `authenticate_for` splits on `:` and compares a digest, so it never looked at
 * length. It is NOT the secret and NOT the `head4…tail4` fingerprint: that is
 * `secret_hint`, a column of the keys table the audit row does not carry. A row
 * with no `key_id` is one the server answered before it recognised a credential
 * (a refused mount handshake, an anonymous probe), which is why the audit page
 * must be able to say "no key on it" instead of showing a blank.
 */
export interface KeyChoice {
  id: string
  label: string | null
}

export function keyChoices(rows: AuditRow[]): KeyChoice[] {
  const seen = new Map<string, string | null>()
  for (const r of rows) if (r.key_id && !seen.has(r.key_id)) seen.set(r.key_id, r.key_label ?? null)
  return [...seen]
    .map(([id, label]) => ({ id, label }))
    .sort((a, b) => (a.label ?? a.id).localeCompare(b.label ?? b.id) || a.id.localeCompare(b.id))
}

/** An id long enough to be unreadable in a column is shown head-first; the tail
    is there because two legacy 32-hex ids can share a prefix and a filter that
    offers two identical labels is a filter that lies. */
export function shortId(id: string): string {
  return id.length > 12 ? `${id.slice(0, 8)}…${id.slice(-4)}` : id
}

/**
 * The key an `apikey.create` row MINTED, read out of the server's own message.
 *
 * `api_keys.created_by` records which ACCOUNT minted a credential; the parent
 * KEY was never recorded anywhere, so "what has this credential created?" had no
 * answer. The server now appends ` key=<id>` to that row's detail, and this is
 * the single place the link is parsed. Rows from before it returns null — an
 * absent link, which the UI says out loud rather than presenting as "this key
 * minted nothing".
 */
export function mintedKey(r: AuditRow): string | null {
  if (r.tool !== 'apikey.create' || !r.ok) return null
  const m = / key=([0-9a-f]{16,32})/.exec(String(r.detail ?? ''))
  return m ? m[1] : null
}

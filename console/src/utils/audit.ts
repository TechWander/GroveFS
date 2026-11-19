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

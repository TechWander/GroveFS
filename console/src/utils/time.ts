import { t } from '@/i18n'

/*
  Reading a timestamp, in the two ways an operator actually wants one.

  `shortTime` keeps a table one line tall (MM-DD HH:MM:SS, the full RFC 3339
  stamp staying one hover away); `humanLeft` answers "is today the day I have to
  act" for a deadline. Negative and unparseable both answer in WORDS rather than
  a number, because both mean the credential is already refused.
*/

export function shortTime(ts: string | null | undefined): string {
  return typeof ts === 'string' && ts.length >= 19
    ? ts.slice(5, 19).replace('T', ' ')
    : String(ts ?? '')
}

export function humanLeft(iso: string): string {
  const ms = Date.parse(iso) - Date.now()
  if (!Number.isFinite(ms)) return t('unknown')
  if (ms <= 0) return t('already past')
  const ladder: [string, number][] = [
    ['d', 86_400_000],
    ['h', 3_600_000],
    ['m', 60_000],
  ]
  for (const [unit, size] of ladder) {
    if (ms >= size) return `${Math.floor(ms / size)} ${unit}`
  }
  return t('less than a minute')
}

/** Time windows are the FIRST filter for a log, and they are relative: an
    operator asks "what happened in the last hour", not "what happened between
    two timestamps I have to type". The absolute range a real audit export needs
    belongs to the CLI (`grovefs apikey calls`), which has the same rows and a
    machine to diff them in. */
export const WIN_MS: Record<string, number> = {
  '1h': 3_600_000,
  '24h': 86_400_000,
  '7d': 7 * 86_400_000,
}

export const WINDOWS: [string, string][] = [
  ['all', 'all time'],
  ['1h', 'last hour'],
  ['24h', 'last 24 hours'],
  ['7d', 'last 7 days'],
]

export function withinWindow(ts: string | undefined, win: string): boolean {
  if (!win || win === 'all') return true
  const t0 = Date.parse(String(ts ?? ''))
  const span = WIN_MS[win]
  return Number.isFinite(t0) && span !== undefined && Date.now() - t0 <= span
}

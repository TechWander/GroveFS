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

/** Time windows for the audit log live in `utils/audit.ts` — here only the
    FORMATTING helpers belong; a second copy of the ladder drifted once already
    in the hand-written file. */

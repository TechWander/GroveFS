import { t } from '@/i18n'

/** A cell that must stay one line tall even when the server's sentence is not.
    The whole text is one click away in the drawer, so nothing is lost — it is
    just not allowed to be six lines tall in a table of forty rows. */
export function clip(value: string | null | undefined, n: number): string {
  const s = value ?? ''
  return s.length > n ? `${s.slice(0, n)}…` : s
}

/** What the table will show for this secret from now on: the same head and
    tail the store computes server-side. Reproduced here ONLY in the one-time
    dialog, where the plaintext is already on the screen — it proves to the
    reader which fingerprint their row will carry. */
export function maskOf(full: string): string {
  const s = String(full)
  const secret = s.slice(s.indexOf(':') + 1)
  return `${secret.slice(0, 4)}…${secret.slice(-4)}`
}

/** The live result count every filter bar carries: "showing everything" and
    "the filter cut it down" are different facts and say different sentences. */
export function counted(shown: number, total: number): string {
  return shown === total
    ? t('${n} in total', { n: total })
    : t('${n} of ${m} shown', { n: shown, m: total })
}

/** The same limit as `apikeys::NOTE_MAX` — the server still refuses a longer
    note with a named limit, this only stops a paste from looking accepted. */
export const NOTE_MAX = 500

/** The ttl choices, in the CLI's own spelling (`--ttl 7d`). Weeks are absent
    because the parser has no `w` — offering `1w` here would be offering an
    error message. */
export const TTL_CHOICES: [string, string][] = [
  ['', 'never expires'],
  ['1h', '1 hour'],
  ['12h', '12 hours'],
  ['24h', '1 day'],
  ['7d', '7 days'],
  ['30d', '30 days'],
  ['90d', '90 days'],
  ['365d', '1 year'],
]

/** The `apikey.update` spelling for "do not touch the deadline". */
export const TTL_KEEP = '__keep'

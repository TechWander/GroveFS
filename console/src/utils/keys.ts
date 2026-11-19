import { t } from '@/i18n'
import type { KeyRow } from '@/types'
import { shortTime } from './time'

/*
  Where a credential stands, as ONE word.

  Four buckets, three colours: live (green), live-but-close-to-its-deadline
  (amber), refused (red). `expired` is the SERVER's verdict — the same instant
  comparison the handshake makes — so this page can never call a key live that
  its next mount will refuse, and a browser in another timezone cannot disagree
  with itself about an RFC 3339 string.
*/

export type KeyBucket = 'all' | 'live' | 'expiring' | 'expired' | 'revoked'

export const KEY_BUCKETS: KeyBucket[] = ['all', 'live', 'expiring', 'expired', 'revoked']

export function keyBucket(r: KeyRow): KeyBucket {
  if (r.revoked_at) return 'revoked'
  if (r.expired) return 'expired'
  if (r.expires_at && Date.parse(r.expires_at) - Date.now() < 7 * 86_400_000) return 'expiring'
  return 'live'
}

export type Tone = 'good' | 'warn' | 'bad'

export function keyTone(r: KeyRow): Tone {
  const b = keyBucket(r)
  if (b === 'revoked' || b === 'expired') return 'bad'
  if (b === 'expiring') return 'warn'
  return 'good'
}

export function keyLabel(r: KeyRow): string {
  const b = keyBucket(r)
  if (b === 'expired') return t('expired')
  if (b === 'revoked') return t('revoked')
  if (b === 'expiring') return t('expiring soon')
  return t('live')
}

/** The badge's tooltip: WHY the row is the colour it is, with the timestamp
    the server used. */
export function keyWhy(r: KeyRow): string {
  const b = keyBucket(r)
  if (b === 'revoked') return t('revoked ${at}', { at: String(r.revoked_at ?? '') })
  if (b === 'expired') return t('it expired at ${at}', { at: String(r.expires_at ?? '') })
  if (b === 'expiring') return t('it expires at ${at}', { at: String(r.expires_at ?? '') })
  return r.expires_at
    ? t('it expires at ${at}', { at: String(r.expires_at ?? '') })
    : t('no deadline')
}

/** The second line under the badge: the deadline itself, in words. */
export function expiryLine(r: KeyRow): string {
  if (r.revoked_at) return t('revoked ${at}', { at: shortTime(r.revoked_at) })
  if (!r.expires_at) return t('never expires')
  return r.expired
    ? t('expired ${when}', { when: shortTime(r.expires_at) })
    : t('expires ${when}', { when: shortTime(r.expires_at) })
}

export function bucketLabel(b: KeyBucket): string {
  if (b === 'all') return t('all')
  if (b === 'expired') return t('expired')
  if (b === 'revoked') return t('revoked')
  if (b === 'expiring') return t('expiring soon')
  return t('live')
}

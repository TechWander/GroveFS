import { request } from '@/api/http'
import type { Op, OpResult, Soft } from '@/api/ops'
import { useSessionStore } from '@/stores/session'
import { messageOf } from '@/utils/error'
import { t } from '@/i18n'

/*
  One governance call, in two flavours.

  `api()` THROWS with the server's sentence, which callers show verbatim: a
  paraphrased refusal is a refusal nobody can act on. `soft()` is the same call
  for the places where a refusal is one answer among several on the page — the
  overview loads half a dozen things at once, and the account list being an
  administrator's op should not blank the page, it should say so in that block.

  Both first check the gateway's routed-op list, so a page built against a
  different binary fails on a NAMED missing op instead of a mystery 403. That
  check is a courtesy to the reader, not a permission: the meta-server's
  `gates()` is the authority, and an un-routed op never reaches it anyway.
*/

export function useOps() {
  const session = useSessionStore()

  async function api<K extends Op>(
    op: K,
    args: Record<string, unknown> = {},
  ): Promise<OpResult[K]> {
    if (!session.can(op)) {
      throw new Error(`the console does not route '${op}' — it stays an operator CLI command`)
    }
    const { status, j } = await request<OpEnvelope<OpResult[K]>>('/api/call', {
      method: 'POST',
      headers: { 'content-type': 'application/json', 'x-grovefs-csrf': session.csrf ?? '' },
      body: JSON.stringify({ op, args }),
    })
    if (status === 401) {
      session.signedOut()
      throw new Error(t('signed out'))
    }
    if (!j) throw new Error(t('the gateway answered HTTP ${s} without a JSON body', { s: status }))
    if (j.ok === false) throw new Error(j.error || `HTTP ${status}`)
    return j.result as OpResult[K]
  }

  async function soft<K extends Op>(
    op: K,
    args: Record<string, unknown> = {},
  ): Promise<Soft<OpResult[K]>> {
    try {
      return { ok: true, r: (await api(op, args)) ?? null }
    } catch (e) {
      return { ok: false, error: messageOf(e) }
    }
  }

  return { api, soft }
}

/** `/api/call`'s envelope: the gateway's own `{ok,result}` / `{ok:false,error}`. */
interface OpEnvelope<T> {
  ok?: boolean
  result?: T
  error?: string
}

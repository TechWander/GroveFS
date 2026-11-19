import { computed, ref } from 'vue'
import { defineStore } from 'pinia'
import { request } from '@/api/http'
import type { Reply } from '@/api/http'
import type { Op } from '@/api/ops'
import { t } from '@/i18n'
import { messageOf } from '@/utils/error'
import type { SessionReply, Who } from '@/types'

/*
  The session, and the two facts the whole UI is built from: who is signed in,
  and which ops the gateway will route for them.

  Re-reading `/api/session` after a sign-in (instead of trusting the sign-in
  reply) is deliberate: `/api/session` asks the server `whoami`, so the role, the
  delegations and the op list the page is built from all come from ONE
  authenticated answer. And every check here is presentation only —
  `server_ext::gates()` decides what an account may do; hiding a button is a
  courtesy, and a refusal from the server is rendered verbatim because its
  wording is the answer to the user's actual question.
*/

export type SessionStatus = 'booting' | 'anonymous' | 'ready'

export const useSessionStore = defineStore('session', () => {
  const status = ref<SessionStatus>('booting')
  const csrf = ref<string | null>(null)
  const who = ref<Who | null>(null)
  /** The gateway's routed-op whitelist, as `/api/session` returned it. */
  const ops = ref<string[]>([])
  /** The line the sign-in card shows: a server refusal, or the session ending. */
  const notice = ref('')
  const busy = ref(false)

  const signedIn = computed(() => who.value !== null)
  const isSuper = computed(() => !!who.value?.super)
  const account = computed(() => who.value?.account ?? '')
  const role = computed(() => who.value?.role ?? '')
  const bases = computed(() => who.value?.bases ?? [])

  const can = (op: Op): boolean => ops.value.includes(op)

  function drop() {
    csrf.value = null
    who.value = null
    ops.value = []
    status.value = 'anonymous'
  }

  /** A 401 from anywhere lands here: the cookie is gone, so say so on the card
      the user is already looking at rather than on a page that cannot load. */
  function signedOut() {
    drop()
    notice.value = t('the session ended — sign in again')
  }

  async function boot(): Promise<void> {
    let reply: Reply<SessionReply>
    try {
      reply = await request<SessionReply>('/api/session')
    } catch (e) {
      // The gateway is unreachable (it restarted, a proxy is down, the laptop
      // slept). The old file let this reject and left a blank page on screen —
      // the one failure mode a management console should never have.
      drop()
      notice.value = t('the console could not read its own session: ${e}', { e: messageOf(e) })
      return
    }
    const { status: code, j } = reply
    if (code === 200 && j?.signedIn && j.who) {
      csrf.value = j.csrf ?? null
      who.value = j.who
      ops.value = j.ops ?? []
      notice.value = ''
      status.value = 'ready'
      return
    }
    drop()
    if (j?.error) notice.value = j.error
  }

  /** Returns whether the sign-in took, so the card can clear its own password
      field only when it worked. */
  async function signIn(account: string, password: string): Promise<boolean> {
    busy.value = true
    notice.value = ''
    try {
      const { status: code, j } = await request<{ ok?: boolean; error?: string }>('/api/login', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ account, password }),
      })
      if (code === 200 && j?.ok) {
        await boot()
        return true
      }
      notice.value = j?.error || t('sign-in failed (HTTP ${s})', { s: code })
      return false
    } catch (e) {
      // A refused connection is not a wrong password, and the card must not say
      // it is: this is the line that decides whether someone keeps typing.
      notice.value = t('the console could not reach the gateway: ${e}', { e: messageOf(e) })
      return false
    } finally {
      busy.value = false
    }
  }

  async function signOut(): Promise<void> {
    try {
      await request('/api/logout', {
        method: 'POST',
        headers: { 'x-grovefs-csrf': csrf.value ?? '' },
      })
    } catch {
      /* the session is gone either way */
    }
    drop()
  }

  return {
    status,
    csrf,
    who,
    ops,
    notice,
    busy,
    signedIn,
    isSuper,
    account,
    role,
    bases,
    can,
    boot,
    signIn,
    signOut,
    signedOut,
  }
})

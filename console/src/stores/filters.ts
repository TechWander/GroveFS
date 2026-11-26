import { reactive, ref } from 'vue'
import { defineStore } from 'pinia'

/*
  The filter state of the four list pages, kept in one place each.

  A page that forgets what you typed the moment a button is pressed is a page you
  have to re-search after every action, so these survive a reload and a page
  switch. They are cleared at sign-in, because a stale search box carried onto a
  different account hides rows you did not look for. `auditLimit` lives here for
  the same reason the old file kept it on `ST`: "refresh" must not shrink the
  window you chose — and neither must it reset the page you were reading.
*/

export interface KindFilter {
  q: string
  kind: string
  /** A metadata instance name, or `all`. Set by a Storage card / deep link. */
  instance: string
}

export interface KeyFilter {
  q: string
  state: string
  scope: string
  view: string
}

export interface AccountFilter {
  q: string
  role: string
  state: string
  /** Only accounts delegated this base view (`all` = everyone). */
  view: string
}

export interface AuditFilter {
  q: string
  window: string
  /** Inclusive `YYYY-MM-DD` bounds, read only when `window === 'custom'`.
      Empty string = open end. */
  from: string
  to: string
  plane: string
  op: string
  verdict: string
  /** Which CREDENTIAL signed the calls being shown — the door, not the person:
      a console click arrives on the gateway's own key and names its account in
      the message. `all` means every key. */
  key: string
  /** Which VIEW the calls are ABOUT. The audit row has no view column — the
      name lives in the server's message — so this matches rows whose detail
      mentions the name, and "all" shows every row. */
  view: string
}

const fresh = {
  views: (): KindFilter => ({ q: '', kind: 'all', instance: 'all' }),
  keys: (): KeyFilter => ({ q: '', state: 'all', scope: 'all', view: 'all' }),
  accounts: (): AccountFilter => ({ q: '', role: 'all', state: 'all', view: 'all' }),
  audit: (): AuditFilter => ({
    q: '',
    window: 'all',
    from: '',
    to: '',
    plane: 'all',
    op: 'all',
    verdict: 'all',
    key: 'all',
    view: 'all',
  }),
}

export const useFilterStore = defineStore('filters', () => {
  const vFilter = reactive<KindFilter>(fresh.views())
  const keyFilter = reactive<KeyFilter>(fresh.keys())
  const accFilter = reactive<AccountFilter>(fresh.accounts())
  const auditFilter = reactive<AuditFilter>(fresh.audit())
  const auditLimit = ref(50)
  /* Paging the loaded rows. `auditLimit` is how many rows the server was asked
     for; these two decide how many of the MATCHING ones are on screen at once,
     so a 1000-row load stays a scrollable page instead of a wall. The page
     resets on any filter change (see AuditPage) — landing on page 7 of a
     result set you just narrowed is never the set you meant. */
  const auditPage = ref(1)
  const auditPageSize = ref(20)

  function reset() {
    Object.assign(vFilter, fresh.views())
    Object.assign(keyFilter, fresh.keys())
    Object.assign(accFilter, fresh.accounts())
    Object.assign(auditFilter, fresh.audit())
    auditLimit.value = 50
    auditPage.value = 1
    auditPageSize.value = 20
  }

  return {
    vFilter,
    keyFilter,
    accFilter,
    auditFilter,
    auditLimit,
    auditPage,
    auditPageSize,
    reset,
  }
})

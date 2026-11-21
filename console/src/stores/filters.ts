import { reactive, ref } from 'vue'
import { defineStore } from 'pinia'

/*
  The filter state of the four list pages, kept in one place each.

  A page that forgets what you typed the moment a button is pressed is a page you
  have to re-search after every action, so these survive a reload and a page
  switch. They are cleared at sign-in, because a stale search box carried onto a
  different account hides rows you did not look for. `auditLimit` lives here for
  the same reason the old file kept it on `ST`: "refresh" must not shrink the
  window you chose.
*/

export interface KindFilter {
  q: string
  kind: string
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
}

export interface AuditFilter {
  q: string
  window: string
  plane: string
  op: string
  verdict: string
  /** Which CREDENTIAL signed the calls being shown — the door, not the person:
      a console click arrives on the gateway's own key and names its account in
      the message. `all` means every key. */
  key: string
}

const fresh = {
  views: (): KindFilter => ({ q: '', kind: 'all' }),
  keys: (): KeyFilter => ({ q: '', state: 'all', scope: 'all', view: 'all' }),
  accounts: (): AccountFilter => ({ q: '', role: 'all', state: 'all' }),
  audit: (): AuditFilter => ({
    q: '',
    window: 'all',
    plane: 'all',
    op: 'all',
    verdict: 'all',
    key: 'all',
  }),
}

export const useFilterStore = defineStore('filters', () => {
  const vFilter = reactive<KindFilter>(fresh.views())
  const keyFilter = reactive<KeyFilter>(fresh.keys())
  const accFilter = reactive<AccountFilter>(fresh.accounts())
  const auditFilter = reactive<AuditFilter>(fresh.audit())
  const auditLimit = ref(50)

  function reset() {
    Object.assign(vFilter, fresh.views())
    Object.assign(keyFilter, fresh.keys())
    Object.assign(accFilter, fresh.accounts())
    Object.assign(auditFilter, fresh.audit())
    auditLimit.value = 50
  }

  return { vFilter, keyFilter, accFilter, auditFilter, auditLimit, reset }
})

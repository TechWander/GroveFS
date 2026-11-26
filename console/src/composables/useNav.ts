import { useCatalogStore } from '@/stores/catalog'
import { useFilterStore } from '@/stores/filters'
import { useUiStore } from '@/stores/ui'
import { router } from '@/router'

/*
  The jumps the console makes between pages.

  The hand-written file had one global `go(page)` and poked `ST.sel` / `ST.keyFilter`
  by hand at every call site — which is how "open this base view" could set the
  selection on one page and forget it on another. Here a jump carries BOTH facts
  it is about: what to select, and where to go.

  Every related entity shown as text is a candidate for one of these jumps: a
  key's view, a key's owner, a call's key or account, a view's keys and its
  audit trail. The jumps never invent a second list page — they set the filter
  the destination already offers and walk there, so the verbs, the search box
  and the chips stay in ONE place per entity.

  `catalog.sel` is deliberately not cleared by navigation: the views page's
  picker remembers what you last opened, so a pill clicked from the accounts page
  lands on a detail pane that already shows the record you asked for.
*/
export function useNav() {
  const catalog = useCatalogStore()
  const filters = useFilterStore()
  const ui = useUiStore()

  const to = (page: string): Promise<void> => router.push('/' + page).then(() => undefined)

  /** A jump that carries a FILTER with it: the landing page reads the query
      once (see `consumeQuery`), applies it to its filter store and then strips
      it, so a refresh never re-pins a filter the operator has since cleared. */
  const goTo = (page: string, query: Record<string, string> = {}): Promise<void> =>
    router.push({ path: '/' + page, query }).then(() => undefined)

  /** Read the query keys this console knows, apply them, and strip them from
      the URL. Called by the three list pages at setup time. */
  const consumeQuery = (): void => {
    const route = router.currentRoute.value
    const q = route.query
    let touched = false
    const take = (key: string): string | null => {
      const v = q[key]
      if (typeof v !== 'string' || !v) return null
      touched = true
      return v
    }
    const view = take('view')
    const instance = take('instance')
    const owner = take('owner')
    const search = take('q')
    if (view) {
      catalog.sel = view
      filters.keyFilter.view = view
      filters.accFilter.view = view
    }
    if (instance) filters.vFilter.instance = instance
    if (owner) filters.keyFilter.q = owner
    if (search) {
      // `q` means the search box of whichever list landed here.
      if (route.name === 'accounts') filters.accFilter.q = search
      else if (route.name === 'keys') filters.keyFilter.q = search
    }
    if (touched) void router.replace({ path: route.path, query: {} })
  }

  /** Choose a view INSIDE the views page: the picker's stripe and the detail
      pane both follow `catalog.sel`, and a record pane left open on the row you
      just switched away from is a lie — so it closes. This is the old `pickView`. */
  const pickView = (name: string): void => {
    catalog.sel = name
    ui.closeDrawer()
  }

  /** A base view's name is a link everywhere it appears: the next question is
      always what that name looks like. */
  const openView = (name: string): Promise<void> => {
    catalog.sel = name
    filters.vFilter.instance = 'all'
    return to('views')
  }

  /** From a view (or a storage card) to the keys bound to it. */
  const openKeysOfView = (view: string): Promise<void> => {
    filters.keyFilter.view = view
    filters.keyFilter.q = ''
    return goTo('keys', { view })
  }

  /** From a view to the accounts delegated it. */
  const openAccountsOfView = (view: string): Promise<void> => {
    filters.accFilter.view = view
    filters.accFilter.q = ''
    return goTo('accounts', { view })
  }

  /** From a key's owner to that account (the accounts page matches by name). */
  const openAccountsOf = (account: string): Promise<void> => {
    filters.accFilter.q = account
    filters.accFilter.view = 'all'
    return to('accounts')
  }

  /** The views that live on one metadata instance. */
  const openInstance = (instance: string): Promise<void> => {
    filters.vFilter.instance = instance
    filters.vFilter.q = ''
    return goTo('views', { instance })
  }

  /** An account's keys are not a separate list — they are the keys page with
      that account in the search box, so the filters and verbs stay in one place. */
  const openKeysOf = (account: string): Promise<void> => {
    filters.keyFilter.q = account
    filters.keyFilter.state = 'all'
    return to('keys')
  }

  /** A key's audit trail is not a separate list either — it is the audit page
      with that credential already picked in the key dropdown. */
  const openAuditOfKey = (id: string): Promise<void> => {
    filters.auditFilter.key = id
    return to('audit')
  }

  /** The calls that mention one view, from the view's own page: the audit row
      carries no view column (the name is in the server's message), so this is
      the search box pointed at that name, opened as the view-filter chip. */
  const openAuditOfView = (name: string): Promise<void> => {
    filters.auditFilter.view = name
    return to('audit')
  }

  /** An account mentioned anywhere — a key's owner, a call's actor — is the
      accounts page searched to that name. */
  const openAccount = (name: string): Promise<void> => {
    filters.accFilter.q = name
    filters.accFilter.role = 'all'
    filters.accFilter.state = 'all'
    return to('accounts')
  }

  return {
    to,
    goTo,
    consumeQuery,
    pickView,
    openView,
    openKeysOf,
    openKeysOfView,
    openAccountsOf,
    openAccountsOfView,
    openInstance,
    openAuditOfKey,
    openAuditOfView,
    openAccount,
  }
}

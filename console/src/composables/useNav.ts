import { useCatalogStore } from '@/stores/catalog'
import { useFilterStore } from '@/stores/filters'
import { useUiStore } from '@/stores/ui'
import { router } from '@/router'

/*
  The three jumps the console makes between pages.

  The hand-written file had one global `go(page)` and poked `ST.sel` / `ST.keyFilter`
  by hand at every call site — which is how "open this base view" could set the
  selection on one page and forget it on another. Here a jump carries BOTH facts
  it is about: what to select, and where to go.

  `catalog.sel` is deliberately not cleared by navigation: the views page's
  picker remembers what you last opened, so a pill clicked from the accounts page
  lands on a detail pane that already shows the record you asked for.
*/
export function useNav() {
  const catalog = useCatalogStore()
  const filters = useFilterStore()
  const ui = useUiStore()

  const to = (page: string): Promise<void> => router.push('/' + page).then(() => undefined)

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
    return to('views')
  }

  /** An account's keys are not a separate list — they are the keys page with
      that account in the search box, so the filters and verbs stay in one place. */
  const openKeysOf = (account: string): Promise<void> => {
    filters.keyFilter.q = account
    filters.keyFilter.state = 'all'
    return to('keys')
  }

  return { to, pickView, openView, openKeysOf }
}

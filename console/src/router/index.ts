import { createRouter, createWebHashHistory } from 'vue-router'
import type { Component } from 'vue'
import OverviewPage from '@/pages/OverviewPage.vue'
import ViewsPage from '@/pages/ViewsPage.vue'
import KeysPage from '@/pages/KeysPage.vue'
import AccountsPage from '@/pages/AccountsPage.vue'
import AuditPage from '@/pages/AuditPage.vue'

/*
  Five destinations, declared ONCE.

  In the hand-written console the same list existed three times: five `<button
  data-page>` in index.html, a `{overview: pageOverview, …}` switch in app.js,
  and a `.page` section per name. Here one table feeds the router, the nav and
  the page header, so a destination cannot be listed in one place and missing in
  another.

  The history mode is HASH on purpose: the gateway serves `/`, `/app.css`,
  `/app.js` and `/favicon.svg` and nothing else (`web.rs` answers every other
  path with "no console page"), so a real `/keys` URL would 404 on reload or on
  someone pasting a link. `#/keys` survives both, is shareable, and needs no
  server change.
*/

export interface PageMeta {
  /** The route name, and the key the old `data-page` attribute used. */
  name: string
  path: string
  /** Dictionary key of the nav label (English text doubles as the key). */
  label: string
  /** Which inline `<svg>` the nav button draws — see `NavIcon.vue`. */
  icon: 'overview' | 'views' | 'keys' | 'accounts' | 'audit'
  component: Component
}

export const PAGES: PageMeta[] = [
  {
    name: 'overview',
    path: '/overview',
    label: 'Overview',
    icon: 'overview',
    component: OverviewPage,
  },
  { name: 'views', path: '/views', label: 'Views', icon: 'views', component: ViewsPage },
  { name: 'keys', path: '/keys', label: 'Keys', icon: 'keys', component: KeysPage },
  {
    name: 'accounts',
    path: '/accounts',
    label: 'Accounts',
    icon: 'accounts',
    component: AccountsPage,
  },
  { name: 'audit', path: '/audit', label: 'Audit', icon: 'audit', component: AuditPage },
]

export const routes = [
  { path: '/', redirect: '/overview' },
  ...PAGES.map((p) => ({ name: p.name, path: p.path, component: p.component })),
  // The old console had no deep links at all, so an unknown hash is not a page a
  // user can be sent to: it is the overview, which is what `#` meant before.
  { path: '/:pathMatch(.*)*', name: 'not-found', redirect: '/overview' },
]

/** One instance, imported by `main.ts` AND by anything that has to navigate
    from outside a component (a pill in a drawer jumping to another page). That
    is what the old file did with a global `go(page)`. */
export const router = createRouter({
  history: createWebHashHistory(),
  routes,
})

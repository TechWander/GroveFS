<script setup lang="ts">
/*
  The one drawer, showing whichever record is open.

  The ui store holds a discriminated entry (`kind` + its row), and this component
  narrows it once per kind in a computed — which is what makes `row` reach a
  child with its real type instead of as a union. Nothing else in the app decides
  which pane is up: a row clicked on the overview, on its own page or in another
  pane's table all land here, so the record always reads the same.

  It lives in the shell rather than in a page for the same reason: an audit row
  opened from the overview should still be open while you look at it, and closing
  it is one action (`ui.closeDrawer()`), wherever you were.
*/
import { computed } from 'vue'
import { useUiStore } from '@/stores/ui'
import AuditDrawer from './AuditDrawer.vue'
import KeyDrawer from './KeyDrawer.vue'
import AccountDrawer from './AccountDrawer.vue'

const ui = useUiStore()

/** Narrowed once per kind, so each pane receives its own row type: the store
    keeps a discriminated entry, and `kind` is what decides the shape of `row`. */
const audit = computed(() => (ui.drawer?.kind === 'audit' ? ui.drawer.row : null))
const key = computed(() => (ui.drawer?.kind === 'key' ? ui.drawer.row : null))
const account = computed(() =>
  ui.drawer?.kind === 'account'
    ? {
        row: ui.drawer.row,
        grants: ui.drawer.grants,
        keys: ui.drawer.keys,
        isSelf: ui.drawer.isSelf,
      }
    : null,
)

function close(): void {
  ui.closeDrawer()
}
</script>

<template>
  <AuditDrawer v-if="audit" :row="audit" @close="close" />
  <KeyDrawer v-else-if="key" :row="key" @close="close" />
  <AccountDrawer
    v-else-if="account"
    :row="account.row"
    :grants="account.grants"
    :keys="account.keys"
    :is-self="account.isSelf"
    @close="close"
  />
</template>

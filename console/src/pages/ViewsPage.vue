<script setup lang="ts">
/*
  The namespace page: a picker on the left, one name read completely on the
  right.

  The page itself owns almost nothing. It loads the namespace (both halves of it,
  `physical.list` and `def.list`, merged into one list of names) and then hands
  the two panes their work — which is why switching a view does not refetch the
  list, and why the accounts page's "open this base view" can land here with a
  selection already made.

  `loadViews(true)` forces the read on every entry: a base view created in
  another tab is a thing people expect to see immediately, and the namespace is
  two cheap calls, not a scan.
*/
import { useCatalogStore } from '@/stores/catalog'
import { usePageLoad } from '@/composables/usePageLoad'
import { useNav } from '@/composables/useNav'
import { useUiStore } from '@/stores/ui'
import PageHead from '@/components/layout/PageHead.vue'
import ActionButton from '@/components/ui/ActionButton.vue'
import LoadingHint from '@/components/ui/LoadingHint.vue'
import ViewList from '@/components/views/ViewList.vue'
import ViewDetail from '@/components/views/ViewDetail.vue'

const catalog = useCatalogStore()
const ui = useUiStore()
const { consumeQuery } = useNav()

/* Apply `?instance=` / `?view=` before the first render: a card clicked on the
   Storage page lands here with the filter already set, not one frame later. */
consumeQuery()

function refresh(): void {
  ui.reload()
}

/* `loadViews` keeps its own error (three pages share the list, and a refusal in
   one must not blank the other two), so the page reads the failure from the
   store rather than from this hook. `loadStorage` is soft: a delegated account
   cannot read `store.info`, and the list simply has no instance column for it. */
const { pending } = usePageLoad(async () => {
  await Promise.all([catalog.loadViews(true), catalog.loadStorage(true)])
})
</script>

<template>
  <PageHead
    kicker="Namespace"
    title="Views"
    desc="Base views, forks, aliases and lenses — select one to inspect."
  >
    <template #actions>
      <ActionButton :action="refresh" :label="'refresh'" variant="link" />
    </template>
  </PageHead>

  <LoadingHint v-if="pending && !catalog.views.length" />
  <p v-else-if="catalog.error" class="err">{{ catalog.error }}</p>
  <div v-else class="split">
    <ViewList />
    <ViewDetail />
  </div>
</template>

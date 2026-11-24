<script setup lang="ts">
/*
  Create a base view — the one act on this page that chooses where bytes live.

  The instance dropdown lists the control plane's own registry (via
  `store.info`), never a copy typed into this file: it is the same expanded
  book the startup banner printed and the book `place_new_view` balances
  against. Empty selection means "auto" — the least-loaded placement the CLI
  gives without `--pg-instance`; one entry means there is no decision to make
  and the row hides itself instead of asking the operator to confirm the
  obvious.

  When the signed-in account cannot create one, the panel does not offer a button
  guaranteed to be refused — it says whose act it is instead. That is the single
  place in the console where a capability is hidden rather than refused, because
  here the refusal would arrive on every page load, not on one verb.
*/
import { computed, onMounted, ref } from 'vue'
import { useCatalogStore } from '@/stores/catalog'
import { useOps } from '@/composables/useOps'
import { useSessionStore } from '@/stores/session'
import { useUiStore } from '@/stores/ui'
import { t } from '@/i18n'
import type { PgLedgerRow } from '@/types'
import PanelBox from '@/components/layout/PanelBox.vue'
import FieldRow from '@/components/layout/FieldRow.vue'
import ActionButton from '@/components/ui/ActionButton.vue'

const catalog = useCatalogStore()
const session = useSessionStore()
const ui = useUiStore()
const { api, soft } = useOps()

const name = ref('')
const instance = ref('')
const instances = ref<PgLedgerRow[]>([])

onMounted(async () => {
  // A failing or ACL-refused store.info leaves the list empty, which hides
  // the row: creating with auto placement stays possible either way.
  const r = await soft('store.info', {})
  const list = r.ok ? r.r?.pgInstances?.instances : undefined
  if (Array.isArray(list)) instances.value = list
})

const selectable = computed(() => instances.value.length > 1)

// The auto option IS least-loaded placement, so say which instance that
// currently means — the same fewest-views-then-registry-order rule the
// server applies, read off the same ledger. Counts missing (control plane
// was unreachable at snapshot time) falls back to the plain wording.
const autoLabel = computed(() => {
  const known = instances.value.filter((i) => i.views !== null)
  if (known.length === 0 || !selectable.value) return t('auto — least loaded')
  const min = Math.min(...known.map((i) => i.views as number))
  const winner = known.find((i) => (i.views as number) === min)
  return winner
    ? t('auto — currently ${n} (${v} view(s))', { n: winner.name, v: winner.views })
    : t('auto — least loaded')
})

async function create(): Promise<void> {
  const view = name.value.trim()
  const args: Record<string, unknown> = { view }
  if (selectable.value && instance.value) args.pg_instance = instance.value
  const r = await api('view.create', args)
  // A created view may have taken a name that already had a definition, and the
  // server's own note about that IS the answer — shown, not swallowed.
  ui.toast(typeof r === 'string' ? r : t('view created'))
  catalog.sel = view
  ui.reload()
}
</script>

<template>
  <div v-if="!session.can('view.create')" id="view-new" class="panel">
    <p class="muted">
      {{ t('creating a BASE view is an administrator’s act — it chooses where bytes live.') }}
    </p>
  </div>

  <PanelBox v-else id="view-new" :title="'New base view'">
    <FieldRow for="new-base-name" label="name">
      <input id="new-base-name" v-model="name" placeholder="project-x" />
    </FieldRow>
    <FieldRow v-if="selectable" for="new-base-pg" label="PG instance">
      <select id="new-base-pg" v-model="instance">
        <option value="">{{ autoLabel }}</option>
        <option v-for="i in instances" :key="i.name" :value="i.name">
          {{ i.views === null ? i.name : `${i.name} — ${i.views} view(s)` }}
        </option>
      </select>
      <template #hint>
        {{ t('the new view’s tree is created on this instance; later writes always follow it') }}
      </template>
    </FieldRow>
    <p class="muted">
      {{ t('a base view owns physical deletion; lenses, aliases and forks reshape one') }}
    </p>
    <div class="panel-foot">
      <ActionButton :action="create" :label="'Create'" variant="primary" />
    </div>
  </PanelBox>
</template>

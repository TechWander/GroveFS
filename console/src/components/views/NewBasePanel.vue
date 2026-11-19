<script setup lang="ts">
/*
  Create a base view — the one act on this page that chooses where bytes live.

  `PG instance` is optional and empty means "pick one", which is why the field's
  placeholder says so rather than leaving the box unexplained: placement is the
  decision this form is really asking for, and on a deployment with one instance
  it is a decision nobody wants to make twice.

  When the signed-in account cannot create one, the panel does not offer a button
  guaranteed to be refused — it says whose act it is instead. That is the single
  place in the console where a capability is hidden rather than refused, because
  here the refusal would arrive on every page load, not on one verb.
*/
import { ref } from 'vue'
import { useCatalogStore } from '@/stores/catalog'
import { useOps } from '@/composables/useOps'
import { useSessionStore } from '@/stores/session'
import { useUiStore } from '@/stores/ui'
import { t } from '@/i18n'
import PanelBox from '@/components/layout/PanelBox.vue'
import FieldRow from '@/components/layout/FieldRow.vue'
import ActionButton from '@/components/ui/ActionButton.vue'

const catalog = useCatalogStore()
const session = useSessionStore()
const ui = useUiStore()
const { api } = useOps()

const name = ref('')
const instance = ref('')

async function create(): Promise<void> {
  const view = name.value.trim()
  const args: Record<string, unknown> = { view }
  if (instance.value.trim()) args.pg_instance = instance.value.trim()
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
    <FieldRow for="new-base-pg" label="PG instance">
      <input id="new-base-pg" v-model="instance" placeholder="(least loaded)" />
    </FieldRow>
    <ActionButton :action="create" :label="'Create'" variant="primary" />
    <p class="muted">
      {{ t('a base view owns physical deletion; lenses, aliases and forks reshape one') }}
    </p>
  </PanelBox>
</template>

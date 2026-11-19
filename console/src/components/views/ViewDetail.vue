<script setup lang="ts">
/*
  The right-hand pane of the views page: one name, read completely.

  Two blocks sit side by side on purpose — what this name IS, and what hangs off
  it. The first draft of the console put the derived list in the same flat
  definition list as the stamp, where it read as one more string instead of the
  reason to be careful before deleting anything.

  Anything the ACL can refuse is OFFERED and let the server refuse: a hidden
  button teaches nothing about why, and the refusal names the delegation that
  would change it. Two verbs here are two-step, ordered so that the intermediate
  state is the harmless one — dropping a fork deletes its TREE first and its
  definition second, because reversing that would strand a tree whose name no
  longer resolves to a base, and a delegated owner would lose the rights that
  justify the second call before it is made.
*/
import { computed, onMounted, ref, watch } from 'vue'
import type { KeyRow, ViewStats } from '@/types'
import { exportUrl } from '@/api/http'
import { useOps } from '@/composables/useOps'
import { useCatalogStore } from '@/stores/catalog'
import { useSessionStore } from '@/stores/session'
import { useUiStore } from '@/stores/ui'
import { messageOf } from '@/utils/error'
import { kindName, stampText } from '@/utils/views'
import { t } from '@/i18n'
import KvList from '@/components/layout/KvList.vue'
import KvRow from '@/components/layout/KvRow.vue'
import BarRow from '@/components/layout/BarRow.vue'
import StateBadge from '@/components/ui/StateBadge.vue'
import CountPill from '@/components/ui/CountPill.vue'
import ActionButton from '@/components/ui/ActionButton.vue'
import LoadingHint from '@/components/ui/LoadingHint.vue'
import KeyTable from '@/components/keys/KeyTable.vue'
import LineagePanel from './LineagePanel.vue'
import ImportPanel from './ImportPanel.vue'
import DefEditPanel from './DefEditPanel.vue'
import ForkDialog from '@/components/dialogs/ForkDialog.vue'
import DefineDialog from '@/components/dialogs/DefineDialog.vue'

const catalog = useCatalogStore()
const session = useSessionStore()
const ui = useUiStore()
const { api, soft } = useOps()

const stats = ref<ViewStats | null>(null)
const pending = ref(false)
const error = ref('')
const mine = ref<KeyRow[]>([])
const keysError = ref('')
const forkOpen = ref(false)
const defineOpen = ref(false)

/** What this name's tree IS, in one of three answers. A fork has its own tree
    and a base's definition; a lens has neither. */
const treeText = computed(() => {
  const s = stats.value
  if (!s) return ''
  return s.tree ? t('physical tree') : s.forkTree ? t('fork tree') : t('none of its own')
})

async function load(): Promise<void> {
  const name = catalog.sel
  if (!name) {
    stats.value = null
    return
  }
  pending.value = true
  error.value = ''
  try {
    const s = await api('view.stats', { view: name })
    stats.value = s
    // A view's own key list is a second question, and a refusal of THIS one must
    // not blank the record that was just answered: it reports into its own line.
    mine.value = []
    keysError.value = ''
    if (session.can('apikey.create')) {
      const res = await soft('apikey.list', {})
      if (res.ok) mine.value = (res.r ?? []).filter((k) => k.view === s.name)
      else keysError.value = res.error
    }
  } catch (e) {
    stats.value = null
    error.value = messageOf(e)
  } finally {
    pending.value = false
  }
}

onMounted(() => void load())
watch(
  () => catalog.sel,
  () => void load(),
)
watch(
  () => ui.nonce,
  () => void load(),
)

/** Ask again, quietly: `@saved` from the definition editor changes this view's
    text, not the namespace, so only this pane is refetched. */
function refetch(): void {
  void load()
}

async function dropFork(): Promise<void> {
  const s = stats.value
  if (!s) return
  if (
    !(await ui.ask(
      'Drop this fork?',
      '${name}: its own tree is deleted. Its base ${b} and every shared object stay.',
      { name: s.name, b: s.base || '' },
      'Drop fork',
    ))
  )
    return
  await api('fork.drop', { name: s.name })
  try {
    await api('def.remove', { name: s.name })
  } catch (e) {
    // Half-done is the truth here, and it says what is left to finish.
    ui.toast(
      t('the fork tree is gone, but its definition could not be removed: ${e}', {
        e: messageOf(e),
      }),
      'bad',
    )
  }
  catalog.sel = null
  ui.reload()
}

async function removeDef(): Promise<void> {
  const s = stats.value
  if (!s) return
  if (
    !(await ui.ask(
      'Remove this definition?',
      '${name} =\n${def}\n\nOnly the name goes away — no tree and no object is deleted.',
      { name: s.name, def: s.definition || '' },
      'Remove definition',
    ))
  )
    return
  await api('def.remove', { name: s.name })
  ui.toast(t('definition removed'))
  ui.reload()
}

function doExport(): void {
  const s = stats.value
  if (!s) return
  // A download, not a fetch: the gateway answers it as a file.
  window.location.assign(exportUrl(s.name, session.csrf))
}
</script>

<template>
  <div id="view-detail" class="col-wide">
    <p v-if="!catalog.sel" class="muted">{{ t('pick a view on the left') }}</p>

    <LoadingHint v-else-if="pending" />

    <p v-else-if="error" class="err">{{ error }}</p>

    <div v-else-if="stats" class="detail">
      <div class="head">
        <h2 class="mono">{{ stats.name }}</h2>
        <StateBadge :label="kindName(stats.kind)" :kind="stats.kind" />
        <StateBadge
          v-if="(stats.sessions ?? 0) > 0"
          tone="good"
          :label="t('${n} mount session(s)', { n: stats.sessions })"
        />
        <CountPill
          v-if="stats.kind === 'base' && (stats.family ?? []).length"
          :text="t('${n} derived', { n: (stats.family ?? []).length })"
          :title="t('names that resolve through this base')"
        />
      </div>

      <div class="grid2">
        <div>
          <h3 class="det-h">{{ t('This name') }}</h3>
          <KvList>
            <KvRow label="base" :value="stats.base" mono />
            <KvRow label="stamp" :value="stampText(stats.stamp)" mono />
            <KvRow label="tree" :value="treeText" />
            <KvRow
              label="on this server"
              :value="stats.resident ? t('open') : t('not open')"
              muted
            />
            <KvRow
              label="keys"
              :value="
                stats.keys === null || stats.keys === undefined
                  ? null
                  : t('${n} bound to this name', { n: stats.keys })
              "
            />
          </KvList>
        </div>
        <div>
          <h3 class="det-h">{{ t('Lineage') }}</h3>
          <LineagePanel :stats="stats" />
        </div>
      </div>

      <template v-if="stats.definition">
        <h3 class="det-h">{{ t('Definition') }}</h3>
        <pre>{{ stats.definition }}</pre>
      </template>

      <div class="row">
        <button type="button" @click="doExport">{{ t('Export dump') }}</button>
        <button v-if="session.can('fork.create')" type="button" @click="forkOpen = true">
          {{ t('Fork…') }}
        </button>
        <button v-if="session.can('def.put')" type="button" @click="defineOpen = true">
          {{ t('Lens / alias…') }}
        </button>
        <ActionButton
          v-if="stats.kind === 'fork' && session.can('fork.drop')"
          :action="dropFork"
          :label="'Drop fork'"
          variant="danger"
        />
        <ActionButton
          v-if="stats.kind !== 'base' && session.can('def.remove')"
          :action="removeDef"
          :label="'Remove definition'"
          variant="danger"
        />
      </div>

      <ImportPanel :stats="stats" />

      <template v-if="session.can('apikey.create')">
        <BarRow :title="'Keys bound to ${name}'" :level="3" :vars="{ name: stats.name }" />
        <p v-if="keysError" class="err">{{ keysError }}</p>
        <KeyTable
          v-else
          :rows="mine"
          :scoped-to-view="true"
          :empty="'no keys bound to this view yet'"
        />
      </template>

      <DefEditPanel
        v-if="session.can('def.put') && stats.kind !== 'base'"
        :key="stats.name"
        :stats="stats"
        @saved="refetch"
      />

      <ForkDialog v-if="forkOpen" :stats="stats" @close="forkOpen = false" />
      <DefineDialog v-if="defineOpen" :stats="stats" @close="defineOpen = false" />
    </div>
  </div>
</template>

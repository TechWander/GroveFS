<script setup lang="ts">
/*
  Upload a dump. A delegated account is pinned to `as_fork` of a base it
  governs: importing WITHOUT that argument creates a base view, which the server
  refuses for anyone but an administrator — the UI says so up front instead of
  letting the refusal arrive after a 40 MB upload.

  The checkbox that changes the destination is therefore only shown to someone
  who may use it, and the footnote under the row states which of the two shapes
  this upload will take. Both are the server's rules, read from the session it
  already handed out — nothing here decides anything.
*/
import { ref } from 'vue'
import type { ViewStats } from '@/types'
import { importDump } from '@/api/http'
import { useCatalogStore } from '@/stores/catalog'
import { useSessionStore } from '@/stores/session'
import { useUiStore } from '@/stores/ui'
import { t } from '@/i18n'
import BarRow from '@/components/layout/BarRow.vue'
import FieldRow from '@/components/layout/FieldRow.vue'
import ActionButton from '@/components/ui/ActionButton.vue'

const props = defineProps<{ stats: ViewStats }>()

const catalog = useCatalogStore()
const session = useSessionStore()
const ui = useUiStore()

const file = ref<File | null>(null)
const target = ref('')
const replace = ref(false)
const asBase = ref(false)

function picked(ev: Event): void {
  file.value = (ev.target as HTMLInputElement).files?.[0] ?? null
}

async function run(): Promise<void> {
  const f = file.value
  if (!f) throw new Error(t('choose a dump file first'))
  const view = target.value.trim()
  const { status, j } = await importDump({
    file: f,
    view,
    csrf: session.csrf,
    // An administrator who did not tick the box still gets a fork, exactly as
    // the old form behaved: the safe destination is the default one.
    asFork: session.isSuper && asBase.value ? undefined : props.stats.base || props.stats.name,
    replace: replace.value,
  })
  if (!j || j.ok === false || !j.result) {
    throw new Error(j?.error || t('import failed (HTTP ${s})', { s: status }))
  }
  ui.toast(
    t('imported ${n} nodes, ${m} blobs as ${kind}', {
      n: j.result.nodes,
      m: j.result.blobs,
      kind: j.result.kind,
    }),
  )
  catalog.sel = view
  ui.reload()
}
</script>

<template>
  <BarRow :title="'Import a dump'" :level="3" />

  <div class="row">
    <FieldRow :label="'metadata dump'" for="import-file">
      <input id="import-file" type="file" accept=".txt,.json,.ndjson,text/plain" @change="picked" />
    </FieldRow>
    <FieldRow label="as view" for="import-target">
      <input id="import-target" v-model="target" :placeholder="`${props.stats.name}-copy`" />
    </FieldRow>
    <label class="check" for="import-replace">
      <input id="import-replace" v-model="replace" type="checkbox" />
      {{ t('replace an existing tree') }}
    </label>
    <label v-if="session.isSuper" class="check" for="import-as-base">
      <input id="import-as-base" v-model="asBase" type="checkbox" />
      {{ t('import as a BASE view') }}
    </label>
    <ActionButton :action="run" :label="'Import'" />
  </div>

  <span class="muted">
    {{
      session.isSuper
        ? t(
            'a dump becomes a BASE view, or a fork of a view you govern — content bytes are never in a dump',
          )
        : t('imported as a FORK of ${name}; only an administrator can import a new base view', {
            name: props.stats.base || props.stats.name,
          })
    }}
  </span>
</template>

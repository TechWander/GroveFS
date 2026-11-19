<script setup lang="ts">
/*
  Define a name over an existing view: a lens, or an alias.

  The definition text is the artifact — the console writes exactly what
  `grovefs view define --file` takes, and shows it in the monospace box it was
  authored in, because a person who knows the grammar wants to edit it, not to be
  asked three questions about it. The kind picker presets the box (an `alias`
  line, or a `base` line with the hide/only/map comments that shape a lens), and
  switching kind rewrites the preset: half-typed grammar from the other kind is
  not a starting point anyone wants to clean up.

  The name defaults to nothing and the server refuses an empty one in its own
  words — inventing a `foo-lens` here would be this page guessing what a view is
  for.
*/
import { ref, watch } from 'vue'
import type { ViewStats } from '@/types'
import { useCatalogStore } from '@/stores/catalog'
import { useOps } from '@/composables/useOps'
import { useUiStore } from '@/stores/ui'
import { messageOf } from '@/utils/error'
import { t } from '@/i18n'
import BaseModal from '@/components/overlay/BaseModal.vue'
import FieldRow from '@/components/layout/FieldRow.vue'

const props = defineProps<{ stats: ViewStats }>()
const emit = defineEmits<{ close: [] }>()

const { api } = useOps()
const catalog = useCatalogStore()
const ui = useUiStore()

type DefKind = 'derived' | 'alias'

const name = ref('')
const kind = ref<DefKind>('derived')

/** The template for a kind, in the grammar's own spelling. */
function preset(k: DefKind): string {
  return k === 'alias'
    ? `alias ${props.stats.name}\n`
    : `base ${props.stats.name}\n# hide /subtree\n# only /shown\n# map /src/path /shown/path\n`
}

const text = ref(preset('derived'))
watch(kind, (k) => {
  text.value = preset(k)
})

async function run(fail: (msg: string) => void, close: () => void): Promise<void> {
  try {
    await api('def.put', { name: name.value.trim(), text: text.value })
    close()
    ui.toast(t('definition saved'))
    catalog.sel = name.value.trim()
    ui.reload()
  } catch (e) {
    fail(messageOf(e))
  }
}
</script>

<template>
  <BaseModal
    :title="'Define a view over ${name}'"
    :vars="{ name: props.stats.name }"
    :confirm="'Save'"
    @confirm="run"
    @close="emit('close')"
  >
    <p class="muted">
      {{ t('The definition text is exactly what `grovefs view define --file` takes.') }}
    </p>
    <FieldRow for="define-kind" label="kind">
      <select id="define-kind" v-model="kind">
        <option value="derived">{{ t('derived lens (read-only shape over a base)') }}</option>
        <option value="alias">{{ t('alias (another name for it)') }}</option>
      </select>
    </FieldRow>
    <FieldRow for="define-name" label="new view name">
      <input id="define-name" v-model="name" :placeholder="`${props.stats.name}-lens`" />
    </FieldRow>
    <FieldRow for="define-text" label="definition">
      <textarea id="define-text" v-model="text" rows="8" spellcheck="false"></textarea>
    </FieldRow>
  </BaseModal>
</template>

<script setup lang="ts">
/*
  Edit the definition this name was built from, in the grammar it was written in.

  There is no form here on purpose: a lens is a small text file, and the console
  that hides it behind dropdowns would force a round trip through the CLI for
  anything the wizard does not predict. What the box holds is exactly what
  `grovefs view define --file` takes, so the two tools cannot disagree about a
  view's shape.

  Saving re-reads THIS view only (`@saved`), not the namespace: the text has
  changed, no name has, and a page that rebuilds both panes loses the selection
  stripe for no reason. The footnote is the part people need — the server caches
  definitions per mount, so a live mount notices on its own schedule, within
  seconds, without a restart.
*/
import { ref } from 'vue'
import type { ViewStats } from '@/types'
import { useOps } from '@/composables/useOps'
import { useUiStore } from '@/stores/ui'
import { t } from '@/i18n'
import BarRow from '@/components/layout/BarRow.vue'
import ActionButton from '@/components/ui/ActionButton.vue'

const props = defineProps<{ stats: ViewStats }>()
const emit = defineEmits<{ saved: [] }>()

const { api } = useOps()
const ui = useUiStore()

const text = ref(props.stats.definition || '')

async function run(): Promise<void> {
  await api('def.put', { name: props.stats.name, text: text.value })
  ui.toast(t('saved — live mounts pick it up within their attr TTL (≤2s)'))
  emit('saved')
}
</script>

<template>
  <BarRow :title="'Edit this definition'" :level="3" />

  <textarea
    v-model="text"
    rows="6"
    spellcheck="false"
    :aria-label="t('Edit this definition')"
  ></textarea>

  <div class="row">
    <ActionButton :action="run" :label="'Save'" />
  </div>
</template>

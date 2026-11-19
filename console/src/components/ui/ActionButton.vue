<script setup lang="ts">
/*
  Every verb in the console is this button.

  It is disabled while its call is in flight (so a double-click cannot mint two
  keys) and a rejection becomes a toast — a failure that only reaches the
  developer console is a silent failure for the user. The `.on` spinner is the
  one addition over the old file: a verb that takes a round trip now says so.
*/
import { ref } from 'vue'
import { useUiStore } from '@/stores/ui'
import { messageOf } from '@/utils/error'
import { t } from '@/i18n'

const props = defineProps<{
  /** A dictionary key, so a language switch relabels every verb. */
  label: string
  /** The verb itself. It may return nothing or a promise — `run()` awaits
      whatever comes back, so one type covers every caller in the console. */
  action: () => unknown
  /** The legacy class string: `primary`, `mini`, `link`, `danger`, `mini danger`. */
  variant?: string
  title?: string
  disabled?: boolean
}>()

const ui = useUiStore()
const busy = ref(false)

async function run() {
  busy.value = true
  try {
    await props.action()
  } catch (e) {
    ui.toast(messageOf(e), 'bad')
  } finally {
    busy.value = false
  }
}
</script>

<template>
  <button
    type="button"
    :class="props.variant || ''"
    :disabled="busy || props.disabled"
    :title="props.title"
    :aria-busy="busy ? 'true' : undefined"
    @click="run"
  >
    {{ t(props.label) }}
    <span v-if="busy" class="spin" aria-hidden="true"></span>
  </button>
</template>

<script setup lang="ts">
/*
  "copy" — the one verb that never touches the gateway.

  It lives in the drawer next to an id and in the one-time secret dialog, and it
  works on plain HTTP too (see `useClipboard`), which is how the console is
  actually deployed inside a cluster.
*/
import { useClipboard } from '@/composables/useClipboard'
import { t } from '@/i18n'

const props = defineProps<{
  text: string
  /** `mini` in a record pane, `link` under a secret. */
  variant?: 'mini' | 'link'
  /** A dictionary key; the default is `copy`. */
  label?: string
}>()

const { copy } = useClipboard()
</script>

<template>
  <button type="button" :class="props.variant || 'mini'" @click="copy(props.text)">
    {{ t(props.label || 'copy') }}
  </button>
</template>

<script setup lang="ts">
/*
  One labelled control inside a form panel or a dialog.

  `for` is optional, and where a caller can name its control it should: the
  hand-written version paired label and input by adjacency only, which is
  invisible to a screen reader. `hint` is the quiet sentence under the control —
  the limit, or what the value actually does.

  `label` is a dictionary key.
*/
import { t } from '@/i18n'
const props = defineProps<{ label: string; for?: string }>()
</script>

<template>
  <div class="field">
    <label :for="props.for">{{ t(props.label) }}</label>
    <slot />
    <span v-if="$slots.hint" class="muted"><slot name="hint" /></span>
  </div>
</template>

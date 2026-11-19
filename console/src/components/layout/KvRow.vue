<script setup lang="ts">
/*
  One `<dt>`/`<dd>` pair, as a fragment: the two elements land directly in the
  parent `<dl>`, which is what keeps the label column aligned.

  An absent value is an em dash, not a blank cell — "the server sent nothing for
  this field" and "this field is empty" look identical otherwise, and only one of
  them is a bug.
*/
import { computed } from 'vue'
import { t } from '@/i18n'

const props = defineProps<{
  /** A dictionary key — the field's NAME, never its value. */
  label: string
  value?: string | number | null
  mono?: boolean
  muted?: boolean
}>()

const blank = computed(
  () => props.value === null || props.value === undefined || props.value === '',
)
</script>

<template>
  <dt>{{ t(props.label) }}</dt>
  <dd :class="{ mono: props.mono, muted: props.muted }">
    <slot>{{ blank ? '—' : props.value }}</slot>
  </dd>
</template>

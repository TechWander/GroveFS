<script setup lang="ts">
/*
  One word of STATE, coloured.

  `tone` is the verdict (`good` / `warn` / `bad`), `kind` is a NAME family (a
  view kind, or the `admin` role) — the two never merge into one colour, because
  a DISABLED admin is not the same fact as an enabled user. `title` carries the
  why: the badge is the scan, the tooltip is the sentence.
*/
const props = defineProps<{
  label: string
  tone?: 'good' | 'warn' | 'bad' | 'admin' | 'off'
  /** View kinds paint the badge's dot/border family: `base`, `fork`, … */
  kind?: string | null
  title?: string
}>()
</script>

<template>
  <span
    :class="['badge', props.tone || '', props.kind ? 'k-' + props.kind : '']"
    :title="props.title"
  >
    {{ props.label }}
  </span>
</template>

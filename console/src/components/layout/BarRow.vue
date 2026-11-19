<script setup lang="ts">
/*
  An in-page section heading with its own row: the title, then whatever belongs
  beside it — a one-line explanation, a count, a control.

  `title` is a dictionary key, and `vars` interpolates the ones it carries
  ("Keys bound to ${name}") — a section heading is still a sentence the reader
  has to understand in their own language, variable names included.
*/
import { t } from '@/i18n'
import type { Vars } from '@/i18n'
const props = defineProps<{ title: string; level?: 2 | 3; vars?: Vars }>()
</script>

<template>
  <div class="bar">
    <component :is="props.level === 3 ? 'h3' : 'h2'">{{ t(props.title, props.vars) }}</component>
    <slot />
  </div>
</template>

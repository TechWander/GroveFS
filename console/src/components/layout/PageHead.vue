<script setup lang="ts">
/*
  One big title, one line that says what the page is for, actions right.

  The `.bar` sections below it stay the small in-page headings, so a page has
  exactly one thing shouting at you.

  `title` and `desc` are DICTIONARY KEYS — a pre-translated sentence passes
  through `t()` unchanged, so a caller may build one here when it needs vars.
*/
import { t } from '@/i18n'

const props = defineProps<{ title: string; desc?: string }>()
</script>

<template>
  <div class="page-head">
    <div class="ph-t">
      <h2>{{ t(props.title) }}</h2>
      <p v-if="props.desc" class="muted">{{ t(props.desc) }}</p>
    </div>
    <div v-if="$slots.actions" class="ph-a">
      <slot name="actions" />
    </div>
  </div>
</template>

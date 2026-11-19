<script setup lang="ts">
/** The one-line answer while a page or a pane is still loading. */
import { t } from '@/i18n'

const props = defineProps<{ text?: string }>()
</script>

<template>
  <span class="muted" role="status">
    {{ t(props.text || 'loading…') }}
    <span class="spin" aria-hidden="true"></span>
  </span>
</template>

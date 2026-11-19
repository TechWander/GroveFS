<script setup lang="ts">
/**
  One labelled control inside a filter bar.

  `label` is a dictionary key.
*/
import { t } from '@/i18n'

const props = defineProps<{ label: string; for?: string }>()
</script>

<template>
  <div class="fld">
    <label :for="props.for">{{ t(props.label) }}</label>
    <slot />
  </div>
</template>

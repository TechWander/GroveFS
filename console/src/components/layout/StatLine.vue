<script setup lang="ts">
/**
  A labelled figure: the name, the value, and beside it the quiet sentence that
  says what the value is measured against. One `dl`-like rhythm for the
  deployment snapshot and for an account's own record.

  `label` is a dictionary key; the value is the slot, because it is data.
*/
import { t } from '@/i18n'

const props = defineProps<{ label: string }>()
</script>

<template>
  <div class="stat-line">
    <span class="lbl">{{ t(props.label) }}</span>
    <span class="val"><slot /></span>
  </div>
</template>

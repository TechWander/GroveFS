<script setup lang="ts">
/*
  A chip row for a filter with a fixed set of values, each carrying its OWN
  count — so "how many are expired" is answerable before anything is clicked.

  `dot` names a COLOUR FAMILY (a view kind); a plain filter gets none, so the
  dot keeps meaning "kind" and nothing else.
*/
import type { ChipItem } from './chips'
import { t } from '@/i18n'

const props = defineProps<{
  items: ChipItem[]
  modelValue: string
}>()

const emit = defineEmits<{ 'update:modelValue': [value: string] }>()
</script>

<template>
  <div class="chips" role="group">
    <button
      v-for="item in props.items"
      :key="item.v"
      type="button"
      :class="['chip', { dotless: !item.dot, on: props.modelValue === item.v }]"
      :data-kind="item.dot || undefined"
      :aria-pressed="props.modelValue === item.v ? 'true' : 'false'"
      @click="emit('update:modelValue', item.v)"
    >
      {{ t(item.t) }}
      <span v-if="item.n !== undefined && item.n !== null" class="c-sub">{{ item.n }}</span>
    </button>
  </div>
</template>

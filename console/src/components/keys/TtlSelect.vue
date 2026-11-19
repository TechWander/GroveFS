<script setup lang="ts">
/*
  The deadline picker, in the CLI's own spelling (`--ttl 7d`).

  Weeks are absent because the parser has no `w` — offering `1w` here would be
  offering an error message. The empty value means "no deadline" on create and
  is paired with an explicit `keep unchanged` on edit, where "not now" and "never"
  are two different answers (`TTL_KEEP` is the wire spelling of the first).
*/
import { t } from '@/i18n'
import { TTL_CHOICES, TTL_KEEP } from '@/utils/text'

const props = defineProps<{ modelValue: string; keep?: boolean }>()
const emit = defineEmits<{ 'update:modelValue': [value: string] }>()
</script>

<template>
  <select
    :value="props.modelValue"
    @change="emit('update:modelValue', ($event.target as HTMLSelectElement).value)"
  >
    <option v-if="props.keep" :value="TTL_KEEP">{{ t('keep unchanged') }}</option>
    <option v-for="[value, label] in TTL_CHOICES" :key="value" :value="value">
      {{ t(label) }}
    </option>
  </select>
</template>

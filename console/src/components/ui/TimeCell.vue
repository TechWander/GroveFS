<script setup lang="ts">
/*
  A timestamp as a table can show it: MM-DD HH:MM:SS, with the full RFC 3339
  stamp one hover away. An ISO stamp is half a table wide otherwise.
*/
import { computed } from 'vue'
import { shortTime } from '@/utils/time'

const props = defineProps<{ ts?: string | null }>()

const short = computed(() => shortTime(props.ts))
</script>

<template>
  <span :title="props.ts || ''">{{ short }}</span>
</template>

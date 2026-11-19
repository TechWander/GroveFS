<script setup lang="ts">
/**
  One block of the drawer: an uppercase heading, then the record's fields (a
  `KvList`, so the label column aligns) and any table or paragraph the block
  needs. The spacing between blocks is the drawer's own.

  `heading` is a dictionary key.
*/
import { t } from '@/i18n'

const props = defineProps<{ heading?: string }>()
</script>

<template>
  <section>
    <h3 v-if="props.heading" class="det-h">{{ t(props.heading) }}</h3>
    <slot />
  </section>
</template>

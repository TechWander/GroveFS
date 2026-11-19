<script setup lang="ts">
/*
  The filter row every list page opens with.

  Two rows on purpose: the search box and the live result count share the top
  one, while every labelled facet sits below — so a page's query and its facets
  never compete for one line. A bar with ONLY a search (the delegation list)
  gives that search the whole row.
*/
import { useSlots } from 'vue'

const props = defineProps<{
  /** `counted(shown, total)` from the page: how many rows this filter leaves. */
  count?: string
}>()

const slots = useSlots()
</script>

<template>
  <div class="filterbar">
    <div class="filterbox filter-top">
      <slot name="search" />
      <span class="count" role="status" aria-live="polite">{{ props.count || '' }}</span>
    </div>
    <div v-if="slots.opts" class="filterbox filter-opts">
      <slot name="opts" />
    </div>
  </div>
</template>

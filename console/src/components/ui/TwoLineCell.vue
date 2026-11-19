<script setup lang="ts">
/*
  Two facts in one cell: the one you scan, then the one you read.

  This is the console's most-used cell — a label over its id, an account over
  when it joined, a state badge over its deadline — so it is one component
  rather than four hand-written divs.
*/
import MonoText from './MonoText.vue'

const props = defineProps<{
  top?: string | null
  sub?: string | null
  /** The second line is usually an identifier, and identifiers are mono. */
  subMono?: boolean
}>()
</script>

<template>
  <div class="cell2">
    <div>
      <slot name="top">{{ props.top }}</slot>
    </div>
    <div v-if="props.sub || $slots.sub" class="c-sub">
      <slot name="sub">
        <MonoText v-if="props.subMono" :text="props.sub" />
        <template v-else>{{ props.sub }}</template>
      </slot>
    </div>
  </div>
</template>

<script setup lang="ts">
/*
  The console's form container: a titled box whose direct children lay out as a
  responsive field grid. The slot therefore stays FLAT — a page passes its
  `FieldRow`s and its submit button directly, because wrapping them in another
  div would turn the grid into a single column.

  `title` is a dictionary key, like every other label in this UI.
*/
import { t } from '@/i18n'

const props = defineProps<{ title: string }>()
</script>

<template>
  <div class="panel">
    <h3>{{ t(props.title) }}</h3>
    <slot />
  </div>
</template>

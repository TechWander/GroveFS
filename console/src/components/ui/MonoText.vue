<script setup lang="ts">
/** Identifiers and values that must be read character by character. */
const props = defineProps<{ text: string | number | null | undefined }>()
</script>

<template>
  <span class="mono">{{ props.text ?? '' }}</span>
</template>

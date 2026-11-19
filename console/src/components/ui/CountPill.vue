<script setup lang="ts">
/*
  A count, or a jump that looks like one.

  `.count-pill` is the console's quietest control: as a `<span>` it reports how
  many names hang below a view, as a `<button>` it IS the navigation ("open this
  base view", "show this account's keys"), and `.on` says the number is worth
  clicking. Static when there is nothing to open — a button that does nothing is
  the worst affordance a table can carry.
*/
const props = defineProps<{
  text: string
  title?: string
  clickable?: boolean
  on?: boolean
}>()

const emit = defineEmits<{ click: [] }>()
</script>

<template>
  <button
    v-if="props.clickable"
    type="button"
    :class="['count-pill', { on: props.on }]"
    :title="props.title"
    @click="emit('click')"
  >
    {{ props.text }}
  </button>
  <span v-else class="count-pill" :title="props.title">{{ props.text }}</span>
</template>

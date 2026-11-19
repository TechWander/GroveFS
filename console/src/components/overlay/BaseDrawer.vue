<script setup lang="ts">
/*
  The row drawer: a record's full field set in a second pane, so a list can stay
  narrow (the columns you compare) while every long value — a note, a timestamp
  chain, a refusal's detail text — is one click away, and scroll position and
  filters survive the look.

  It is NOT a dialog: nothing is confirmed here. `aria-modal="false"` says so, so
  a screen reader does not trap its user in the pane, while `Esc`, the scrim and
  the ✕ all close it and focus goes back where it came from. Only one drawer
  exists at a time (the ui store holds one entry): opening a second row replaces
  the first.
*/
import { onBeforeUnmount, onMounted, ref } from 'vue'
import { t } from '@/i18n'
import { onEscape, pushOverlay } from '@/composables/overlayStack'

const props = defineProps<{
  title: string
  /** A name is monospace; a sentence is not. */
  mono?: boolean
}>()

const emit = defineEmits<{ close: [] }>()
const pane = ref<HTMLElement | null>(null)
let opener: Element | null = null
const overlay = pushOverlay()
let offEscape: (() => void) | null = null

onMounted(() => {
  opener = document.activeElement
  pane.value?.focus()
  offEscape = onEscape(overlay.isTop, () => emit('close'))
})

onBeforeUnmount(() => {
  overlay.pop()
  offEscape?.()
  offEscape = null
  if (opener instanceof HTMLElement && opener.isConnected) opener.focus()
})
</script>

<template>
  <Teleport to="body">
    <div class="drawer-back" @click="emit('close')"></div>
    <div
      ref="pane"
      class="drawer"
      role="dialog"
      aria-modal="false"
      :aria-label="props.title"
      tabindex="-1"
    >
      <div class="dh">
        <h2 :class="props.mono ? 'mono' : ''">{{ props.title }}</h2>
        <slot name="badges" />
        <button type="button" class="x" :aria-label="t('Close')" @click="emit('close')">✕</button>
      </div>
      <div class="db">
        <slot />
      </div>
      <div v-if="$slots.left || $slots.footer" class="df">
        <span v-if="$slots.left" class="left"><slot name="left" /></span>
        <slot name="footer" />
      </div>
    </div>
  </Teleport>
</template>

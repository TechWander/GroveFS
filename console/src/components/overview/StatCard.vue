<script setup lang="ts">
/*
  One deployment figure with its name and what it is measured against.

  The overview's grid puts FOUR of these per row, so the eight numbers an
  operator scans land in two rows instead of a width-dependent ribbon. `tone` is
  STATE only (amber = act on it, green = verified) — colour never decorates a
  plain number here.

  A count gets the display size; a NAME (the backend, the signed-in account) is
  set as a heading, so the grid reads with one rhythm instead of nine different
  type sizes. That is the `text` class below, and it is decided from the VALUE,
  not from what the caller thinks the field is: a count that arrives as `null`
  is still a count, and an account name is still a name.

  A card with a `page` is a jump, and says so — the number and the destination
  are the same fact. It stays keyboard-operable, which the hand-written `<div>`
  with an `onclick` was not.
*/
import { computed } from 'vue'
import type { Vars } from '@/i18n'
import { useNav } from '@/composables/useNav'
import { t } from '@/i18n'

const props = defineProps<{
  /** Dictionary key of the card's name. */
  label: string
  /** The figure or the name. `null`/`undefined` renders the em dash. */
  value?: string | number | null
  /** Dictionary key of the quiet line under it. */
  sub?: string
  vars?: Vars
  tone?: 'ok' | 'warn' | 'mute'
  /** Where the card jumps, as the route name. */
  page?: string
}>()

const { to } = useNav()

/** The old page's rule, kept verbatim: a value made only of digits, separators
    and spacing is a figure; anything else is a word. */
const FIGURE = /^[\d.,%\s—–-]+$/
const isText = computed(() => typeof props.value === 'string' && !FIGURE.test(props.value))
const shown = computed(() =>
  props.value === null || props.value === undefined ? '—' : String(props.value),
)
const subText = computed(() => (props.sub ? t(props.sub, props.vars) : ''))

function jump(): void {
  if (props.page) void to(props.page)
}

function onKey(ev: KeyboardEvent): void {
  if (!props.page) return
  if (ev.key === 'Enter' || ev.key === ' ') {
    ev.preventDefault()
    jump()
  }
}
</script>

<template>
  <div
    :class="['card', props.page ? 'link-card' : '', props.tone || '', isText ? 'text' : '']"
    :role="props.page ? 'button' : undefined"
    :tabindex="props.page ? 0 : undefined"
    @click="jump"
    @keydown="onKey"
  >
    <div class="k">{{ t(props.label) }}</div>
    <div class="n">{{ shown }}</div>
    <div v-if="subText" class="s">{{ subText }}</div>
  </div>
</template>

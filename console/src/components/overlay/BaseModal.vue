<script setup lang="ts">
/*
  One dialog, with the hand-written console's contract kept intact.

  The contract that matters is the INLINE error: the verb's handler gets
  `(fail, close)`, so a refusal shows INSIDE the dialog that caused it — the
  server's ACL sentence about a missing delegation is only useful next to the
  button that pressed for it, where the text the user just typed is still on
  screen. `fail(msg)` keeps the dialog open and re-enables the verb; `close()` is
  the only success path. The caller owns the mounting (`v-if`), which is what
  makes "the dialog is gone" one fact instead of a flag and a render.

  What is new here, and why: `role="dialog"` + `aria-modal`, an initial focus, a
  Tab that cannot leave the card, the focus returning to the control that opened
  the dialog, and an `Esc` that belongs to the TOPMOST overlay only — in the old
  file `Esc` closed the drawer no matter what was standing in front of it.
*/
import { computed, onBeforeUnmount, onMounted, ref } from 'vue'
import type { Vars } from '@/i18n'
import { t } from '@/i18n'
import { onEscape, pushOverlay } from '@/composables/overlayStack'

const props = defineProps<{
  /** Dictionary key of the title; `vars` interpolates into it. */
  title: string
  vars?: Vars
  /** Dictionary key of the verb. Without it the dialog only reports. */
  confirm?: string
  confirmVars?: Vars
  confirmVariant?: 'primary' | 'danger'
  /** Hold the verb inert while its preconditions are unmet (e.g. a delete
      preview whose server-side guard refused). The button stays VISIBLE so the
      operator sees the action they wanted is blocked here, next to the reason —
      not after a click that only 400s. */
  confirmDisabled?: boolean
  /** Dictionary key of the cancel button; it is always there, like the old
      `modal()` put it first in every footer. */
  cancel?: string
}>()

const emit = defineEmits<{
  confirm: [fail: (msg: string) => void, close: () => void]
  close: []
}>()

const busy = ref(false)
const error = ref('')
const card = ref<HTMLElement | null>(null)

const titleText = computed(() => t(props.title, props.vars))
const confirmText = computed(() => (props.confirm ? t(props.confirm, props.confirmVars) : ''))
const cancelText = computed(() => t(props.cancel ?? 'Cancel'))

const FOCUSABLE =
  'a[href], button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])'

/** Where focus goes back to when the dialog closes — the control the user
    pressed, not the top of the document. */
let opener: Element | null = null
const overlay = pushOverlay()
let offEscape: (() => void) | null = null

function fail(msg: string): void {
  error.value = msg
  busy.value = false
}

function close(): void {
  busy.value = false
  emit('close')
}

function submit(): void {
  if (busy.value) return
  error.value = ''
  busy.value = true
  emit('confirm', fail, close)
}

function trap(ev: KeyboardEvent): void {
  if (ev.key !== 'Tab' || !card.value) return
  const list = [...card.value.querySelectorAll<HTMLElement>(FOCUSABLE)].filter(
    (n) => n.offsetParent !== null,
  )
  const first = list[0]
  const last = list[list.length - 1]
  if (!first || !last) {
    ev.preventDefault()
    return
  }
  const at = document.activeElement
  if (ev.shiftKey && (at === first || at === card.value)) {
    ev.preventDefault()
    last.focus()
  } else if (!ev.shiftKey && at === last) {
    ev.preventDefault()
    first.focus()
  }
}

onMounted(() => {
  opener = document.activeElement
  card.value?.addEventListener('keydown', trap)
  const first = card.value?.querySelector<HTMLElement>(FOCUSABLE)
  if (first) first.focus()
  else card.value?.focus()
  offEscape = onEscape(overlay.isTop, () => emit('close'))
})

onBeforeUnmount(() => {
  overlay.pop()
  offEscape?.()
  offEscape = null
  card.value?.removeEventListener('keydown', trap)
  if (opener instanceof HTMLElement && opener.isConnected) opener.focus()
})
</script>

<template>
  <Teleport to="body">
    <div id="modal-host">
      <div
        ref="card"
        class="modal"
        role="dialog"
        aria-modal="true"
        :aria-label="titleText"
        tabindex="-1"
      >
        <h3>{{ titleText }}</h3>
        <slot :fail="fail" :busy="busy" />
        <p v-if="error" class="err" role="alert">{{ error }}</p>
        <!-- Always there, exactly as the old `modal()` built it: its footer opened
             with a Cancel, so a dialog that only reports — the one-time secret is
             the case — still has a visible way out instead of only `Esc`. -->
        <div class="foot">
          <slot name="footer" :fail="fail" :submit="submit">
            <button type="button" :disabled="busy" @click="emit('close')">{{ cancelText }}</button>
            <button
              v-if="props.confirm"
              type="button"
              :class="props.confirmVariant || 'primary'"
              :disabled="busy || props.confirmDisabled"
              :aria-busy="busy ? 'true' : undefined"
              @click="submit"
            >
              {{ confirmText }}
              <span v-if="busy" class="spin" aria-hidden="true"></span>
            </button>
          </slot>
        </div>
      </div>
    </div>
  </Teleport>
</template>

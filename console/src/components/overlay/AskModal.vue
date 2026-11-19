<script setup lang="ts">
/*
  The one prompt that answers a PROMISE: `ui.ask()` returns a boolean, so a
  destructive verb reads as

      if (!(await ask(…))) return

  exactly as it did in the hand-written file. Cancelling, pressing `Esc` and
  being replaced by a newer prompt all resolve `false`; only the named verb
  resolves `true` — and it is the verb's own words on the button, because
  "Are you sure?" is answered without reading it.
*/
import { useUiStore } from '@/stores/ui'
import { t } from '@/i18n'
import BaseModal from './BaseModal.vue'

const ui = useUiStore()

/** Which button ended the dialog: `ask()` must not resolve `true` on a cancel
    that arrives one emit later. */
let confirmed = false

function onConfirm(_fail: (msg: string) => void, close: () => void): void {
  confirmed = true
  close()
}

function onClose(): void {
  ui.answer(confirmed)
  confirmed = false
}
</script>

<template>
  <BaseModal
    v-if="ui.asking"
    :title="ui.asking.title"
    :confirm="ui.asking.confirm"
    confirm-variant="danger"
    @confirm="onConfirm"
    @close="onClose"
  >
    <!-- A refusal's line breaks separate "why" from "what to do next", so the
         body is preformatted text and never a collapsed paragraph. -->
    <pre>{{ t(ui.asking.body, ui.asking.vars) }}</pre>
  </BaseModal>
</template>

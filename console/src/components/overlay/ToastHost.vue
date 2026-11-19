<script setup lang="ts">
/*
  The loudest answer the console gives, and the one that must never be the only
  way to read a failure: a refusal a dialog CAN show inline does that instead.

  `aria-live="polite"` is kept from the hand-written page — a governance failure
  that a screen reader never hears is a failure the operator cannot act on. A
  failure stays up five times as long as a success, and now carries its own ✕,
  because a 15-second toast that cannot be dismissed is a countdown, not a
  notice.
*/
import { useUiStore } from '@/stores/ui'
import { t } from '@/i18n'

const ui = useUiStore()
</script>

<template>
  <div id="toast-host" aria-live="polite">
    <div v-for="x in ui.toasts" :key="x.id" :class="['toast', x.kind === 'info' ? '' : x.kind]">
      <span>{{ x.msg }}</span>
      <button type="button" class="x" :aria-label="t('Close')" @click="ui.dismissToast(x.id)">
        ✕
      </button>
    </div>
  </div>
</template>

<style>
.toast {
  display: flex;
  align-items: flex-start;
  gap: var(--sp-3);
}
.toast > span {
  flex: 1 1 auto;
  min-width: 0;
}
.toast .x {
  flex: none;
  width: 1.25rem;
  height: 1.25rem;
  margin: -0.15rem -0.25rem 0 0;
  padding: 0;
  border: 0;
  border-radius: var(--r-xs);
  background: transparent;
  color: var(--fg-faint);
  font-size: 0.72rem;
  line-height: 1;
}
.toast .x:hover {
  background: var(--bg-sunken);
  color: var(--fg);
}
</style>

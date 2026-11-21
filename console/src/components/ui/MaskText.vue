<script setup lang="ts">
/*
  The stored credential fingerprint: `a1b2…c3d4`.

  The elision is DRAWN, not typed, so it cannot be mistaken for part of the
  value — and the title says what the middle actually is. It is cut out of the
  secret, not out of its digest: the digest is what a handshake is checked
  against, it gives no hint back, and a table of it would be a table of strings
  nobody can use. The full secret reaches a screen in exactly one way — Reveal,
  unwrapped from the copy the store keeps — and the audit trail gets a row for
  every time that happens.
*/
import { t } from '@/i18n'

const props = defineProps<{ hint?: string | null }>()

const WHY = () =>
  t(
    'first and last 4 hex digits of the secret; a list never shows the middle, and only Reveal can — audited, from the store’s wrapped copy',
  )

function parts(hint: string): [string, string | null] {
  const cut = hint.indexOf('…')
  return cut < 0 ? [hint, null] : [hint.slice(0, cut), hint.slice(cut + 1)]
}
</script>

<template>
  <span v-if="!props.hint" class="muted">{{ t('no stored fingerprint') }}</span>
  <span v-else-if="parts(props.hint)[1] === null" class="mask">{{ props.hint }}</span>
  <span v-else class="mask" :title="WHY()">
    {{ parts(props.hint)[0] }}
    <i>…</i>
    {{ parts(props.hint)[1] }}
  </span>
</template>

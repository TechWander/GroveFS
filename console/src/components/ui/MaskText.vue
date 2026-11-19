<script setup lang="ts">
/*
  The stored credential fingerprint: `a1b2…c3d4`.

  The elision is DRAWN, not typed, so it cannot be mistaken for part of the
  value — and the title says WHY the middle is missing: GroveFS stores a
  SHA-256 digest, so no view, no API and no database dump can produce it.
  `apikey rotate` is the way to get a usable secret back.
*/
import { t } from '@/i18n'

const props = defineProps<{ hint?: string | null }>()

const WHY = () =>
  t(
    'first and last 4 hex digits of the secret; only its digest is stored, so the middle can never be shown again',
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

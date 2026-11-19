<script setup lang="ts">
/*
  Base views as pills that JUMP.

  An account's reach is a list of names, and the next question is always what one
  of those names looks like — so the name is the link, not an icon beside it.
  Three are shown and the rest collapse into `+N`, whose tooltip lists them: a row
  that grew to twelve delegations would otherwise make the table unreadable, and
  "this account governs a lot" is a fact worth seeing at a glance.
*/
import { useNav } from '@/composables/useNav'
import { t } from '@/i18n'

const props = defineProps<{ bases: string[] }>()

const { openView } = useNav()

const MAX = 3
const shown = (): string[] => props.bases.slice(0, MAX)
const rest = (): string[] => props.bases.slice(MAX)

function jump(name: string): void {
  void openView(name)
}
</script>

<template>
  <span v-if="!bases.length" class="muted">{{ t('none') }}</span>
  <div v-else class="cell-badges">
    <button
      v-for="b in shown()"
      :key="b"
      type="button"
      class="count-pill"
      :title="t('open this base view')"
      @click="jump(b)"
    >
      {{ b }}
    </button>
    <span v-if="rest().length" class="count-pill" :title="rest().join(', ')">
      {{ '+' + rest().length }}
    </span>
  </div>
</template>

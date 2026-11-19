<script setup lang="ts">
/*
  An account as a person, not a bare token: initial, name, and the one fact that
  distinguishes two people (what it did here).

  `off` is the DISABLED state — the avatar loses its colour, so a retired
  principal is recognisable in a table of forty rows without reading the name.
*/
import { computed } from 'vue'
import { t } from '@/i18n'

const props = defineProps<{
  name?: string | null
  /** The second line: "joined 11-02 …", "minted by …". */
  sub?: string | null
  off?: boolean
}>()

const initials = computed(() =>
  String(props.name ?? '')
    .slice(0, 2)
    .toUpperCase(),
)
</script>

<template>
  <span v-if="!props.name" class="muted">{{ t('unassigned') }}</span>
  <div v-else class="who-cell">
    <span :class="['avatar', { off: props.off }]">{{ initials }}</span>
    <div>
      <div class="nm mono">{{ props.name }}</div>
      <div v-if="props.sub" class="sub2">{{ props.sub }}</div>
    </div>
  </div>
</template>

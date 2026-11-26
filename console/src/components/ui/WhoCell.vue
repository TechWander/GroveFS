<script setup lang="ts">
/*
  An account as a person, not a bare token: initial, name, and the one fact that
  distinguishes two people (what it did here).

  `off` is the DISABLED state — the avatar loses its colour, so a retired
  principal is recognisable in a table of forty rows without reading the name.

  `go` makes the NAME itself the link (the parent handles `navigate`) — used
  wherever an account appears inside another entity's row: a key's owner, a
  call's actor. Only the name is the target, so a click beside it still hits
  the enclosing row instead of stealing it.
*/
import { computed } from 'vue'
import { t } from '@/i18n'

const props = defineProps<{
  name?: string | null
  /** The second line: "joined 11-02 …", "minted by …". */
  sub?: string | null
  off?: boolean
  go?: boolean
}>()
const emit = defineEmits<{ navigate: [] }>()

const initials = computed(() =>
  String(props.name ?? '')
    .slice(0, 2)
    .toUpperCase(),
)
</script>

<template>
  <span v-if="!props.name" class="muted unassigned">{{ t('unassigned') }}</span>
  <div v-else class="who-cell">
    <span :class="['avatar', { off: props.off }]">{{ initials }}</span>
    <div>
      <button
        v-if="props.go"
        type="button"
        class="mini mono nm"
        :title="t('open this account')"
        @click="emit('navigate')"
      >
        {{ props.name }}
      </button>
      <div v-else class="nm mono">{{ props.name }}</div>
      <div v-if="props.sub" class="sub2">{{ props.sub }}</div>
    </div>
  </div>
</template>

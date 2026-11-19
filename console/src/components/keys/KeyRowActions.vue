<script setup lang="ts">
/*
  The four verbs a live credential has, in one place.

  The keys page's row, the key drawer's footer and the views page's
  "keys bound to this view" table all show exactly these, so a key cannot offer a
  different set of actions depending on where it was found — the mistake the old
  file came closest to making (it repeated the four buttons twice).

  A revoked row shows none of them: nothing can un-revoke a credential and a
  revoked row is not editable, so offering a verb that is guaranteed to be
  refused is worse than offering nothing.
*/
import { computed, ref } from 'vue'
import type { KeyRow } from '@/types'
import { useOps } from '@/composables/useOps'
import { useSessionStore } from '@/stores/session'
import { useUiStore } from '@/stores/ui'
import { t } from '@/i18n'
import ActionButton from '@/components/ui/ActionButton.vue'
import KeyEditDialog from '@/components/dialogs/KeyEditDialog.vue'

const props = withDefaults(defineProps<{ row: KeyRow; place?: 'row' | 'drawer' }>(), {
  place: 'row',
})

const { api } = useOps()
const session = useSessionStore()
const ui = useUiStore()

const editing = ref(false)

/** `label · id · scope` is the credential: enough to be certain which one is
    about to stop working, without a second lookup. */
function identity(note: string | null): Record<string, string> {
  return { l: props.row.label, i: props.row.id, s: props.row.scope, note: note ?? '' }
}

const binding = computed(() =>
  props.row.view
    ? t('bound to ${v}', { v: props.row.view })
    : t('NOT bound to a view — it reaches every tree'),
)

async function toggleRo(): Promise<void> {
  const ro = !props.row.ro
  await api('apikey.set_ro', { id: props.row.id, ro })
  ui.toast(ro ? t('now read-only') : t('now read-write'))
  ui.reload()
}

async function revoke(): Promise<void> {
  if (
    !(await ui.ask(
      'Revoke this key?',
      '${l} (${i}) · scope ${s} · ${note}\n\nLive mounts keep working until their next operation.',
      identity(binding.value),
      'Revoke',
    ))
  )
    return
  await api('apikey.revoke', { id: props.row.id })
  ui.toast(t('revoked'))
  ui.reload()
}

/** Rotate = "the secret is lost but the credential stays": same id, label,
    scope, view binding and audit chain; only the digest is replaced, and the new
    plaintext gets its one-time display. */
async function rotate(): Promise<void> {
  if (
    !(await ui.ask(
      'Rotate this key?',
      '${l} (${i}) · scope ${s}\n\nThe id, label, scope, view binding and audit history are kept; only the secret is replaced. The old secret stops working at its NEXT handshake — a live mount on it stays open until it reconnects; to kill a credential outright, Revoke it.',
      identity(null),
      'Rotate',
    ))
  )
    return
  const res = await api('apikey.rotate', { id: props.row.id })
  ui.showSecret({
    title: 'Key rotated',
    detailed: false,
    secret: res,
    view: props.row.view ?? null,
  })
  ui.toast(t('key rotated'))
  ui.reload()
}

interface Verb {
  key: string
  run: () => unknown
  variant?: string
  show: boolean
}

const verbs = computed<Verb[]>(() => {
  const edit: Verb = {
    key: 'Edit',
    run: () => {
      editing.value = true
    },
    show: session.can('apikey.update'),
  }
  const ro: Verb = { key: props.row.ro ? 'Make rw' : 'Make ro', run: toggleRo, show: true }
  const rot: Verb = { key: 'Rotate', run: rotate, show: true }
  const rev: Verb = { key: 'Revoke', run: revoke, variant: 'mini danger', show: true }
  // The row leads with the flip that has no consequence; the drawer, which is
  // reading the record, leads with the verb that edits it. Both orders are the
  // ones the console has always shown.
  return (props.place === 'drawer' ? [edit, ro, rot, rev] : [ro, edit, rot, rev]).filter(
    (v) => v.show,
  )
})
</script>

<template>
  <div v-if="!props.row.revoked_at" class="row">
    <ActionButton
      v-for="v in verbs"
      :key="v.key"
      :label="v.key"
      :variant="v.variant || 'mini'"
      :action="v.run"
    />
  </div>

  <KeyEditDialog v-if="editing" :row="props.row" @close="editing = false" />
</template>

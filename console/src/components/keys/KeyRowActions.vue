<script setup lang="ts">
/*
  The verbs a credential has, in one place.

  The keys page's row, the key drawer's footer and the views page's
  "keys bound to this view" table all show exactly these, so a key cannot offer a
  different set of actions depending on where it was found — the mistake the old
  file came closest to making (it repeated the four buttons twice).

  Two verbs are conditional on more than the account: Edit on the binary
  answering for the op, Reveal on the registry holding a wrapped copy of THIS
  row's secret. Both are facts the list already carries, so a button that would
  only ever print a refusal sentence is not offered at all.

  A revoked row keeps exactly ONE verb: Delete. Nothing can un-revoke a
  credential and a revoked row is not editable, so offering a verb that is
  guaranteed to be refused is worse than offering nothing — but once a key is
  revoked, deleting it is the only useful thing left to do with it. Reading a
  revoked credential's secret back is legal (`apikey secret` on the CLI allows
  it for exactly that cleanup), but it buys a string that can no longer
  authenticate anything, which is not what a button here should be for.
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

const emit = defineEmits<{ deleted: [] }>()

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

const canDelete = computed(
  () =>
    session.can('apikey.delete') &&
    (session.isSuper || (!!props.row.owner && props.row.owner === session.account)),
)

/** Delete = "this credential should no longer exist". It is the hard cleanup
    verb, so the dialog names the row and says what it does NOT take with it:
    the audit trail keeps its attribution, and a live mount stops at its next
    operation. Revoke stays the reversible choice, and the copy points there. */
async function remove(): Promise<void> {
  if (
    !(await ui.ask(
      'Delete this key?',
      '${l} (${i}) · scope ${s}\n\nThis removes the credential from the store. A live mount stops at its next operation. The audit rows it wrote are kept. This cannot be undone — use Revoke instead if you only want to disable it.',
      identity(null),
      'Delete',
    ))
  )
    return
  await api('apikey.delete', { id: props.row.id })
  ui.toast(t('key deleted'))
  emit('deleted')
  ui.reload()
}

/** Rotate = "the secret is lost but the credential stays": same id, label,
    scope, view binding and audit chain; only the secret is replaced, and the new
    one gets this display. Whether the store ALSO kept a recoverable copy of it
    is the reply's to say, and the dialog's to tell. */
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
    copy: res.stored ? 'stored' : 'once',
    secret: res,
    view: props.row.view ?? null,
  })
  ui.toast(t('key rotated'))
  ui.reload()
}

/** Reveal = "the registry kept a copy, and this reads it back". No prompt: the
    click is already the decision, the modal that follows is the confirmation
    surface, and the store writes the call to the audit trail with the id it
    opened — which is the sentence that makes offering this verb at all
    responsible. */
async function reveal(): Promise<void> {
  const res = await api('apikey.secret', { id: props.row.id })
  if (!res.available || !res.key) {
    // The row said a copy exists, so this is the rare third answer: the
    // wrapping material moved to another node, or the copy no longer matches
    // its digest. The store's own sentence, verbatim, is the only useful thing
    // to show.
    ui.toast(res.why || t('no recoverable copy of this secret'), 'bad')
    return
  }
  ui.showSecret({
    title: 'Stored secret',
    detailed: false,
    copy: 'revealed',
    secret: {
      key: res.key,
      id: res.id,
      label: res.label,
      scope: res.scope,
      owner: res.owner,
    },
    view: res.view ?? null,
  })
}

interface Verb {
  key: string
  run: () => unknown
  variant?: string
  show: boolean
}

const verbs = computed<Verb[]>(() => {
  const live = !props.row.revoked_at
  const edit: Verb = {
    key: 'Edit',
    run: () => {
      editing.value = true
    },
    show: live && session.can('apikey.update'),
  }
  const revealV: Verb = {
    key: 'Reveal',
    run: reveal,
    show: live && session.can('apikey.secret') && !!props.row.secret_available,
  }
  const ro: Verb = { key: props.row.ro ? 'Make rw' : 'Make ro', run: toggleRo, show: live }
  const rot: Verb = { key: 'Rotate', run: rotate, show: live }
  const rev: Verb = { key: 'Revoke', run: revoke, variant: 'mini danger', show: live }
  const del: Verb = {
    key: 'Delete',
    run: remove,
    variant: 'mini danger',
    show: canDelete.value,
  }
  // The row leads with the flip that has no consequence; the drawer, which is
  // reading the record, leads with the verb that edits it. Deletion is last in
  // both: it is the one that cannot be taken back.
  return (
    props.place === 'drawer'
      ? [edit, ro, revealV, rot, rev, del]
      : [ro, edit, revealV, rot, rev, del]
  ).filter((v) => v.show)
})
</script>

<template>
  <div v-if="verbs.length" class="row">
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

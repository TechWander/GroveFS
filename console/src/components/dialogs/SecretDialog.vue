<script setup lang="ts">
/*
  The secret on screen.

  It is mounted by the shell instead of by the page that minted it for one
  reason that has nothing to do with crypto: a refresh of the list behind it, or
  the drawer it was rotated from closing, must never be able to take the
  credential away from someone who is mid-copy.

  What the bottom line promises depends on what the STORE did with the string,
  and the store is the only one who can say: with no wrapping material on the
  server this display is the only one there will ever be and `Rotate` is the way
  back; with a wrapped copy written, `Reveal` reads it again — and this dialog is
  also where that second reading happens, audited, under the same id. Guessing
  here would be the worst kind of UI bug: a sentence that tells an operator a
  credential is safe to discard when it is not, or safe to chase when nothing can
  bring it back.
*/
import { computed } from 'vue'
import { useUiStore } from '@/stores/ui'
import { maskOf } from '@/utils/text'
import { t } from '@/i18n'
import BaseModal from '@/components/overlay/BaseModal.vue'
import CopyButton from '@/components/ui/CopyButton.vue'
import KvList from '@/components/layout/KvList.vue'
import KvRow from '@/components/layout/KvRow.vue'
import MonoText from '@/components/ui/MonoText.vue'

const ui = useUiStore()

const state = computed(() => ui.secret)
const bound = computed(() =>
  state.value?.view ? t('bound to ${v}', { v: state.value.view }) : t('not bound to a view'),
)
const fingerprint = computed(() => (state.value ? maskOf(state.value.secret.key) : ''))

/** The line under the credential: three different deployments, three different
    truths, and the copy the operator should be told off for trusting is the one
    that is a lie. */
const lead = computed(() => {
  switch (state.value?.copy) {
    case 'stored':
      return t(
        'The registry kept a recoverable copy of this secret, wrapped under its store key — Reveal reads it back.',
      )
    case 'revealed':
      return t(
        'This is the copy the registry keeps, opened under its store key. The call is written to the audit trail with this id.',
      )
    default:
      return t(
        'This store keeps no recoverable copy of it, so this is the only time the secret is shown — Rotate is the way to get a new one.',
      )
  }
})

/** The bottom line of the create flow: what the list will show from now on,
    and which verb reads the rest of it back. */
const tail = computed(() => {
  const h = fingerprint.value
  return state.value?.copy === 'stored'
    ? t(
        'The list shows its fingerprint ${h}; Reveal reads this secret back for as long as the store holds its wrapping material.',
        { h },
      )
    : t(
        'Save it now. From here on the list shows its fingerprint ${h}; Rotate is the only way to obtain a usable secret again.',
        { h },
      )
})

/** A credential someone had to write down is a different errand from one they
    just fetched: the first asks "did you save it", the second just closes. */
const cancel = computed(() => (state.value?.copy === 'revealed' ? 'Close' : 'I have saved it'))

function dismiss(): void {
  ui.secret = null
}
</script>

<template>
  <BaseModal v-if="state" :title="state.title" :cancel="cancel" @close="dismiss">
    <p>{{ lead }}</p>
    <div class="secret-card">
      <code class="secret">{{ state.secret.key }}</code>
      <CopyButton variant="link" :text="state.secret.key" />
    </div>

    <KvList v-if="state.detailed">
      <KvRow label="id" mono :value="state.secret.id" />
      <KvRow label="label" :value="state.secret.label" />
      <KvRow label="owner" :value="state.secret.owner" />
      <KvRow label="minted by" :value="state.secret.created_by" />
      <KvRow label="scope" mono :value="state.secret.scope" />
      <KvRow label="view">
        <MonoText v-if="state.view" :text="state.view" />
        <span v-else>{{ t('not bound to a view') }}</span>
      </KvRow>
      <KvRow label="valid until" :value="state.secret.expires_at || t('never expires')" />
      <KvRow label="note" :value="state.secret.note" />
    </KvList>

    <p v-if="state.detailed" class="muted">{{ tail }}</p>
    <p v-else class="muted">
      {{
        t('label ${l} · scope ${s} · owner ${o} · ${b}', {
          l: state.secret.label,
          s: state.secret.scope,
          o: state.secret.owner || '—',
          b: bound,
        })
      }}
    </p>
  </BaseModal>
</template>

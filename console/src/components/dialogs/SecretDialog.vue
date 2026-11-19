<script setup lang="ts">
/*
  The one-time secret.

  GroveFS stores a digest, so this dialog is the only place the plaintext has
  ever existed on this machine — which is why it is mounted by the shell instead
  of by the page that minted it: a refresh of the list behind it, or the drawer it
  was rotated from closing, must never be able to take the credential away.

  The fingerprint line is the promise about what happens next: from here on the
  registry shows `a1b2…c3d4` and nothing else, and Rotate is the only way to get a
  usable secret again.
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

function dismiss(): void {
  ui.secret = null
}
</script>

<template>
  <BaseModal v-if="state" :title="state.title" cancel="I have saved it" @close="dismiss">
    <p>{{ t('This is the only time the secret is shown — GroveFS stores a digest.') }}</p>
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

    <p v-if="state.detailed" class="muted">
      {{
        t(
          'Save it now. From here on the list shows its fingerprint ${h}; Rotate is the only way to obtain a usable secret again.',
          { h: fingerprint },
        )
      }}
    </p>
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

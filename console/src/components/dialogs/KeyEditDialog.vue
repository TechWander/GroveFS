<script setup lang="ts">
/*
  The ONLY two fields a key's row lets you edit: its remark and its deadline.

  That restriction is the point, and the dialog says so out loud — scope, view
  binding and id are what a running application is already holding, and moving
  them under it would either break the mount or hand it a reach nobody asked for.

  The refusal stays inside this dialog, in the server's own words: "there is no
  ACTIVE row with that id" (a revoked key) and "bad ttl" both arrive here, where
  the text just typed is still on the screen.
*/
import { ref } from 'vue'
import type { KeyRow } from '@/types'
import { useOps } from '@/composables/useOps'
import { useUiStore } from '@/stores/ui'
import { messageOf } from '@/utils/error'
import { humanLeft, shortTime } from '@/utils/time'
import { NOTE_MAX, TTL_KEEP } from '@/utils/text'
import { t } from '@/i18n'
import BaseModal from '@/components/overlay/BaseModal.vue'
import FieldRow from '@/components/layout/FieldRow.vue'
import TtlSelect from '@/components/keys/TtlSelect.vue'

const props = defineProps<{ row: KeyRow }>()
const emit = defineEmits<{ close: [] }>()

const { api } = useOps()
const ui = useUiStore()

const note = ref(props.row.note ?? '')
const ttl = ref(TTL_KEEP)

/** What the deadline is right now, in the words the row uses — so "keep
    unchanged" is a choice the user can see the consequence of. */
function nowLine(): string {
  if (!props.row.expires_at) return t('no deadline now')
  const left = props.row.expired
    ? t('already past')
    : t('in ${left}', { left: humanLeft(props.row.expires_at) })
  return t('now ${at} — ${left}', { at: shortTime(props.row.expires_at), left })
}

async function run(fail: (msg: string) => void, close: () => void): Promise<void> {
  const args: Record<string, unknown> = { id: props.row.id, note: note.value }
  if (ttl.value !== TTL_KEEP) args.ttl = ttl.value || 'never'
  try {
    await api('apikey.update', args)
    close()
    ui.toast(t('key updated'))
    ui.reload()
  } catch (e) {
    fail(messageOf(e))
  }
}
</script>

<template>
  <BaseModal
    title="Edit ${label}"
    :vars="{ label: props.row.label }"
    confirm="Save"
    @confirm="run"
    @close="emit('close')"
  >
    <p class="muted">
      {{
        t(
          'A note and a deadline are bookkeeping: they change what the registry says about this key, never what it can open.',
        )
      }}
    </p>
    <FieldRow for="key-edit-note" label="note">
      <textarea
        id="key-edit-note"
        v-model="note"
        class="plain"
        rows="3"
        :maxlength="NOTE_MAX"
        spellcheck="false"
      ></textarea>
      <template #hint>
        {{ t('${n} characters at most; empty clears it', { n: NOTE_MAX }) }}
      </template>
    </FieldRow>
    <FieldRow for="key-edit-ttl" label="valid for">
      <TtlSelect v-model="ttl" keep />
      <template #hint>{{ nowLine() }}</template>
    </FieldRow>
  </BaseModal>
</template>

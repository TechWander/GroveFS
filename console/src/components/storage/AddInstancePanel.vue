<script setup lang="ts">
/*
  Register a new storage instance from the web, replacing the hand-edited
  `instances.conf`.

  The panel only appears when the registry is SERVER-MANAGED (a `file:` book the
  meta-server adopted at first start) and the caller holds `storage.add`. On an
  operator-owned registry there is nothing to append to from here — the page says
  so and stays read-only — so offering this form would only ever 403.

  The server validates the name and dials the URI before it writes (PG: a trial
  connection plus schema probe; treedb: a parse + `list_views` reach), so a typo
  is refused with the server's own sentence rather than becoming a card that
  cannot be reached. That refusal is shown inline, in the dialog's own error
  line, because the value just typed is what the operator needs to correct.

  The FIRST instance is named `control` by the startup adoption; this panel is
  for the ones added afterwards, so it never claims to move the control plane.
*/
import { ref } from 'vue'
import { useOps } from '@/composables/useOps'
import { useSessionStore } from '@/stores/session'
import { useUiStore } from '@/stores/ui'
import { messageOf } from '@/utils/error'
import { t } from '@/i18n'
import PanelBox from '@/components/layout/PanelBox.vue'
import FieldRow from '@/components/layout/FieldRow.vue'
import ActionButton from '@/components/ui/ActionButton.vue'

const props = defineProps<{ isPg: boolean }>()
const emit = defineEmits<{ close: [] }>()

const { api } = useOps()
const session = useSessionStore()
const ui = useUiStore()

const name = ref('')
const uri = ref('')
const error = ref('')

const uriPlaceholder = () => (props.isPg ? 'postgres://user:pw@host:5432/db' : 'host:port')

async function create(): Promise<void> {
  error.value = ''
  try {
    const r = await api('storage.add', { name: name.value.trim(), uri: uri.value.trim() })
    ui.toast(r.msg)
    emit('close')
    ui.reload()
  } catch (e) {
    // The server's guard sentence verbatim — a paraphrase of "cannot reach this
    // URI" tells the operator nothing they can fix.
    error.value = messageOf(e)
  }
}
</script>

<template>
  <PanelBox v-if="session.can('storage.add')" id="inst-add" :title="'Register an instance'">
    <FieldRow for="add-name" label="name">
      <input id="add-name" v-model="name" placeholder="replica-2" autocomplete="off" />
      <template #hint>{{ t('the placement identity — later views are balanced onto this name') }}</template>
    </FieldRow>
    <FieldRow for="add-uri" :label="isPg ? 'connection string' : 'address'">
      <input id="add-uri" v-model="uri" :placeholder="uriPlaceholder()" autocomplete="off" />
      <template #hint>
        {{
          isPg
            ? t('the meta-server dials it once before saving; an unreachable URI is refused')
            : t('treedb routes to one --meta-store address; this records an inventory entry')
        }}
      </template>
    </FieldRow>
    <p v-if="error" class="err" role="alert">{{ error }}</p>
    <div class="panel-foot">
      <ActionButton :action="create" :label="'Register'" variant="primary" :disabled="!name.trim() || !uri.trim()" />
      <button type="button" class="link" @click="emit('close')">{{ t('cancel') }}</button>
    </div>
  </PanelBox>
</template>

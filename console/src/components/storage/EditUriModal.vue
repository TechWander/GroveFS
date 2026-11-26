<script setup lang="ts">
/*
  Re-point a named instance at a different connection string / address.

  The NAME is the placement identity (PG) or the inventory key (treedb), so this
  never renames — it moves what the name resolves to. That is the right verb for
  a relocated or restored database, and nothing else. The server re-validates the
  URI and (PG) dials a trial connection before it writes; a refusal — including
  "the control plane URI moves by handover" and "the serving address changes with
  a restart" — is shown verbatim from the server's sentence, because a paraphrase
  of a guard tells the operator nothing they can act on.
*/
import { ref } from 'vue'
import type { StoreInstance } from '@/types'
import { useOps } from '@/composables/useOps'
import { useUiStore } from '@/stores/ui'
import { messageOf } from '@/utils/error'
import { t } from '@/i18n'
import BaseModal from '@/components/overlay/BaseModal.vue'
import FieldRow from '@/components/layout/FieldRow.vue'

const props = defineProps<{ inst: StoreInstance; isPg: boolean }>()
const emit = defineEmits<{ close: [] }>()

const { api } = useOps()
const ui = useUiStore()
const uri = ref(props.inst.uri ?? '')

async function run(fail: (msg: string) => void, close: () => void): Promise<void> {
  try {
    const r = await api('storage.update', { instance: props.inst.name, uri: uri.value.trim() })
    close()
    ui.toast(r.msg)
    ui.reload()
  } catch (e) {
    fail(messageOf(e))
  }
}
</script>

<template>
  <BaseModal
    :title="'Edit ${name}'"
    :vars="{ name: props.inst.name }"
    :confirm="'Save'"
    @confirm="run"
    @close="emit('close')"
  >
    <FieldRow for="edit-uri" :label="isPg ? 'connection string' : 'address'">
      <input
        id="edit-uri"
        v-model="uri"
        :placeholder="isPg ? 'postgres://user:pw@host:5432/db' : 'host:port'"
        autocomplete="off"
      />
      <template #hint>
        {{
          isPg
            ? t('views placed on this name now resolve here; the database must already hold their schemas')
            : t('treedb routes to one --meta-store address; this only updates the inventory record')
        }}
      </template>
    </FieldRow>
  </BaseModal>
</template>

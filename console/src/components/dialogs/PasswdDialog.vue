<script setup lang="ts">
/*
  Change a password — yours or one you administer.

  Two entries and a client-side equality check, and NOTHING else: the length
  rule is the server's (`at least 10 characters` is what it calls it, quoted
  under the field), so a short password still arrives as the server's own
  refusal rather than as a rule this page invented.

  It never reloads the list. A password is the one field the registry does not
  show, so nothing on screen would change — a refresh here would only throw
  away the row you were looking at.
*/
import { ref } from 'vue'
import { useOps } from '@/composables/useOps'
import { useUiStore } from '@/stores/ui'
import { messageOf } from '@/utils/error'
import { t } from '@/i18n'
import BaseModal from '@/components/overlay/BaseModal.vue'
import FieldRow from '@/components/layout/FieldRow.vue'

const props = defineProps<{ account: string }>()
const emit = defineEmits<{ close: [] }>()

const { api } = useOps()
const ui = useUiStore()

const pw1 = ref('')
const pw2 = ref('')

async function run(fail: (msg: string) => void, close: () => void): Promise<void> {
  if (pw1.value !== pw2.value) {
    fail(t('the two entries differ'))
    return
  }
  try {
    await api('account.passwd', { account: props.account, password: pw1.value })
    close()
    ui.toast(t('password changed'))
  } catch (e) {
    fail(messageOf(e))
  }
}
</script>

<template>
  <BaseModal
    :title="'Password for ${account}'"
    :vars="{ account: props.account }"
    :confirm="'Change password'"
    @confirm="run"
    @close="emit('close')"
  >
    <FieldRow label="new password" for="passwd-1">
      <template #hint>{{ t('at least 10 characters') }}</template>
      <input id="passwd-1" v-model="pw1" type="password" autocomplete="new-password" />
    </FieldRow>
    <FieldRow label="repeat it" for="passwd-2">
      <input id="passwd-2" v-model="pw2" type="password" autocomplete="new-password" />
    </FieldRow>
  </BaseModal>
</template>

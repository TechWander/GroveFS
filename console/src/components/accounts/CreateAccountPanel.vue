<script setup lang="ts">
/*
  Create a principal.

  Three fields, and the panel explains each one's limit where it is typed rather
  than in the refusal after it: the account grammar, the password floor, and —
  the one that actually matters — that a `user` is inert until a base view is
  delegated to it below. That last sentence is why this form and the delegation
  form sit on the same page.

  The password is cleared after a create and never shown again: GroveFS stores a
  digest of it, so this form is the only place it exists in the open, exactly as
  the key mint above behaves.
*/
import { ref } from 'vue'
import { useOps } from '@/composables/useOps'
import { useUiStore } from '@/stores/ui'
import { t } from '@/i18n'
import PanelBox from '@/components/layout/PanelBox.vue'
import FieldRow from '@/components/layout/FieldRow.vue'
import ActionButton from '@/components/ui/ActionButton.vue'

const ui = useUiStore()
const { api } = useOps()

const account = ref('')
const password = ref('')
const role = ref('user')

async function create(): Promise<void> {
  const r = await api('account.create', {
    account: account.value.trim(),
    password: password.value,
    role: role.value,
  })
  password.value = ''
  // The server's `note` is the part that is not in this form (how the first
  // owner differs, what was seeded), so it travels into the toast verbatim.
  ui.toast(
    t('${account} created as ${role} — ${note}', {
      account: r.account,
      role: r.role,
      note: r.note,
    }),
  )
  ui.reload()
}
</script>

<template>
  <PanelBox :title="'Create an account'">
    <FieldRow label="account" for="new-account">
      <input id="new-account" v-model="account" placeholder="bob" />
      <template #hint>{{ t('a-z 0-9 . _ -, 2–32 chars') }}</template>
    </FieldRow>

    <FieldRow label="password" for="new-account-password">
      <input
        id="new-account-password"
        v-model="password"
        type="password"
        autocomplete="new-password"
      />
      <template #hint>{{ t('at least 10 characters') }}</template>
    </FieldRow>

    <FieldRow label="role" for="new-account-role">
      <select id="new-account-role" v-model="role">
        <option value="user">{{ t('user — governs what it is delegated') }}</option>
        <option value="admin">{{ t('admin — governs the deployment') }}</option>
      </select>
      <template #hint>
        {{ t('a user is useless until it is delegated a base view below') }}
      </template>
    </FieldRow>

    <ActionButton :action="create" :label="'Create'" variant="primary" />
  </PanelBox>
</template>

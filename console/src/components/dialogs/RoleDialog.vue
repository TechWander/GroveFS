<script setup lang="ts">
/*
  Promote an account to administrator, or demote one.

  The dialog says what the role IS before it asks for the verb, because the two
  sentences are not symmetric: `admin` adds reach over every account, demoting
  removes it — and the reader should see which one they are holding.

  Refusals land inline, and the important one is the server's: "…is the only
  enabled administrator…". That guard is what keeps a deployment reachable, and
  its wording is the answer, so it is not paraphrased or pre-empted here.

  Changing your OWN role re-reads the session instead of reloading the page: the
  header's role badge and the op list the page is built from both came from
  `/api/session`, and after a demotion they are no longer true.
*/
import { computed } from 'vue'
import type { AccountRow } from '@/types'
import { useOps } from '@/composables/useOps'
import { useSessionStore } from '@/stores/session'
import { useUiStore } from '@/stores/ui'
import { messageOf } from '@/utils/error'
import { t } from '@/i18n'
import BaseModal from '@/components/overlay/BaseModal.vue'

const props = defineProps<{ row: AccountRow }>()
const emit = defineEmits<{ close: [] }>()

const { api } = useOps()
const session = useSessionStore()
const ui = useUiStore()

const want = computed(() => (props.row.role === 'admin' ? 'user' : 'admin'))

async function run(fail: (msg: string) => void, close: () => void): Promise<void> {
  try {
    await api('account.role', { account: props.row.account, role: want.value })
    close()
    ui.toast(t('${account} is now ${role}', { account: props.row.account, role: want.value }))
    if (props.row.account === session.account) await session.boot()
    else ui.reload()
  } catch (e) {
    fail(messageOf(e))
  }
}
</script>

<template>
  <BaseModal
    :title="'Make ${account} ${role}'"
    :vars="{ account: props.row.account, role: want }"
    :confirm="'Make ${role}'"
    :confirm-vars="{ role: want }"
    :confirm-variant="want === 'admin' ? 'primary' : 'danger'"
    @confirm="run"
    @close="emit('close')"
  >
    <p v-if="want === 'admin'">
      {{
        t(
          'An administrator governs every view, every account, and where a new base view is placed.',
        )
      }}
    </p>
    <p v-else>
      {{
        t(
          'A user governs only the base views it is delegated, plus their forks, aliases, lenses and keys.',
        )
      }}
    </p>
  </BaseModal>
</template>

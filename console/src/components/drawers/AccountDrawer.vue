<script setup lang="ts">
/*
  An account's whole record: what it is, what it governs, and which credentials
  still carry its name.

  The last block is the reason this pane exists. Deleting an account deliberately
  does NOT revoke its keys — the server says how many are still live when you do
  — so the one check an operator has to make before deleting is visible here
  rather than something the keys page has to be walked for. When the registry is
  not this account's to read, the pane says so instead of showing an empty table:
  "no keys" and "you cannot see them" are different facts.

  The reach count is derived, not stored: a delegation of a base view is a
  delegation of every lens, fork and alias that resolves through it, so "what it
  governs" is the grants plus the names hanging off them.
*/
import { computed, ref } from 'vue'
import type { AccountRow, GrantRow, KeyRow } from '@/types'
import { useCatalogStore } from '@/stores/catalog'
import { useUiStore } from '@/stores/ui'
import { useNav } from '@/composables/useNav'
import { t } from '@/i18n'
import BaseDrawer from '@/components/overlay/BaseDrawer.vue'
import DrawerSection from '@/components/overlay/DrawerSection.vue'
import KvList from '@/components/layout/KvList.vue'
import KvRow from '@/components/layout/KvRow.vue'
import MonoText from '@/components/ui/MonoText.vue'
import StateBadge from '@/components/ui/StateBadge.vue'
import CopyButton from '@/components/ui/CopyButton.vue'
import TimeCell from '@/components/ui/TimeCell.vue'
import MiniKeyTable from '@/components/keys/MiniKeyTable.vue'
import GrantTable from '@/components/accounts/GrantTable.vue'
import PasswdDialog from '@/components/dialogs/PasswdDialog.vue'

const props = defineProps<{
  row: AccountRow
  grants: GrantRow[]
  keys: KeyRow[] | null
  isSelf: boolean
}>()
const emit = defineEmits<{ close: [] }>()

const catalog = useCatalogStore()
const ui = useUiStore()
const { openKeysOf } = useNav()
const pwOpen = ref(false)

/** The credential registry is the keys page's job; hand over with the owner
    filter already applied instead of duplicating a second list here. */
function keysOfThisAccount(): void {
  ui.closeDrawer()
  void openKeysOf(props.row.account)
}

/** Every non-base name that resolves through one of this account's delegations. */
const reach = computed(() =>
  catalog.views.filter(
    (v) => v.kind !== 'base' && props.grants.some((g) => g.base_view === v.base),
  ),
)
</script>

<template>
  <BaseDrawer :title="props.row.account" mono @close="emit('close')">
    <template #badges>
      <StateBadge
        :label="props.row.role"
        :tone="props.row.role === 'admin' ? 'admin' : undefined"
        :title="
          props.row.role === 'admin'
            ? t('an administrator governs every view and every account')
            : t('a user governs only the base views it is delegated')
        "
      />
      <StateBadge
        :label="props.row.disabled_at ? t('disabled') : t('enabled')"
        :tone="props.row.disabled_at ? 'bad' : 'good'"
        :title="
          props.row.disabled_at ? t('disabled ${at}', { at: props.row.disabled_at }) : undefined
        "
      />
    </template>

    <DrawerSection :heading="'Account'">
      <KvList>
        <KvRow label="account">
          <span class="cell-badges">
            <MonoText :text="props.row.account" />
            <CopyButton :text="props.row.account" />
          </span>
        </KvRow>
        <KvRow label="role">
          {{
            props.row.role === 'admin'
              ? t('an administrator governs every view and every account')
              : t('a user governs only the base views it is delegated')
          }}
        </KvRow>
        <KvRow label="created"><TimeCell :ts="props.row.created_at" /></KvRow>
        <KvRow label="last sign-in">
          <TimeCell v-if="props.row.last_login_at" :ts="props.row.last_login_at" />
          <span v-else class="muted">{{ t('never') }}</span>
        </KvRow>
        <KvRow label="disabled">
          <TimeCell v-if="props.row.disabled_at" :ts="props.row.disabled_at" />
          <span v-else class="muted">{{ t('not disabled') }}</span>
        </KvRow>
      </KvList>
    </DrawerSection>

    <DrawerSection :heading="'What it governs'">
      <KvList>
        <KvRow label="delegated base views" :value="props.grants.length" />
        <KvRow label="names that reach through them" :value="reach.length" />
      </KvList>
      <GrantTable
        v-if="props.grants.length"
        :rows="props.grants"
        :empty="'no base view is delegated to this account'"
        :with-account="false"
        :with-actions="false"
      />
      <p v-else class="muted">
        {{
          t(
            'It governs no base view: a user with no delegation can sign in and change its own password, nothing else.',
          )
        }}
      </p>
    </DrawerSection>

    <DrawerSection :heading="'Credentials in its name'">
      <p v-if="props.keys === null" class="muted">
        {{ t('the key registry is an administrator’s view') }}
      </p>
      <MiniKeyTable
        v-else-if="props.keys.length"
        :rows="props.keys"
        :empty="'no keys you can see'"
      />
      <p v-else class="muted">{{ t('no keys in its name') }}</p>
      <p v-if="props.keys?.length">
        <button type="button" class="link" @click="keysOfThisAccount">
          {{ t('All keys in this account’s name…') }}
        </button>
      </p>
    </DrawerSection>

    <template v-if="props.isSelf" #left>{{ t('this is you') }}</template>
    <template #footer>
      <button type="button" @click="pwOpen = true">
        {{ props.isSelf ? t('My password') : t('Password') }}
      </button>
    </template>
  </BaseDrawer>

  <PasswdDialog v-if="pwOpen" :account="props.row.account" @close="pwOpen = false" />
</template>

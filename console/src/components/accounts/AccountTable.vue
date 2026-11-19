<script setup lang="ts">
/*
  The principal table: who can govern this deployment, and how far each of them
  reaches.

  Two of the columns exist so that a delete can be decided without a second
  lookup: the delegated bases (what the account governs) and its keys (what can
  still authenticate in its name — `account.delete` deliberately does NOT revoke
  those, so the count has to be visible before the verb, not after). Both are
  read up front by the page, because "3 keys" and "2 base views" as live numbers
  are two round trips per row otherwise.

  The verbs in the last column are the whole lifecycle: its password, its role,
  whether it may sign in at all, and the account itself. The row is also the
  record pane's trigger, so a click anywhere that is not a verb opens it.
*/
import { ref } from 'vue'
import type { Column } from '@/components/ui/table'
import type { AccountRow, GrantRow, KeyRow } from '@/types'
import { useOps } from '@/composables/useOps'
import { useSessionStore } from '@/stores/session'
import { useUiStore } from '@/stores/ui'
import { keyBucket } from '@/utils/keys'
import { shortTime } from '@/utils/time'
import { t } from '@/i18n'
import DataTable from '@/components/ui/DataTable.vue'
import WhoCell from '@/components/ui/WhoCell.vue'
import StateBadge from '@/components/ui/StateBadge.vue'
import CountPill from '@/components/ui/CountPill.vue'
import TimeCell from '@/components/ui/TimeCell.vue'
import ActionButton from '@/components/ui/ActionButton.vue'
import BasePills from './BasePills.vue'
import PasswdDialog from '@/components/dialogs/PasswdDialog.vue'
import RoleDialog from '@/components/dialogs/RoleDialog.vue'
import { useNav } from '@/composables/useNav'

const props = defineProps<{
  rows: AccountRow[]
  grants: GrantRow[]
  /** `null` is not "no keys": it is "the registry is not this account's to
      read", and the column says so with a dash instead of a zero. */
  keys: KeyRow[] | null
  empty?: string
}>()

const session = useSessionStore()
const ui = useUiStore()
const { api } = useOps()
const { openKeysOf } = useNav()

const COLS: Column<AccountRow>[] = [
  { id: 'account', label: 'account', cls: 'nowrap' },
  { id: 'role', label: 'role' },
  { id: 'bases', label: 'delegated bases' },
  { id: 'keys', label: 'keys', cls: 'nowrap' },
  { id: 'last', label: 'last sign-in', cls: 'nowrap' },
  { id: 'state', label: 'state' },
  { id: 'actions', label: '', cls: 'actions' },
]

const grantsOf = (name: string): GrantRow[] => props.grants.filter((g) => g.account === name)
const keysOf = (name: string): KeyRow[] | null =>
  props.keys === null ? null : props.keys.filter((k) => k.owner === name || k.created_by === name)

/** What the row's pill reports: a base view governs everything derived from it,
    so the delegation list is the truth and `account.list`'s own `bases` is the
    fallback for a row whose grants this account cannot read. */
function basesOf(a: AccountRow): string[] {
  const g = grantsOf(a.account).map((x) => x.base_view)
  return g.length ? g : a.bases || []
}

function liveOf(ks: KeyRow[]): number {
  return ks.filter((k) => keyBucket(k) === 'live' || keyBucket(k) === 'expiring').length
}

/** The pill's three answers, as functions that never return `null`: a template
    row may not cast, and "the registry is not readable" is already carried by
    `hasKeys`. */
const hasKeys = (a: AccountRow): boolean => keysOf(a.account) !== null
const keyAll = (a: AccountRow): number => keysOf(a.account)?.length ?? 0
const keyText = (a: AccountRow): string => {
  const ks = keysOf(a.account) ?? []
  return t('${all} key(s) · ${live} live', { all: ks.length, live: liveOf(ks) })
}

const passwdFor = ref<string | null>(null)
const roleFor = ref<AccountRow | null>(null)

const isActive = (a: AccountRow): boolean =>
  ui.drawer?.kind === 'account' && ui.drawer.row.account === a.account

function open(a: AccountRow): void {
  ui.openDrawer({
    kind: 'account',
    row: a,
    grants: grantsOf(a.account),
    keys: keysOf(a.account),
    isSelf: a.account === session.account,
  })
}

function showKeys(a: AccountRow): void {
  void openKeysOf(a.account)
}

async function toggleDisabled(a: AccountRow): Promise<void> {
  const want = !a.disabled_at
  if (
    !(await ui.ask(
      want ? 'Disable sign-in?' : 'Enable sign-in?',
      want
        ? '${account} could no longer sign in; its open sessions die on their next call.'
        : '${account} may sign in again.',
      { account: a.account },
      want ? 'Disable' : 'Enable',
    ))
  )
    return
  await api('account.disable', { account: a.account, disabled: want })
  ui.toast(
    want
      ? t('${account} disabled', { account: a.account })
      : t('${account} enabled', { account: a.account }),
  )
  ui.reload()
}

async function remove(a: AccountRow): Promise<void> {
  if (
    !(await ui.ask(
      'Delete this account?',
      '${account} — its delegations go with it.\nIts api keys DO NOT; the server reports how many are still live.',
      { account: a.account },
      'Delete',
    ))
  )
    return
  const res = await api('account.delete', { account: a.account })
  ui.toast(t('${account}: ${note}', { account: res.account, note: res.note }))
  ui.reload()
}
</script>

<template>
  <DataTable
    :cols="COLS"
    :rows="props.rows"
    :empty="props.empty"
    :key-of="(a: AccountRow) => a.account"
    clickable
    :is-active="isActive"
    @open="open"
  >
    <template #cell-account="{ row }">
      <WhoCell
        :name="row.account"
        :sub="row.created_at ? t('joined ${when}', { when: shortTime(row.created_at) }) : null"
        :off="!!row.disabled_at"
      />
    </template>

    <template #cell-role="{ row }">
      <StateBadge
        :label="row.role"
        :tone="row.role === 'admin' ? 'admin' : undefined"
        :title="
          row.role === 'admin'
            ? t('an administrator governs every view and every account')
            : t('a user governs only the base views it is delegated')
        "
      />
    </template>

    <template #cell-bases="{ row }"><BasePills :bases="basesOf(row)" /></template>

    <template #cell-keys="{ row }">
      <span v-if="!hasKeys(row)" class="muted">—</span>
      <CountPill
        v-else
        clickable
        :on="keyAll(row) > 0"
        :text="keyText(row)"
        :title="t('show this account’s keys on the keys page')"
        @click="showKeys(row)"
      />
    </template>

    <template #cell-last="{ row }">
      <TimeCell v-if="row.last_login_at" :ts="row.last_login_at" />
      <span v-else class="muted">{{ t('never') }}</span>
    </template>

    <template #cell-state="{ row }">
      <StateBadge
        :label="row.disabled_at ? t('disabled') : t('enabled')"
        :tone="row.disabled_at ? 'bad' : 'good'"
        :title="row.disabled_at ? t('disabled ${at}', { at: row.disabled_at }) : undefined"
      />
    </template>

    <template #cell-actions="{ row }">
      <div class="row">
        <button type="button" class="mini" @click="passwdFor = row.account">
          {{ row.account === session.account ? t('My password') : t('Password') }}
        </button>
        <button v-if="session.isSuper" type="button" class="mini" @click="roleFor = row">
          {{ row.role === 'admin' ? t('Make user') : t('Make admin') }}
        </button>
        <ActionButton
          v-if="session.isSuper && row.account !== session.account"
          :label="row.disabled_at ? 'Enable' : 'Disable'"
          :variant="row.disabled_at ? 'mini' : 'mini danger'"
          :action="() => toggleDisabled(row)"
        />
        <ActionButton
          v-if="session.isSuper && row.account !== session.account"
          :label="'Delete'"
          variant="mini danger"
          :action="() => remove(row)"
        />
      </div>
    </template>
  </DataTable>

  <PasswdDialog v-if="passwdFor" :account="passwdFor" @close="passwdFor = null" />
  <RoleDialog v-if="roleFor" :row="roleFor" @close="roleFor = null" />
</template>

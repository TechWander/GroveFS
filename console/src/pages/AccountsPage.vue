<script setup lang="ts">
/*
  Who governs what: the principals, their roles, and the delegations that give a
  role its reach.

  Grants and keys are read UP FRONT, not on demand, because two of the table's
  columns are those lists reduced to a number — and "can this account be deleted
  safely" is answered by the keys column (deleting a principal deliberately does
  NOT revoke what it minted). The drawer then shows the rows behind both numbers,
  so the click that checks is the same one that explains.

  An account that may not read the list gets its own record instead of a refusal
  page (`SelfAccountPanel`), and the delegation block below belongs to an
  administrator, so a `user` sees the two things that are its: its password, and
  the bases it governs.
*/
import { computed, ref } from 'vue'
import type { AccountRow, GrantRow, KeyRow } from '@/types'
import { useOps } from '@/composables/useOps'
import { usePageLoad } from '@/composables/usePageLoad'
import { useCatalogStore } from '@/stores/catalog'
import { useFilterStore } from '@/stores/filters'
import { useSessionStore } from '@/stores/session'
import { useUiStore } from '@/stores/ui'
import type { ChipItem } from '@/components/ui/chips'
import { counted } from '@/utils/text'
import { t } from '@/i18n'
import PageHead from '@/components/layout/PageHead.vue'
import BarRow from '@/components/layout/BarRow.vue'
import FldRow from '@/components/layout/FldRow.vue'
import FilterBar from '@/components/ui/FilterBar.vue'
import ChipGroup from '@/components/ui/ChipGroup.vue'
import StateBadge from '@/components/ui/StateBadge.vue'
import ActionButton from '@/components/ui/ActionButton.vue'
import LoadingHint from '@/components/ui/LoadingHint.vue'
import AccountTable from '@/components/accounts/AccountTable.vue'
import GrantTable from '@/components/accounts/GrantTable.vue'
import CreateAccountPanel from '@/components/accounts/CreateAccountPanel.vue'
import GrantPanel from '@/components/accounts/GrantPanel.vue'
import SelfAccountPanel from '@/components/accounts/SelfAccountPanel.vue'

const catalog = useCatalogStore()
const filters = useFilterStore()
const session = useSessionStore()
const ui = useUiStore()
const { soft } = useOps()

const accounts = ref<AccountRow[]>([])
const grants = ref<GrantRow[]>([])
const keys = ref<KeyRow[] | null>(null)
/** Set when `account.list` refused: an administrator's op, so a `user` lands in
    the "your account" branch, and the server's sentence is shown there. */
const refused = ref('')

async function load(): Promise<void> {
  await catalog.loadViews()
  const res = await soft('account.list', {})
  if (!res.ok) {
    refused.value = res.error
    accounts.value = []
    grants.value = []
    keys.value = null
    return
  }
  refused.value = ''
  accounts.value = res.r ?? []
  const [g, k] = await Promise.all([soft('grant.list', {}), soft('apikey.list', {})])
  grants.value = g.ok ? (g.r ?? []) : []
  keys.value = k.ok ? (k.r ?? []) : null
}

const { pending, error } = usePageLoad(load)

function refresh(): void {
  ui.reload()
}

const grantsOf = (name: string): GrantRow[] => grants.value.filter((g) => g.account === name)

const stateOf = (a: AccountRow): string => (a.disabled_at ? 'disabled' : 'enabled')

const roleChips = computed<ChipItem[]>(() =>
  (['all', 'admin', 'user'] as const).map((v) => ({
    v,
    t: v === 'all' ? 'all roles' : v,
    n: v === 'all' ? accounts.value.length : accounts.value.filter((a) => a.role === v).length,
  })),
)

const stateChips = computed<ChipItem[]>(() =>
  (['all', 'enabled', 'disabled'] as const).map((v) => ({
    v,
    t: v === 'all' ? 'any state' : v,
    n: v === 'all' ? accounts.value.length : accounts.value.filter((a) => stateOf(a) === v).length,
  })),
)

const hit = computed(() => {
  const f = filters.accFilter
  const q = f.q.trim().toLowerCase()
  return accounts.value.filter((a) => {
    if (f.role !== 'all' && a.role !== f.role) return false
    if (f.state !== 'all' && stateOf(a) !== f.state) return false
    if (q) {
      const hay = [
        a.account,
        a.role,
        ...(a.bases || []),
        ...grantsOf(a.account).map((g) => g.base_view),
      ]
      if (!hay.some((x) => x && String(x).toLowerCase().includes(q))) return false
    }
    return true
  })
})

/** The delegation list has its own box, and it is deliberately NOT remembered
    between reloads: it is a lookup while reading one row, not a working set. */
const grantQuery = ref('')

const grantHit = computed(() => {
  const q = grantQuery.value.trim().toLowerCase()
  if (!q) return grants.value
  return grants.value.filter((g) =>
    [g.account, g.base_view, g.perm, g.granted_by].some(
      (x) => x && String(x).toLowerCase().includes(q),
    ),
  )
})
</script>

<template>
  <PageHead title="Accounts" desc="Who governs what — accounts, roles and base-view delegations.">
    <template #actions>
      <StateBadge v-if="!session.isSuper" tone="off" :label="t('administrators act here')" />
      <ActionButton :action="refresh" :label="'refresh'" variant="link" />
    </template>
  </PageHead>

  <LoadingHint v-if="pending && !accounts.length && !refused" />
  <p v-else-if="error" class="err">{{ error }}</p>

  <SelfAccountPanel v-else-if="refused" :error="refused" />

  <template v-else>
    <CreateAccountPanel v-if="session.isSuper" />

    <FilterBar :count="counted(hit.length, accounts.length)">
      <template #search>
        <input
          v-model="filters.accFilter.q"
          type="search"
          :placeholder="t('filter by account, role or delegated view…')"
          :aria-label="t('filter by account, role or delegated view…')"
        />
      </template>
      <template #opts>
        <FldRow :label="'role'">
          <ChipGroup v-model="filters.accFilter.role" :items="roleChips" />
        </FldRow>
        <FldRow :label="'state'">
          <ChipGroup v-model="filters.accFilter.state" :items="stateChips" />
        </FldRow>
      </template>
    </FilterBar>

    <AccountTable
      :rows="hit"
      :grants="grants"
      :keys="keys"
      :empty="accounts.length ? 'no accounts match the filter' : 'no accounts'"
    />

    <BarRow :title="'Delegations'">
      <span class="muted">
        {{ t('an account governs a base view AND every lens, fork and alias derived from it') }}
      </span>
    </BarRow>

    <GrantPanel v-if="session.isSuper" :accounts="accounts" />

    <FilterBar>
      <template #search>
        <input
          v-model="grantQuery"
          type="search"
          :placeholder="t('filter by account or base view…')"
          :aria-label="t('filter by account or base view…')"
        />
      </template>
    </FilterBar>

    <GrantTable
      :rows="grantHit"
      :empty="
        grants.length
          ? 'no delegations match the filter'
          : 'no base view is delegated to anyone yet'
      "
    />
  </template>
</template>

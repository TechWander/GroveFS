<script setup lang="ts">
/*
  One delegation, read in two places.

  The accounts page shows the whole deployment's list (whose account, which
  base, who granted it, and the verb that takes it back); an account's drawer
  shows the same rows without the account column and without the verb, because
  there the question is "what does this principal reach", not "what do I want
  to change". One column set for both, so a delegation cannot mean two things.

  Withdrawing is the one verb here that changes what someone ELSE can do, so it
  says whose access ends and on which base, in the prompt, before it asks.
*/
import { computed } from 'vue'
import type { Column } from '@/components/ui/table'
import type { GrantRow } from '@/types'
import { useOps } from '@/composables/useOps'
import { useNav } from '@/composables/useNav'
import { useUiStore } from '@/stores/ui'
import { t } from '@/i18n'
import DataTable from '@/components/ui/DataTable.vue'
import TwoLineCell from '@/components/ui/TwoLineCell.vue'
import WhoCell from '@/components/ui/WhoCell.vue'
import TimeCell from '@/components/ui/TimeCell.vue'
import ActionButton from '@/components/ui/ActionButton.vue'

const props = withDefaults(
  defineProps<{
    rows: GrantRow[]
    /** Required, like the other tables: the page that shows an empty delegation
        list has to say whether it is empty by filter or empty in fact. */
    empty: string
    withAccount?: boolean
    withActions?: boolean
  }>(),
  { withAccount: true, withActions: true },
)

const { api } = useOps()
const { openView } = useNav()
const ui = useUiStore()

const COLS: Column<GrantRow>[] = [
  { id: 'account', label: 'account', cls: 'nowrap' },
  { id: 'base', label: 'base view' },
  { id: 'perm', label: 'perm', field: 'perm', cls: 'mono' },
  { id: 'granted', label: 'granted', cls: 'muted nowrap' },
  { id: 'actions', label: '', cls: 'actions' },
]

const cols = computed(() =>
  COLS.filter(
    (c) => (c.id !== 'account' || props.withAccount) && (c.id !== 'actions' || props.withActions),
  ),
)

/** A base view's name is a link: the next question is what that name is. */
function jump(name: string): void {
  void openView(name)
}

async function withdraw(g: GrantRow): Promise<void> {
  if (
    !(await ui.ask(
      'Withdraw this delegation?',
      '${account} stops governing ${base} and everything derived from it, on its next call.',
      { account: g.account, base: g.base_view },
      'Withdraw',
    ))
  )
    return
  await api('grant.remove', { account: g.account, base: g.base_view })
  ui.toast(t('withdrawn'))
  ui.reload()
}
</script>

<template>
  <DataTable
    :cols="cols"
    :rows="props.rows"
    :empty="props.empty"
    :key-of="(g: GrantRow) => g.account + '/' + g.base_view"
  >
    <template #cell-account="{ row }"><WhoCell :name="row.account" /></template>

    <!-- A base view's name is a link: the next question is what that name is. -->
    <template #cell-base="{ row }">
      <button
        type="button"
        class="mini mono"
        :title="t('open this base view')"
        @click="jump(row.base_view)"
      >
        {{ row.base_view }}
      </button>
    </template>

    <template #cell-granted="{ row }">
      <TwoLineCell :sub="row.granted_by ? t('by ${by}', { by: row.granted_by }) : null">
        <template #top><TimeCell :ts="row.granted_at" /></template>
      </TwoLineCell>
    </template>

    <template #cell-actions="{ row }">
      <ActionButton :label="'Withdraw'" variant="mini danger" :action="() => withdraw(row)" />
    </template>
  </DataTable>
</template>

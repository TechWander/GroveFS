<script setup lang="ts">
/*
  The key table's columns, in the order an operator reads them.

  What it is called and what it is for; which credential it IS (a fingerprint,
  never a secret); what it can open; whose it is; whether it still works; when it
  was last seen; then the verbs.

  The views page reuses this exact list with `scoped-to-view`, because there the
  binding would only repeat the page's own heading — and because a key must not
  look different depending on which page found it. That is the whole reason the
  columns live here instead of in the keys page.
*/
import { computed } from 'vue'
import type { Column } from '@/components/ui/table'
import type { KeyRow } from '@/types'
import { useNav } from '@/composables/useNav'
import { useUiStore } from '@/stores/ui'
import { expiryLine, keyLabel, keyTone, keyWhy } from '@/utils/keys'
import { t } from '@/i18n'
import DataTable from '@/components/ui/DataTable.vue'
import TwoLineCell from '@/components/ui/TwoLineCell.vue'
import MaskText from '@/components/ui/MaskText.vue'
import MonoText from '@/components/ui/MonoText.vue'
import TimeCell from '@/components/ui/TimeCell.vue'
import WhoCell from '@/components/ui/WhoCell.vue'
import StateBadge from '@/components/ui/StateBadge.vue'
import KeyRowActions from './KeyRowActions.vue'

const props = withDefaults(
  defineProps<{
    rows: KeyRow[]
    /** Required, not optional: "no keys" and "no keys match the filter" are
        different answers, and only the page knows which one it is showing. */
    empty: string
    /** Inside a view's page, the binding column says nothing new. */
    scopedToView?: boolean
    /** Only the keys page opens the record pane; a view's list is a lookup. */
    openable?: boolean
  }>(),
  { scopedToView: false, openable: false },
)

const ui = useUiStore()
const { openView, openAccountsOf } = useNav()

const COLS: Column<KeyRow>[] = [
  { id: 'label', label: 'label', cls: 'nowrap' },
  { id: 'key', label: 'key' },
  { id: 'note', label: 'note', cls: 'wrap' },
  { id: 'plane', label: 'plane', field: 'scope', cls: 'mono' },
  { id: 'view', label: 'view', cls: 'nowrap' },
  { id: 'mode', label: 'mode', cls: 'nowrap' },
  { id: 'owner', label: 'owner' },
  { id: 'state', label: 'state', cls: 'nowrap' },
  { id: 'last', label: 'last used', cls: 'muted nowrap' },
  { id: 'actions', label: '', cls: 'actions' },
]

const cols = computed(() => (props.scopedToView ? COLS.filter((c) => c.id !== 'view') : COLS))

const isActive = (r: KeyRow): boolean => ui.drawer?.kind === 'key' && ui.drawer.row.id === r.id

const open = (r: KeyRow): void => ui.openDrawer({ kind: 'key', row: r })
</script>

<template>
  <DataTable
    :cols="cols"
    :rows="props.rows"
    :empty="props.empty"
    :key-of="(r: KeyRow) => r.id"
    :clickable="props.openable"
    :is-active="props.openable ? isActive : undefined"
    @open="open"
  >
    <template #cell-label="{ row }">
      <TwoLineCell>
        <template #top>
          <span class="nm">{{ row.label }}</span>
        </template>
        <template #sub><MonoText :text="row.id" /></template>
      </TwoLineCell>
    </template>

    <template #cell-key="{ row }"><MaskText :hint="row.secret_hint" /></template>

    <template #cell-note="{ row }">
      <span v-if="row.note">{{ row.note }}</span>
      <span v-else class="muted">{{ t('no note') }}</span>
    </template>

    <template #cell-view="{ row }">
      <!-- A bound view is a link everywhere it appears: "which tree does this
           key open" is answered by looking at the tree. -->
      <button
        v-if="row.view"
        type="button"
        class="mini mono"
        :title="t('open this view')"
        @click.stop="row.view && openView(row.view)"
      >
        {{ row.view }}
      </button>
      <span v-else class="muted">{{ t('any view') }}</span>
    </template>

    <template #cell-mode="{ row }">
      <StateBadge
        :label="row.ro ? t('read-only') : t('read-write')"
        :tone="row.ro ? 'warn' : undefined"
        :title="row.ro ? t('a mount with it cannot write') : t('it can write')"
      />
    </template>

    <template #cell-owner="{ row }">
      <button
        type="button"
        class="linky"
        :title="t('open this account')"
        @click.stop="row.owner && openAccountsOf(row.owner)"
      >
        <WhoCell
          :name="row.owner"
          :sub="row.created_by ? t('minted by ${who}', { who: row.created_by }) : null"
          :off="!!row.revoked_at"
        />
      </button>
    </template>

    <template #cell-state="{ row }">
      <TwoLineCell :sub="expiryLine(row)">
        <template #top>
          <StateBadge :label="keyLabel(row)" :tone="keyTone(row)" :title="keyWhy(row)" />
        </template>
      </TwoLineCell>
    </template>

    <template #cell-last="{ row }"><TimeCell :ts="row.last_used_at" /></template>

    <template #cell-actions="{ row }"><KeyRowActions :row="row" /></template>
  </DataTable>
</template>

<script setup lang="ts">
/*
  The four columns an account's OWN key list uses inside a drawer.

  Four and not ten because a 34rem pane cannot show ten, and the other six are
  one click away on the keys page — which is why the header row here has no
  verbs either. This table is a check ("does this account still have a live
  credential before I delete it"), not a workspace.
*/
import type { Column } from '@/components/ui/table'
import type { KeyRow } from '@/types'
import { useNav } from '@/composables/useNav'
import { keyLabel, keyTone, keyWhy } from '@/utils/keys'
import { t } from '@/i18n'
import DataTable from '@/components/ui/DataTable.vue'
import TwoLineCell from '@/components/ui/TwoLineCell.vue'
import StateBadge from '@/components/ui/StateBadge.vue'

const props = defineProps<{ rows: KeyRow[]; empty?: string }>()

const { openView } = useNav()

const COLS: Column<KeyRow>[] = [
  { id: 'label', label: 'label' },
  { id: 'plane', label: 'plane', field: 'scope', cls: 'mono' },
  { id: 'view', label: 'view' },
  { id: 'state', label: 'state' },
]
</script>

<template>
  <DataTable :cols="COLS" :rows="props.rows" :empty="props.empty" :key-of="(r: KeyRow) => r.id">
    <template #cell-label="{ row }">
      <TwoLineCell :top="row.label" :sub="row.id" sub-mono />
    </template>
    <template #cell-view="{ row }">
      <button
        v-if="row.view"
        type="button"
        class="mini mono"
        :title="t('open this view')"
        @click="openView(String(row.view))"
      >
        {{ row.view }}
      </button>
      <span v-else class="muted">{{ t('any view') }}</span>
    </template>
    <template #cell-state="{ row }">
      <StateBadge :label="keyLabel(row)" :tone="keyTone(row)" :title="keyWhy(row)" />
    </template>
  </DataTable>
</template>

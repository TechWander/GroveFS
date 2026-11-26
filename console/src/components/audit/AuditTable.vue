<script setup lang="ts">
/*
  The governance log, as one component.

  It appears three times — the overview's "recent calls" block, the audit page
  after its filters, and the last eight rows inside a key's drawer — and it is the
  same six columns and the same row click everywhere. That is the reason it is a
  component rather than markup copied between pages: an audit row that reads
  differently depending on which page found it is a row people mis-read.

  A row opens the record's drawer, which is where the server's whole message
  lives: the table clips `detail` to one line on purpose, because the column that
  explains a failure must not be allowed to make forty rows four hundred lines
  tall.
*/
import type { Column } from '@/components/ui/table'
import type { AuditRow } from '@/types'
import { useUiStore } from '@/stores/ui'
import { useNav } from '@/composables/useNav'
import { auditActor } from '@/utils/audit'
import { clip } from '@/utils/text'
import { t } from '@/i18n'
import DataTable from '@/components/ui/DataTable.vue'
import TimeCell from '@/components/ui/TimeCell.vue'
import TwoLineCell from '@/components/ui/TwoLineCell.vue'
import StateBadge from '@/components/ui/StateBadge.vue'

defineProps<{ rows: AuditRow[]; empty?: string }>()

const ui = useUiStore()
const { goTo } = useNav()

const COLS: Column<AuditRow>[] = [
  { id: 'when', label: 'when', cls: 'mono nowrap' },
  { id: 'who', label: 'who', cls: 'nowrap' },
  { id: 'plane', label: 'plane', field: 'method', cls: 'mono' },
  { id: 'op', label: 'op', field: 'tool', cls: 'mono' },
  { id: 'result', label: 'result', cls: 'nowrap' },
  { id: 'detail', label: 'detail', cls: 'wrap' },
]

const keyOf = (r: AuditRow): string => `${r.ts}|${r.tool ?? ''}|${r.key_id ?? ''}|${r.detail ?? ''}`

const open = (r: AuditRow): void => ui.openDrawer({ kind: 'audit', row: r })
</script>

<template>
  <DataTable :cols="COLS" :rows="rows" :empty="empty" :key-of="keyOf" clickable @open="open">
    <template #cell-when="{ row }">
      <TimeCell :ts="row.ts" />
    </template>
    <template #cell-who="{ row }">
      <!-- The actor and the credential are both doors: the log answers "what
           happened", and the next question is "what is that key / account". -->
      <TwoLineCell v-if="auditActor(row)">
        <template #top>
          <button
            type="button"
            class="linky"
            :title="t('open this account')"
            @click.stop="goTo('accounts', { q: auditActor(row) ?? '' })"
          >
            {{ auditActor(row) }}
          </button>
        </template>
        <template v-if="row.key_id" #sub>
          <button
            type="button"
            class="linky mono"
            :title="t('open this key')"
            @click.stop="goTo('keys', { q: row.key_id ?? '' })"
          >
            {{ t('key ${id}', { id: row.key_id }) }}
          </button>
        </template>
      </TwoLineCell>
      <span v-else class="muted">{{ t('no account on it') }}</span>
    </template>
    <template #cell-result="{ row }">
      <StateBadge
        :label="row.ok ? t('ok') : t('failed')"
        :tone="row.ok ? 'good' : 'bad'"
        :title="row.ok ? undefined : clip(row.detail, 400)"
      />
    </template>
    <template #cell-detail="{ row }">{{ clip(row.detail, 110) }}</template>
  </DataTable>
</template>

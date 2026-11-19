<script setup lang="ts" generic="T">
/*
  The one table.

  A row is a target: clicking anywhere on it opens the record, while a click
  that STARTS on a control inside the row still means that control's verb — the
  guard below is the whole trick, and it is the reason the row does not have to
  stop being clickable just because it holds a button.

  Everything reaches the `<td>` as TEXT or as a component's own nodes. There is
  no `v-html` in this project (and `vue/no-v-html` is an error-level lint rule),
  which is how the old "a view name cannot inject markup" property survives the
  rewrite.
*/
import type { Column } from './table'
import { t } from '@/i18n'

const props = defineProps<{
  cols: Column<T>[]
  rows: T[]
  /** The empty state's sentence: "no keys" and "no keys match the filter" are
      different facts and the page knows which one it is showing. */
  empty?: string
  clickable?: boolean
  /** The row whose drawer is open, so the pane and the list agree. */
  isActive?: (row: T) => boolean
  keyOf?: (row: T) => string
}>()

const emit = defineEmits<{ open: [row: T] }>()

const CONTROLS = 'button, a, select, input, textarea, label'

function openOn(ev: MouseEvent, row: T) {
  if (!props.clickable) return
  const target = ev.target as Element | null
  if (target?.closest?.(CONTROLS)) return
  emit('open', row)
}

function openByKey(ev: KeyboardEvent, row: T) {
  if (!props.clickable) return
  if (ev.key === 'Enter' || ev.key === ' ') {
    ev.preventDefault()
    emit('open', row)
  }
}

/** A column that only shows a field: `null`/`undefined` becomes the em dash the
    old tables used, so an absent value never reads as an empty cell. `false` is
    included because the old `tableNode` treated it the same way — a boolean
    column that renders "false" in nine rows out of ten is noise, not data. */
function plain(row: T, col: Column<T>): string {
  if (!col.field) return ''
  const v = (row as Record<string, unknown>)[col.field as string]
  if (v === null || v === undefined || v === false || v === '') return '—'
  /* Every `field` column declared in this console points at a scalar, and only
     a scalar is meaningful in a cell: the old table would have printed
     `[object Object]` for anything else, which is noise a reader cannot act
     on. Saying so here keeps `String()` provably harmless. */
  return typeof v === 'string' || typeof v === 'number' || typeof v === 'bigint' ? String(v) : '—'
}
</script>

<template>
  <div class="tbl-wrap">
    <table>
      <thead>
        <tr>
          <th v-for="col in cols" :key="col.id" scope="col">{{ t(col.label) }}</th>
        </tr>
      </thead>
      <tbody>
        <tr v-if="!rows.length">
          <td :colspan="cols.length" class="empty">
            <svg
              viewBox="0 0 24 24"
              width="26"
              height="26"
              fill="none"
              stroke="currentColor"
              stroke-width="1.5"
              stroke-linejoin="round"
              aria-hidden="true"
            >
              <path d="M5 6h14l3 7v6a1 1 0 0 1-1 1H3a1 1 0 0 1-1-1v-6l3-7z" />
              <path d="M2 13h5l1.5 2.5h7L17 13h5" />
            </svg>
            <div>{{ t(props.empty || 'nothing here') }}</div>
          </td>
        </tr>
        <tr
          v-for="(row, i) in rows"
          :key="props.keyOf ? props.keyOf(row) : i"
          :class="{ click: props.clickable, cur: props.isActive?.(row) }"
          :tabindex="props.clickable ? 0 : undefined"
          :role="props.clickable ? 'button' : undefined"
          @click="openOn($event, row)"
          @keydown="openByKey($event, row)"
        >
          <td v-for="col in cols" :key="col.id" :class="col.cls">
            <!-- One named slot per column (`#cell-<id>`), so a table declares
                 only the cells that are more than a field, and the ones that are
                 fall through to `plain()` below. A single generic slot could not:
                 providing it would silence the fallback for every column. -->
            <slot :name="'cell-' + col.id" :row="row" :col="col" :index="i">
              {{ plain(row, col) }}
            </slot>
          </td>
        </tr>
      </tbody>
    </table>
  </div>
</template>

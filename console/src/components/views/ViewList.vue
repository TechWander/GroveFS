<script setup lang="ts">
/*
  The namespace picker: a name box, one chip per kind, and the list itself.

  Filtering is CLIENT-side and re-applied on every keystroke — the namespace is
  already loaded (the keys and accounts pages need it too), and a picker that
  round-trips per character is slower than the thing it is filtering. The count
  beside the heading is the live one, so "no views match the filter" is visible
  before you scroll.

  A base view's badge counts the names that hang DIRECTLY below it: the number an
  operator needs before touching or deleting anything. The selected row carries
  `aria-current`, and the whole row is a button — the hand-written list had a
  click handler on an `<li>` and no way to reach it from the keyboard.
*/
import { computed } from 'vue'
import { useCatalogStore } from '@/stores/catalog'
import { useFilterStore } from '@/stores/filters'
import { useNav } from '@/composables/useNav'
import { KIND_CHIPS } from '@/components/ui/chips'
import { counted } from '@/utils/text'
import { kindName, viewChildren } from '@/utils/views'
import { t } from '@/i18n'
import BarRow from '@/components/layout/BarRow.vue'
import CountPill from '@/components/ui/CountPill.vue'
import ChipGroup from '@/components/ui/ChipGroup.vue'
import NewBasePanel from './NewBasePanel.vue'

const catalog = useCatalogStore()
const filters = useFilterStore()
const { pickView } = useNav()

const total = computed(() => catalog.views.length)

const rows = computed(() => {
  const q = filters.vFilter.q.trim().toLowerCase()
  return catalog.views.filter(
    (v) =>
      (filters.vFilter.kind === 'all' || v.kind === filters.vFilter.kind) &&
      (filters.vFilter.instance === 'all' ||
        catalog.instanceOf(v.name) === filters.vFilter.instance) &&
      (!q || v.name.toLowerCase().includes(q)),
  )
})

/* The instance chips exist only where a choice exists: one logical instance is
   a fact about the row, not a filter the operator has to set. */
const instanceChips = computed(() => {
  const names = catalog.storage?.instances.map((i) => i.name) ?? []
  return [
    { v: 'all', t: 'all instances', n: catalog.views.length },
    ...names.map((name) => ({
      v: name,
      t: name,
      n: catalog.views.filter((v) => catalog.instanceOf(v.name) === name).length,
    })),
  ]
})

const below = (name: string, kind: string): number =>
  kind === 'base' ? viewChildren(catalog.views, name).length : 0
</script>

<template>
  <div class="col-narrow">
    <BarRow :title="'View list'">
      <CountPill id="view-count" :text="counted(rows.length, total)" />
    </BarRow>

    <div class="filterbox">
      <input
        v-model="filters.vFilter.q"
        type="search"
        :placeholder="t('filter by name…')"
        :aria-label="t('filter by name…')"
      />
      <ChipGroup v-model="filters.vFilter.kind" :items="KIND_CHIPS" />
      <ChipGroup
        v-if="(catalog.storage?.instances.length ?? 0) > 1"
        v-model="filters.vFilter.instance"
        :items="instanceChips"
      />
    </div>

    <ul id="view-list" class="list">
      <li v-if="!total" class="static muted">{{ t('no views you can see') }}</li>
      <li v-else-if="!rows.length" class="static muted">{{ t('no views match the filter') }}</li>
      <li
        v-for="v in rows"
        :key="v.name"
        :class="[{ sel: catalog.sel === v.name }, 'k-' + v.kind]"
        role="button"
        tabindex="0"
        :aria-current="catalog.sel === v.name ? 'true' : undefined"
        @click="pickView(v.name)"
        @keydown="
          (ev) => {
            if (ev.key === 'Enter' || ev.key === ' ') {
              ev.preventDefault()
              pickView(v.name)
            }
          }
        "
      >
        <span class="mono">{{ v.name }}</span>
        <CountPill
          v-if="below(v.name, v.kind)"
          :text="String(below(v.name, v.kind))"
          :title="t('names derived directly from it')"
        />
        <span v-if="catalog.storage" class="inst" :title="t('metadata instance hosting this view')">
          {{ catalog.instanceOf(v.name) ?? 'hash' }}
        </span>
        <span class="kind">{{ kindName(v.kind) }}</span>
      </li>
    </ul>

    <NewBasePanel />
  </div>
</template>

<script setup lang="ts">
/*
  The lineage block of the detail pane: this view, its parents above it and
  everything derived from it below.

  The descendants are rendered as SIBLINGS, never as nested components: one
  `<li>` per name at one indentation level, whatever the derivation chain. The
  chain is still visible — the `via` note names the immediate definition edge
  (a lens over a lens says `via team1`) — but it is an annotation on a sibling
  row, not a nested `<ul>`. So `main`, `team1` and `team1view` sit side by side
  in the DOM as well as on screen.

  Two guardrails from the old tree survive the flattening:
  · a shared `seen` set, so a name reached by two edges is listed once and a
    hand-written cycle cannot loop;
  · for a base, `view.stats.family` is the server's authoritative list of names
    that resolve through it, so a definition the client merge has not seen still
    appears (with the neutral `definition` kind).
*/
import { computed } from 'vue'
import type { ViewStats, ViewRow } from '@/types'
import { useCatalogStore } from '@/stores/catalog'
import { useNav } from '@/composables/useNav'
import { kindName, viewAncestry, viewDescendants, type Descendant } from '@/utils/views'
import { t } from '@/i18n'

const props = defineProps<{ stats: ViewStats }>()

const catalog = useCatalogStore()
const { pickView } = useNav()

const up = computed(() => viewAncestry(catalog.views, props.stats.name))

const descendants = computed<Descendant[]>(() => {
  const found = viewDescendants(catalog.views, props.stats.name)
  const byName = new Map(found.map((d) => [d.view.name, d]))
  const rows: Descendant[] = []
  const family = props.stats.kind === 'base' ? props.stats.family : undefined

  if (family?.length) {
    for (const name of family) {
      const known = byName.get(name)
      rows.push(known ?? { view: { name, kind: 'other', base: null }, via: null })
    }
  } else {
    rows.push(...found)
  }
  // Direct children first, then the indirect ones; alphabetical inside each
  // group. The order is the only hierarchy the list shows.
  return rows.sort(
    (a, b) => Number(!!a.via) - Number(!!b.via) || a.view.name.localeCompare(b.view.name),
  )
})

const note = computed(() => {
  const n = descendants.value.length
  if (!n) return t('nothing is derived from it yet')
  if (props.stats.kind === 'base')
    return t('${n} name(s) resolve through this base — listed side by side; “via” names the edge', {
      n,
    })
  return t('${n} name(s) hang below it — listed side by side', { n })
})

const rowView = (d: Descendant): ViewRow => d.view
</script>

<template>
  <div class="lineage">
    <div v-if="up.length" class="cell-badges lineage-up">
      <span class="muted">{{ t('resolves through') }}</span>
      <span aria-hidden="true">←</span>
      <button v-for="n in up" :key="n" type="button" class="mini mono" @click="pickView(n)">
        {{ n }}
      </button>
    </div>

    <div class="lineage-head">
      <span class="mono">{{ t('derived names') }}</span>
      <span class="muted">{{ t('${n} total', { n: descendants.length }) }}</span>
    </div>

    <!-- One level, one `<li>` each: the selected view is not a wrapper around
         its descendants, so there is no lineage nesting to misread. -->
    <div class="lineage-root" :class="'k-' + props.stats.kind">
      <span class="t-name t-cur">{{ props.stats.name }}</span>
      <span class="t-meta">{{ kindName(props.stats.kind) }}</span>
    </div>

    <ul class="lineage-list">
      <li
        v-for="d in descendants"
        :key="d.view.name"
        :class="['lineage-row', 'k-' + d.view.kind]"
      >
        <button type="button" class="lineage-name mono" @click="pickView(rowView(d).name)">
          {{ d.view.name }}
        </button>
        <span class="t-meta">{{ kindName(d.view.kind) }}</span>
        <span v-if="d.via" class="lineage-via">
          {{ t('via ${v}', { v: d.via }) }}
        </span>
      </li>
      <li v-if="!descendants.length" class="lineage-row muted">
        {{ t('nothing is derived from it yet') }}
      </li>
    </ul>

    <p class="muted">{{ note }}</p>
  </div>
</template>

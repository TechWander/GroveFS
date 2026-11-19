<script setup lang="ts">
/*
  The lineage block of the detail pane: this view, its parents above it and
  everything derived from it below.

  The point of the panel is to answer "what breaks if I move this base" as a
  shape on screen instead of as a mental model of the namespace, so the ancestry
  is one row of buttons (each jumps) and the descent is a tree (each name jumps
  too). The footnote counts are two DIFFERENT numbers and say so: a base's
  `family` is the server's authoritative list of everything that resolves through
  it, while what is drawn below is the tree this page could build — capped at
  `TREE_DEPTH`, which is why the sentence names the direct children separately.
*/
import { computed } from 'vue'
import type { ViewStats } from '@/types'
import { useCatalogStore } from '@/stores/catalog'
import { useNav } from '@/composables/useNav'
import { TREE_DEPTH, kindName, viewAncestry } from '@/utils/views'
import { t } from '@/i18n'
import LineageTree from './LineageTree.vue'
import { buildLineage } from './tree'

const props = defineProps<{ stats: ViewStats }>()

const catalog = useCatalogStore()
const { pickView } = useNav()

const tree = computed(() => buildLineage(catalog.views, props.stats.name, props.stats.kind))
const up = computed(() => viewAncestry(catalog.views, props.stats.name))
const drawn = computed(() => tree.value.root.kids.length > 0 || tree.value.root.truncated)

const note = computed(() => {
  const fam = (props.stats.family ?? []).length
  if (props.stats.kind === 'base' && fam)
    return t('${n} name(s) resolve through this base — ${d} directly below it', {
      n: fam,
      d: tree.value.root.kids.length,
    })
  return drawn.value
    ? t('${n} name(s) hang below it', { n: tree.value.below })
    : t('nothing is derived from it yet')
})
</script>

<template>
  <div>
    <div v-if="up.length" class="cell-badges">
      <span class="muted">{{ t('resolves through') }}</span>
      <span aria-hidden="true">←</span>
      <button v-for="n in up" :key="n" type="button" class="mini mono" @click="pickView(n)">
        {{ n }}
      </button>
    </div>

    <ul class="tree">
      <li :class="'k-' + props.stats.kind">
        <span class="t-name t-cur">{{ props.stats.name }}</span>
        <span class="t-meta">{{ kindName(props.stats.kind) }}</span>
        <ul v-if="tree.root.kids.length">
          <LineageTree v-for="k in tree.root.kids" :key="k.name" :node="k" />
        </ul>
        <ul v-else-if="tree.root.truncated">
          <li class="muted">
            {{ t('deeper than ${d} levels — expand it from its own page', { d: TREE_DEPTH }) }}
          </li>
        </ul>
      </li>
    </ul>

    <p class="muted">{{ note }}</p>
  </div>
</template>

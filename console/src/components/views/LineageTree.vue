<script setup lang="ts">
/*
  One line of the lineage tree, and its own children.

  A recursive component because the namespace is recursive: a lens over a fork
  over a base is three levels of the same shape. The node data was already
  walked, de-duplicated and depth-capped by `buildLineage`, so this file only
  draws — which is also what keeps the drawing cheap on a wide namespace.
*/
import type { TreeNode } from './tree'
import { TREE_DEPTH, kindName } from '@/utils/views'
import { useNav } from '@/composables/useNav'
import { t } from '@/i18n'

defineProps<{ node: TreeNode }>()

const { pickView } = useNav()
</script>

<template>
  <li :class="['k-' + node.kind, 'clickable']">
    <span class="t-name" @click="pickView(node.name)">{{ node.name }}</span>
    <span class="t-meta">{{ kindName(node.kind) }}</span>
    <ul v-if="node.kids.length">
      <!-- This component's root IS the `<li>`, so the recursion keeps the
           nesting the connector lines are drawn for: `ul > li > ul > li`. -->
      <LineageTree v-for="k in node.kids" :key="k.name" :node="k" />
    </ul>
    <ul v-else-if="node.truncated">
      <li class="muted">
        {{ t('deeper than ${d} levels — expand it from its own page', { d: TREE_DEPTH }) }}
      </li>
    </ul>
  </li>
</template>

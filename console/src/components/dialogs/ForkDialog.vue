<script setup lang="ts">
/*
  Fork a view: a new writable name over a copy of someone else's metadata tree.

  What the copy is, and what it is not, is the whole dialog: a fork duplicates
  the METADATA tree and shares content bytes until something in the fork
  rewrites them. Saying so above the name field is what makes the cost of the
  verb obvious before it is taken, rather than after.

  `seed` defaults to ON, and one call does both halves (`with_def` records
  `fork <base>` beside seeding the tree). That is deliberate: two calls would
  leave a window where a name resolves to nothing, or a tree no name points at,
  depending on which half the tab survived.

  Unchecked, the fork is EMPTY — the shape the CLI's graft workflow starts from,
  which is why the option is offered even though it is the rare case.
*/
import { ref } from 'vue'
import type { ViewStats } from '@/types'
import { useCatalogStore } from '@/stores/catalog'
import { useOps } from '@/composables/useOps'
import { useUiStore } from '@/stores/ui'
import { messageOf } from '@/utils/error'
import { t } from '@/i18n'
import BaseModal from '@/components/overlay/BaseModal.vue'
import FieldRow from '@/components/layout/FieldRow.vue'

const props = defineProps<{ stats: ViewStats }>()
const emit = defineEmits<{ close: [] }>()

const { api } = useOps()
const catalog = useCatalogStore()
const ui = useUiStore()

const name = ref('')
const seed = ref(true)

async function run(fail: (msg: string) => void, close: () => void): Promise<void> {
  try {
    const r = await api('fork.create', {
      base: props.stats.name,
      name: name.value.trim(),
      seed: seed.value,
      with_def: true,
    })
    close()
    ui.toast(
      t('forked ${a} → ${b} (${n} nodes, ${m} blobs)', {
        a: props.stats.name,
        b: name.value.trim(),
        n: r.nodes,
        m: r.blobs,
      }),
    )
    catalog.sel = name.value.trim()
    ui.reload()
  } catch (e) {
    fail(messageOf(e))
  }
}
</script>

<template>
  <BaseModal
    :title="'Fork ${name}'"
    :vars="{ name: props.stats.name }"
    :confirm="'Create fork'"
    @confirm="run"
    @close="emit('close')"
  >
    <p class="muted">
      {{
        t(
          'A fork copies the METADATA tree into a new writable view; content bytes stay shared until something in the fork rewrites them.',
        )
      }}
    </p>
    <FieldRow label="new view name" for="fork-name">
      <input id="fork-name" v-model="name" :placeholder="`${props.stats.name}-exp1`" />
    </FieldRow>
    <label class="check" for="fork-seed">
      <input id="fork-seed" v-model="seed" type="checkbox" />
      {{ t('copy the current tree (unchecked: an empty fork to graft into)') }}
    </label>
  </BaseModal>
</template>

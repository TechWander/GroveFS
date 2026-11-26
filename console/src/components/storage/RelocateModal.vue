<script setup lang="ts">
/*
  Empty a PostgreSQL instance by moving its placed views onto another.

  This is the step the delete guard demands: an instance cannot be removed while
  it still hosts views, so the card offers Relocate to walk them off one at a
  time. `storage.relocate` is deliberately per-view (`pg_store::relocate` wraps
  `move_view`): a whole-batch move across a network has more failure modes than
  an operator watching each step deserves.

  The target list is every OTHER instance, and the server re-runs the same
  placement + mount guards the CLI does — a view mounted on THIS host is refused
  until it is unmounted, and that sentence is printed verbatim in the row rather
  than swallowed into a toast, because it is the row's own outcome.

  One successful move flips this server's cached engine for the view (the reply
  says how many were evicted), so the row leaves the list immediately. When the
  last one goes, the parent reloads and the instance becomes deletable.
*/
import { ref } from 'vue'
import type { StoreInstance } from '@/types'
import { useOps } from '@/composables/useOps'
import { useUiStore } from '@/stores/ui'
import { messageOf } from '@/utils/error'
import { t } from '@/i18n'
import BaseModal from '@/components/overlay/BaseModal.vue'

const props = defineProps<{
  inst: StoreInstance
  /** The views currently resolved onto this instance (the source). */
  views: string[]
  /** Every other PG instance a view may move onto. */
  targets: StoreInstance[]
}>()
const emit = defineEmits<{ close: [] }>()

const { api } = useOps()
const ui = useUiStore()

// A local, mutable working list so a completed move drops its row without
// waiting for the parent reload; the parent's snapshot is the source of truth
// once this dialog closes.
const remaining = ref<string[]>([...props.views])
const chosen = ref<Record<string, string>>({})
const busy = ref<Record<string, boolean>>({})
const rowErr = ref<Record<string, string>>({})
let moved = 0

function pickTarget(v: string): string {
  if (!chosen.value[v]) chosen.value = { ...chosen.value, [v]: props.targets[0]?.name ?? '' }
  return chosen.value[v]
}

async function moveOne(v: string): Promise<void> {
  const to = pickTarget(v)
  if (!to || busy.value[v]) return
  busy.value = { ...busy.value, [v]: true }
  rowErr.value = { ...rowErr.value, [v]: '' }
  try {
    const r = await api('storage.relocate', { view: v, to })
    remaining.value = remaining.value.filter((x) => x !== v)
    moved += 1
    // The server's own note about cached engines / other hosts, kept verbatim.
    ui.toast(`${v} → ${to}: ${r.note}`)
  } catch (e) {
    rowErr.value = { ...rowErr.value, [v]: messageOf(e) }
  } finally {
    busy.value = { ...busy.value, [v]: false }
  }
}

// A plain close: only reload if something actually moved, so a dialog opened by
// mistake does not churn the page.
function onClose(): void {
  if (moved > 0) ui.reload()
  emit('close')
}
</script>

<template>
  <BaseModal :title="'Relocate ${name}'" :vars="{ name: props.inst.name }" :cancel="'Done'" @close="onClose">
    <p v-if="!remaining.length" class="muted">
      {{ t('every view has moved off this instance — it can now be deleted.') }}
    </p>
    <p v-else class="muted">
      {{
        t('Move each placed view onto another instance. A view mounted on this host must be unmounted first.', {
          n: remaining.length,
        })
      }}
    </p>

    <ul v-if="remaining.length" class="reloc">
      <li v-for="v in remaining" :key="v" class="reloc-row">
        <span class="mono">{{ v }}</span>
        <select v-model="chosen[v]">
          <option v-for="tgt in targets" :key="tgt.name" :value="tgt.name">
            {{ tgt.name }}{{ tgt.control ? t(' (control)') : '' }}
          </option>
        </select>
        <button
          type="button"
          class="mini"
          :disabled="busy[v] || !chosen[v]"
          @click="moveOne(v)"
        >
          {{ t('Move') }}
        </button>
        <span v-if="rowErr[v]" class="err small">{{ rowErr[v] }}</span>
      </li>
    </ul>
  </BaseModal>
</template>

<style scoped>
.reloc {
  margin: var(--sp-2) 0 0;
  padding: 0;
  list-style: none;
  max-height: 22rem;
  overflow-y: auto;
}
.reloc-row {
  display: flex;
  flex-wrap: wrap;
  gap: var(--sp-2);
  align-items: center;
  padding: var(--sp-1) 0;
  border-bottom: 1px solid var(--border);
}
.reloc-row .mono {
  flex: 1 1 8rem;
  min-width: 6rem;
  word-break: break-all;
}
.small {
  font-size: 0.75rem;
  flex-basis: 100%;
}
</style>

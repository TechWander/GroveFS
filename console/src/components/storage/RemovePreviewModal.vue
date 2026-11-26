<script setup lang="ts">
/*
  The deletion preview → confirm flow for one instance.

  This dialog is the guard's UI. On mount it calls `storage.preview` and renders
  EXACTLY what the server measured, then trusts the server's own `canDelete`
  verdict rather than re-deriving one. When `canDelete` is false the confirm
  button is disabled and the server's `blockReason` sentence is shown where the
  button would be: an operator must see WHY the delete is refused next to the
  thing they wanted to delete, not after a click that only 400s.

  PG and treedb answer with different shapes, so the dialog keys off `isPg`
  (the PG payload carries no trustworthy `backend`):
    · PG     — `views` is per-view objects (name, nodes, distinct refs, whether
      the schema is really here) and `refs` is the shared `grovefs_refs`
      consistency scan. Both are shown; an UNVERIFIED instance (unreachable,
      so its ledger could not be read) is called out, because a delete dialog
      must not present "could not check" as "clean".
    · treedb — `views` is the live list of view names on that engine (`null`
      when it could not be reached), plus the experimental routing note.

  The verb itself re-runs the same guards server-side, so even if this preview
  were stale the removal still refuses on a live placement. The dialog never
  paraphrases those sentences — it prints them.
*/
import { computed, onMounted, ref } from 'vue'
import type { PreviewViewRow, StoragePreview, StoreInstance } from '@/types'
import { useOps } from '@/composables/useOps'
import { useUiStore } from '@/stores/ui'
import { messageOf } from '@/utils/error'
import { t } from '@/i18n'
import BaseModal from '@/components/overlay/BaseModal.vue'
import LoadingHint from '@/components/ui/LoadingHint.vue'

const props = defineProps<{ inst: StoreInstance; isPg: boolean }>()
const emit = defineEmits<{ close: [] }>()

const { api } = useOps()
const ui = useUiStore()
const preview = ref<StoragePreview | null>(null)
const loadErr = ref('')
const loading = ref(true)

onMounted(async () => {
  try {
    preview.value = await api('storage.preview', { instance: props.inst.name })
  } catch (e) {
    loadErr.value = messageOf(e)
  } finally {
    loading.value = false
  }
})

// PG nests per-view detail objects; treedb gives plain name strings. Split them
// so each is rendered by the block that understands it, never by a cast that
// pretends the two are the same list.
const pgRows = computed<PreviewViewRow[]>(() => {
  if (!props.isPg || !Array.isArray(preview.value?.views)) return []
  return (preview.value?.views as unknown[]).filter(
    (v): v is PreviewViewRow => typeof v === 'object' && v !== null,
  )
})
const affectedNames = computed<string[]>(() => {
  const p = preview.value
  if (!p) return []
  if (props.isPg) return p.placedViews ?? []
  return Array.isArray(p.views) ? (p.views as string[]) : []
})
// treedb's `views` is null exactly when the engine could not be reached — the
// blockReason already carries the server's sentence, but the list area must not
// claim "no view is placed here" when the truth is "could not verify".
const unreachable = computed(() => !props.isPg && preview.value?.views === null)
const refRows = computed(() => preview.value?.refs?.instances ?? [])
const unverified = computed<string[]>(() => preview.value?.refs?.unverified ?? [])
const zeroTotal = computed(() =>
  refRows.value.reduce((n, r) => n + (typeof r.zeroRows === 'number' ? r.zeroRows : 0), 0),
)

async function run(fail: (msg: string) => void, close: () => void): Promise<void> {
  try {
    const r = await api('storage.remove', { instance: props.inst.name })
    close()
    ui.toast(r.msg)
    ui.reload()
  } catch (e) {
    fail(messageOf(e))
  }
}
</script>

<template>
  <BaseModal
    :title="'Delete instance ${name}'"
    :vars="{ name: props.inst.name }"
    :confirm="'Delete'"
    confirm-variant="danger"
    :cancel="'Cancel'"
    :confirm-disabled="!preview || !preview.canDelete"
    @confirm="run"
    @close="emit('close')"
  >
    <LoadingHint v-if="loading" />
    <p v-else-if="loadErr" class="err">{{ loadErr }}</p>

    <template v-else-if="preview">
      <p v-if="preview.control" class="warn">
        {{ t('this instance carries the control plane — promote another first') }}
      </p>
      <p v-else-if="preview.serving" class="warn">
        {{ t('this is the address the meta-server routes to') }}
      </p>

      <p class="muted">
        {{
          t('Deleting removes the registry line only; content bytes live on the data servers and are never touched here.')
        }}
      </p>
      <p v-if="preview.note" class="muted small">{{ preview.note }}</p>

      <!-- PG: the per-view cost of the views still placed here. -->
      <div v-if="isPg && pgRows.length" class="preview-views">
        <h4>{{ t('views placed here') }}</h4>
        <ul>
          <li v-for="row in pgRows" :key="row.view" class="pv-row">
            <span class="mono">{{ row.view }}</span>
            <span class="pv-nums">
              {{ t('nodes') }} {{ row.nodes }} · {{ t('distinct refs') }} {{ row.distinctRefs }}
            </span>
            <span v-if="!row.hasSchema" class="warn small">
              {{ t('schema not found on this instance') }}
            </span>
          </li>
        </ul>
      </div>

      <!-- treedb: the live name list (or the honest "could not verify"). -->
      <div v-else-if="affectedNames.length" class="preview-views">
        <h4>{{ t('views placed here') }}</h4>
        <ul class="mono">
          <li v-for="v in affectedNames" :key="v">{{ v }}</li>
        </ul>
      </div>
      <p v-else-if="unreachable" class="warn">
        {{ t('the engine could not be reached, so its views could not be verified') }}
      </p>
      <p v-else class="muted">{{ t('no view is placed here') }}</p>

      <!-- The shared-ledger consistency scan, PG only, when the server ran it. -->
      <div v-if="isPg && refRows.length" class="preview-refs">
        <h4>{{ t('grovefs_refs consistency') }}</h4>
        <p class="mono small">
          {{ t('zero-holder rows (GC candidates)') }} {{ zeroTotal }}
        </p>
        <ul class="refs-list">
          <li v-for="r in refRows" :key="r.instance" class="refs-row">
            <span class="mono">{{ r.instance }}</span>
            <span v-if="r.error" class="warn small">{{ r.error }}</span>
            <template v-else>
              <span class="small">
                {{ t('holders') }} {{ r.holderRows ?? 0 }} · {{ t('zero') }} {{ r.zeroRows ?? 0 }}
              </span>
              <span v-if="(r.missingViews?.length ?? 0) > 0" class="warn small">
                {{ t('dangling routes') }}: {{ (r.missingViews ?? []).join(', ') }}
              </span>
            </template>
          </li>
        </ul>
        <p v-if="unverified.length" class="warn small">
          {{ t('unverified instances (control plane was unreachable):') }}
          {{ unverified.join(', ') }}
        </p>
      </div>

      <!-- The verdict. Not deletable → the server's reason, and no live confirm. -->
      <p v-if="!preview.canDelete" class="block-reason">
        {{ preview.blockReason || t('the guard refuses this removal') }}
      </p>
    </template>
  </BaseModal>
</template>

<style scoped>
.preview-views h4,
.preview-refs h4 {
  margin: var(--sp-3) 0 var(--sp-1);
  font-size: 0.75rem;
  text-transform: uppercase;
  letter-spacing: 0.05em;
  color: var(--fg-dim);
}
.preview-views ul {
  margin: 0;
  padding-left: 1.1rem;
  max-height: 14rem;
  overflow-y: auto;
  font-size: 0.8125rem;
}
.pv-row {
  display: flex;
  flex-wrap: wrap;
  gap: var(--sp-2);
  align-items: baseline;
  margin-bottom: var(--sp-1);
}
.pv-nums {
  color: var(--fg-dim);
  font-size: 0.75rem;
}
.refs-list {
  margin: 0;
  padding: 0;
  list-style: none;
  font-size: 0.75rem;
}
.refs-row {
  display: flex;
  flex-wrap: wrap;
  gap: var(--sp-2);
  padding: 2px 0;
  border-bottom: 1px solid var(--border);
}
.block-reason {
  margin: var(--sp-3) 0 0;
  padding: var(--sp-2) var(--sp-3);
  border-left: 3px solid var(--warn);
  background: var(--bg-subtle);
  border-radius: var(--r-sm);
  font-size: 0.8125rem;
}
.warn {
  color: var(--warn);
}
.small {
  font-size: 0.75rem;
}
</style>

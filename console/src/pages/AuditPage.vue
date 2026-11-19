<script setup lang="ts">
/*
  The governance log: every call the control plane answered, failures included.

  One fetch of the newest N rows, then everything else is client-side — the time
  chips, the verdict, the two dropdowns and the search box all filter what is
  already here. That is what makes the counts on the chips honest: they are the
  rows this window actually contains, and the line beside the heading says how
  many that is and where the trail ends.

  Where the trail ENDS is a property of the log, not of the activity in it: "the
  oldest row here is three weeks old" reads very differently when the server sweeps
  at 90d versus when nothing is ever deleted. `store.info` answers that, and it is a
  super-only op — so for a delegated operator the soft call simply shows no line
  rather than breaking the page.
*/
import { computed, ref } from 'vue'
import type { AuditRow } from '@/types'
import { useOps } from '@/composables/useOps'
import { usePageLoad } from '@/composables/usePageLoad'
import { useFilterStore } from '@/stores/filters'
import { useSessionStore } from '@/stores/session'
import { useUiStore } from '@/stores/ui'
import type { ChipItem } from '@/components/ui/chips'
import { WINDOWS, auditActor, distinct, withinWindow } from '@/utils/audit'
import { counted } from '@/utils/text'
import { t } from '@/i18n'
import PageHead from '@/components/layout/PageHead.vue'
import BarRow from '@/components/layout/BarRow.vue'
import FldRow from '@/components/layout/FldRow.vue'
import FilterBar from '@/components/ui/FilterBar.vue'
import ChipGroup from '@/components/ui/ChipGroup.vue'
import ActionButton from '@/components/ui/ActionButton.vue'
import LoadingHint from '@/components/ui/LoadingHint.vue'
import AuditTable from '@/components/audit/AuditTable.vue'

const filters = useFilterStore()
const session = useSessionStore()
const ui = useUiStore()
const { api, soft } = useOps()

const LIMITS = [50, 200, 1000]

const rows = ref<AuditRow[]>([])
/** `undefined` means this account may not read the retention setting, which is a
    different answer from `null` ("rows are kept until an operator purges them"). */
const retention = ref<string | null | undefined>(undefined)

async function load(): Promise<void> {
  rows.value = (await api('apikey.calls', { limit: filters.auditLimit })) ?? []
  const life = await soft('store.info', {})
  retention.value = life.ok && life.r ? life.r.auditRetention : undefined
  // A filter left over from an earlier load can name an op that is no longer in
  // the window (a quiet weekend, a renamed verb); fall back to "every op" rather
  // than showing an empty table under a dropdown that says nothing.
  const f = filters.auditFilter
  if (f.op !== 'all' && !distinct(rows.value, 'tool').includes(f.op)) f.op = 'all'
}

const { pending, error } = usePageLoad(load)

function refresh(): void {
  ui.reload()
}

function setLimit(ev: Event): void {
  filters.auditLimit = Number((ev.target as HTMLSelectElement).value)
  ui.reload()
}

const planes = computed(() => distinct(rows.value, 'method'))
const ops = computed(() => distinct(rows.value, 'tool'))

const f = filters.auditFilter

const windowChips = computed<ChipItem[]>(() =>
  WINDOWS.map(([v, label]) => ({
    v,
    t: label,
    n: rows.value.filter((r) => withinWindow(r.ts, v)).length,
  })),
)

const verdictChips = computed<ChipItem[]>(() =>
  (
    [
      ['all', 'every result'],
      ['failed', 'failures only'],
      ['ok', 'only ok'],
    ] as const
  ).map(([v, label]) => ({
    v,
    t: label,
    n: v === 'all' ? rows.value.length : rows.value.filter((r) => !!r.ok === (v === 'ok')).length,
  })),
)

const hit = computed(() =>
  rows.value.filter((r) => {
    if (!withinWindow(r.ts, f.window)) return false
    if (f.plane !== 'all' && r.method !== f.plane) return false
    if (f.op !== 'all' && r.tool !== f.op) return false
    if (f.verdict === 'ok' && !r.ok) return false
    if (f.verdict === 'failed' && r.ok) return false
    const q = f.q.trim().toLowerCase()
    if (q) {
      const hay = [auditActor(r), r.key_label, r.key_id, r.method, r.tool, r.detail]
      if (!hay.some((x) => x && String(x).toLowerCase().includes(q))) return false
    }
    return true
  }),
)

const kept = computed(() => {
  const n = rows.value.length
  if (retention.value === undefined)
    return t('the newest ${n} of them, whatever the filter shows', { n })
  return retention.value
    ? t('the newest ${n} loaded; the server sweeps rows older than ${w}', {
        n,
        w: retention.value,
      })
    : t('the server keeps every row until an operator purges them (`grovefs apikey purge`)')
})
</script>

<template>
  <PageHead title="Audit" desc="Every call the control plane answered — failures included.">
    <template #actions>
      <ActionButton :action="refresh" :label="'refresh'" variant="link" />
    </template>
  </PageHead>

  <LoadingHint v-if="pending && !rows.length" />
  <p v-else-if="error" class="err">{{ error }}</p>

  <template v-else>
    <BarRow :title="'Governance calls'">
      <FldRow :label="'rows to load'" for="audit-limit">
        <select :value="String(filters.auditLimit)" @change="setLimit">
          <option v-for="n in LIMITS" :key="n" :value="String(n)">{{ n }}</option>
        </select>
      </FldRow>
      <span class="muted">{{ kept }}</span>
    </BarRow>

    <FilterBar :count="counted(hit.length, rows.length)">
      <template #search>
        <input
          v-model="f.q"
          type="search"
          :placeholder="t('filter by account, key, op or message…')"
          :aria-label="t('filter by account, key, op or message…')"
        />
      </template>

      <template #opts>
        <FldRow :label="'time'">
          <ChipGroup v-model="f.window" :items="windowChips" />
        </FldRow>
        <FldRow :label="'result'">
          <ChipGroup v-model="f.verdict" :items="verdictChips" />
        </FldRow>
        <FldRow :label="'plane'" for="audit-plane">
          <select id="audit-plane" v-model="f.plane">
            <option value="all">{{ t('every plane') }}</option>
            <option v-for="m in planes" :key="m" :value="m">{{ m }}</option>
          </select>
        </FldRow>
        <FldRow :label="'op'" for="audit-op">
          <select id="audit-op" v-model="f.op">
            <option value="all">{{ t('every op') }}</option>
            <option v-for="o in ops" :key="o" :value="o">{{ o }}</option>
          </select>
        </FldRow>
      </template>
    </FilterBar>

    <AuditTable
      :rows="hit"
      :empty="rows.length ? 'no calls match the filter' : 'no governance calls yet'"
    />

    <p class="muted">
      {{
        session.isSuper
          ? t(
              'Every call the control plane answered, by any key or account. Failures are here too — a wrong credential included. Click a row for the whole message the server wrote.',
            )
          : t(
              'Only your own activity and your views’ keys; the full log is an administrator’s view.',
            )
      }}
    </p>
  </template>
</template>

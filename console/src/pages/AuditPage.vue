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
import { computed, ref, watch } from 'vue'
import type { AuditRow } from '@/types'
import { useOps } from '@/composables/useOps'
import { usePageLoad } from '@/composables/usePageLoad'
import { useCatalogStore } from '@/stores/catalog'
import { useFilterStore } from '@/stores/filters'
import { useSessionStore } from '@/stores/session'
import { useUiStore } from '@/stores/ui'
import type { ChipItem } from '@/components/ui/chips'
import {
  WINDOWS,
  auditActor,
  distinct,
  keyChoices,
  mintedKey,
  shortId,
  withinWindow,
} from '@/utils/audit'
import type { KeyChoice } from '@/utils/audit'
import { counted } from '@/utils/text'
import { t } from '@/i18n'
import PageHead from '@/components/layout/PageHead.vue'
import BarRow from '@/components/layout/BarRow.vue'
import FldRow from '@/components/layout/FldRow.vue'
import FilterBar from '@/components/ui/FilterBar.vue'
import ChipGroup from '@/components/ui/ChipGroup.vue'
import ListPager from '@/components/ui/ListPager.vue'
import ActionButton from '@/components/ui/ActionButton.vue'
import LoadingHint from '@/components/ui/LoadingHint.vue'
import AuditTable from '@/components/audit/AuditTable.vue'
import AuditRetentionPanel from '@/components/audit/AuditRetentionPanel.vue'

const filters = useFilterStore()
const catalog = useCatalogStore()
const session = useSessionStore()
const ui = useUiStore()
const { api, soft } = useOps()

const rows = ref<AuditRow[]>([])
/** `undefined` means this account may not read the retention setting, which is a
    different answer from `null` ("rows are kept until an operator purges them"). */
const retention = ref<string | null | undefined>(undefined)

async function load(): Promise<void> {
  rows.value = (await api('apikey.calls', { limit: filters.auditLimit })) ?? []
  // The view dropdown lists NAMES, and the namespace is the catalog three
  // other pages already share — a cached read here, not a second registry.
  void catalog.loadViews()
  const life = await soft('store.info', {})
  retention.value = life.ok && life.r ? life.r.auditRetention : undefined
  // A filter left over from an earlier load can name an op that is no longer in
  // the window (a quiet weekend, a renamed verb); fall back to "every op" rather
  // than showing an empty table under a dropdown that says nothing.
  const f = filters.auditFilter
  if (f.op !== 'all' && !distinct(rows.value, 'tool').includes(f.op)) f.op = 'all'
  // Same for a credential whose calls have all aged out of the window.
  if (f.key !== 'all' && !keyChoices(rows.value).some((k) => k.id === f.key)) f.key = 'all'
}

const { pending, error } = usePageLoad(load)

function refresh(): void {
  ui.reload()
}

const planes = computed(() => distinct(rows.value, 'method'))
const ops = computed(() => distinct(rows.value, 'tool'))
const keys = computed(() => keyChoices(rows.value))

const f = filters.auditFilter

/** What the key dropdown offers. A key reached by following a mint link has
    often made no call yet, so it is not in `keyChoices(rows)` — and a `<select>`
    whose value matches no option renders BLANK, which is the exact failure this
    page's reload guard refuses to leave on screen. Keep the chosen key in the
    list: its calls being absent from the log is then stated by the empty table,
    not by an unlabelled control. */
const keyOptions = computed<KeyChoice[]>(() => {
  const ks = keys.value
  if (f.key !== 'all' && !ks.some((k) => k.id === f.key)) return [...ks, { id: f.key, label: null }]
  return ks
})

/** What the view dropdown offers: every name the namespace knows, plus the one
    a jump from the views page selected — a view deleted moments ago can still
    be the thing being searched for, and a `<select>` that drops its own value
    renders blank. */
const viewOptions = computed<string[]>(() => {
  const ns = catalog.views.map((v) => v.name)
  if (f.view !== 'all' && !ns.includes(f.view)) return [...ns, f.view]
  return ns
})

/** What a row minted, by id — the parent→child link the store itself does not
    keep (`api_keys.created_by` names an account, never a credential). */
const mintedBy = computed(() => {
  if (f.key === 'all') return []
  const out: string[] = []
  for (const r of rows.value) {
    const child = mintedKey(r)
    if (child && r.key_id === f.key && !out.includes(child)) out.push(child)
  }
  return out
})

const mintChips = computed<ChipItem[]>(() =>
  mintedBy.value.map((id) => ({
    v: id,
    t: shortId(id),
    n: rows.value.filter((r) => r.key_id === id).length || null,
  })),
)

const windowOptions = computed(() =>
  WINDOWS.map(([v, label]) => ({
    v,
    t: label,
    n: rows.value.filter((r) => withinWindow(r.ts, v, f.from, f.to)).length,
  })),
)

/* A 30-day question over a 50-row load answers "nothing happened". When the
   chosen window can outrun what is loaded, raise the load once — the wider
   read is what the operator meant by picking it. */
const WIDE = new Set(['30d', '180d', '360d', 'custom'])
function setWindow(ev: Event): void {
  const w = (ev.target as HTMLSelectElement).value
  f.window = w
  if (WIDE.has(w) && filters.auditLimit < 1000) {
    filters.auditLimit = 1000
    ui.reload()
  }
}

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
    if (!withinWindow(r.ts, f.window, f.from, f.to)) return false
    if (f.plane !== 'all' && r.method !== f.plane) return false
    if (f.op !== 'all' && r.tool !== f.op) return false
    if (f.key !== 'all' && r.key_id !== f.key) return false
    // The view a call is ABOUT is only recoverable from the server's own
    // message — the row has no view column — so this is a mention match over
    // the detail text, stated as such under the table.
    if (f.view !== 'all' && !String(r.detail ?? '').includes(f.view)) return false
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

/*
  Paging the matched rows.

  `hit` is the whole filtered set — it can be a thousand rows deep on a box that
  loads 1000, and a table that long hides the filter that produced it. The pager
  splits THAT set, so the counts on the chips, the `counted()` pill and the
  "no calls match the filter" line all keep describing the same thing the table
  does. `page` is clamped rather than trusted: a refresh that narrows the result
  can leave the stored page past its end, and page 7 of three pages would render
  an empty table under a control that still claims to be on it.
*/
const pageCount = computed(() =>
  Math.max(1, Math.ceil(hit.value.length / filters.auditPageSize)),
)
const page = computed(() => Math.min(filters.auditPage, pageCount.value))
const paged = computed(() => {
  const start = (page.value - 1) * filters.auditPageSize
  return hit.value.slice(start, start + filters.auditPageSize)
})

/*
  The one pager control that is not arithmetic.

  `auditLimit` is how many rows the SERVER was asked for; everything else on this
  page filters or splits that answer. Moving the ceiling therefore reloads — and
  it is the control that answers "how do I see further back?", which is why it
  sits with the pager instead of in the retention strip it used to live in.
*/
function setCeiling(n: number): void {
  filters.auditLimit = n
  filters.auditPage = 1
  ui.reload()
}

/** Show the pager when there is more to read one way or the other: more loaded
    rows to page through, or a fetch that filled its ceiling so an older answer
    is probably still in the trail. A short, filtered list hides it. */
const canReadMore = computed(
  () => hit.value.length > filters.auditPageSize || rows.value.length >= filters.auditLimit,
)

// Any change to what is BEING FILTERED restarts at page 1 — the operator asked a
// new question, and the old answer's page number does not carry over.
watch(
  [() => ({ ...f }), () => filters.auditPageSize],
  () => {
    filters.auditPage = 1
  },
)
</script>

<template>
  <PageHead
    kicker="Ledger"
    title="Audit"
    desc="Every call the control plane answered — failures included."
  >
    <template #actions>
      <ActionButton :action="refresh" :label="'refresh'" variant="link" />
    </template>
  </PageHead>

  <LoadingHint v-if="pending && !rows.length" />
  <p v-else-if="error" class="err">{{ error }}</p>

  <template v-else>
    <!-- The retention policy, at the TOP: how long this trail is kept decides
         how the rows below are read, so the answer sits where the question is
         asked — not after a thousand rows of scrolling. The strip shows its
         editor per verb, so a delegated account that cannot read `store.info`
         at all sees no strip instead of a dash. -->
    <AuditRetentionPanel
      v-if="retention !== undefined || session.can('audit.retention.set')"
      :current="retention"
    />

    <BarRow :title="'Governance calls'">
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
        <FldRow :label="'time range'" for="audit-window">
          <select id="audit-window" :value="f.window" @change="setWindow">
            <option v-for="w in windowOptions" :key="w.v" :value="w.v">
              {{ t(w.t) }} · {{ w.n }}
            </option>
          </select>
        </FldRow>
        <template v-if="f.window === 'custom'">
          <FldRow :label="'from'" for="audit-from">
            <input id="audit-from" v-model="f.from" type="date" />
          </FldRow>
          <FldRow :label="'to'" for="audit-to">
            <input id="audit-to" v-model="f.to" type="date" />
          </FldRow>
        </template>
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
        <FldRow :label="'view'" for="audit-view">
          <select id="audit-view" v-model="f.view">
            <option value="all">{{ t('every view') }}</option>
            <option v-for="v in viewOptions" :key="v" :value="v">{{ v }}</option>
          </select>
        </FldRow>
        <FldRow :label="'key'" for="audit-key">
          <select id="audit-key" v-model="f.key">
            <option value="all">{{ t('every key') }}</option>
            <option v-for="k in keyOptions" :key="k.id" :value="k.id">
              {{ k.label ? `${k.label} · ${shortId(k.id)}` : shortId(k.id) }}
            </option>
          </select>
        </FldRow>
      </template>
    </FilterBar>

    <BarRow v-if="f.key !== 'all'" :title="'key trail'">
      <ChipGroup v-if="mintChips.length" v-model="f.key" :items="mintChips" />
      <span class="muted">
        {{
          mintChips.length
            ? t('${n} minted by this key, in the rows loaded here', { n: mintedBy.length })
            : t('nothing in these rows was minted by this key')
        }}
      </span>
    </BarRow>

    <AuditTable
      :rows="paged"
      :empty="rows.length ? 'no calls match the filter' : 'no governance calls yet'"
    />

    <!-- Paging controls sit where the reader reaches the end of a page: the
         range they just read, how deep the fetch went, the width they want next,
         the page numbers. -->
    <ListPager
      v-if="canReadMore"
      :page="page"
      :size="filters.auditPageSize"
      :total="hit.length"
      :fetch-limit="filters.auditLimit"
      :loaded="rows.length"
      @update:page="filters.auditPage = $event"
      @update:size="filters.auditPageSize = $event"
      @update:fetch-limit="setCeiling"
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

    <!-- The key-attribution sentence answers a question only a reader who has
         just filtered by key is asking; on the unfiltered page it was the last
         of two paragraphs and pushed the table's end into a wall of text. -->
    <p v-if="f.key !== 'all'" class="muted">
      {{
        t(
          'A row’s key is the credential that signed the call. From this console that is always the gateway’s own admin key, because the gateway holds one and forwards what you click: filtering by key finds the door, searching by account finds who walked through it.',
        )
      }}
    </p>
  </template>
</template>

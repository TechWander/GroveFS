<script setup lang="ts">
/*
  Page controls for a list that is ALREADY loaded — the audit page pulls the
  newest N rows and filters them here, so paging is arithmetic, not another
  request.

  Two numbers sit in this row and they mean different things, which is why they
  are labelled differently and kept together:

  · the FETCH CEILING (`fetchLimit`) — how many rows the server was asked for.
    Raising it is the only way to read further back, and it reloads.
  · the PAGE WIDTH (`size`) — how much of that answer is on screen at once.
    Pure arithmetic, no request.

  Two rules the markup follows:

  · A page list never prints forty numbers. When the set is long it shows the
    first, the last, and the neighbourhood of the current page, with a `…`
    standing where a run was skipped — so the reader always knows how far the
    trail goes without scrolling through the control itself.
  · Changing the page size restarts at page 1, because a page number means
    nothing across a different page width; changing the page keeps the size.

  The scroll to the top of the page is deliberate: after a 200-row table is
  replaced by another 200-row table, leaving the viewport at its bottom shows
  the END of a page the reader has not started. It honours
  `prefers-reduced-motion` instead of animating over the operator's objection.
*/
import { computed } from 'vue'
import { t } from '@/i18n'

const props = defineProps<{
  /** 1-based. */
  page: number
  /** Rows per page. */
  size: number
  /** How many rows MATCHED, across every page. */
  total: number
  /** The widths offered. Defaults to the console's own ladder. */
  sizes?: number[]
  /** The ceiling the server was asked for; omitted where a list has no such
      knob (the accounts and keys pages page a full list, so they pass nothing). */
  fetchLimit?: number
  /** How many rows that fetch actually returned. Equal to `fetchLimit` when
      there are older rows the reader has not pulled — `total` cannot say that,
      because a filter can shrink the matched set to nothing. */
  loaded?: number
  /** The ceilings offered. */
  fetchLimits?: number[]
}>()

const emit = defineEmits<{
  'update:page': [n: number]
  'update:size': [n: number]
  'update:fetchLimit': [n: number]
}>()

const widths = computed(() => props.sizes ?? [20, 50, 100])
const ceilings = computed(() => props.fetchLimits ?? [50, 200, 1000])
/** Did the server fill the order? Then the trail almost certainly runs deeper
    than what is on screen, and the reader is entitled to know that the pager's
    "of N" is a count of the LOADED set, not of the deployment's history. */
const atCeiling = computed(
  () => props.fetchLimit !== undefined && props.loaded === props.fetchLimit,
)
const pages = computed(() => Math.max(1, Math.ceil(props.total / props.size)))
const from = computed(() => (props.total ? (props.page - 1) * props.size + 1 : 0))
const to = computed(() => Math.min(props.total, props.page * props.size))

/** First, last, current ±1, `gap` where a run was skipped. */
const steps = computed<(number | 'gap')[]>(() => {
  const n = pages.value
  const cur = props.page
  if (n <= 7) return Array.from({ length: n }, (_, i) => i + 1)
  const out: (number | 'gap')[] = [1]
  const lo = Math.max(2, cur - 1)
  const hi = Math.min(n - 1, cur + 1)
  if (lo > 2) out.push('gap')
  for (let i = lo; i <= hi; i++) out.push(i)
  if (hi < n - 1) out.push('gap')
  out.push(n)
  return out
})

function go(n: number): void {
  const p = Math.min(Math.max(1, n), pages.value)
  if (p === props.page) return
  emit('update:page', p)
  const reduce = window.matchMedia?.('(prefers-reduced-motion: reduce)').matches
  window.scrollTo({ top: 0, behavior: reduce ? 'auto' : 'smooth' })
}

function onWidth(ev: Event): void {
  emit('update:size', Number((ev.target as HTMLSelectElement).value))
  emit('update:page', 1)
}

/* Raising or lowering the ceiling REPLACES the loaded set with a bigger (or
   smaller) answer, so the page resets with it — the same rule `onWidth` follows
   for a different reason. */
function onCeiling(ev: Event): void {
  emit('update:fetchLimit', Number((ev.target as HTMLSelectElement).value))
  emit('update:page', 1)
}
</script>

<template>
  <nav class="pager" :aria-label="t('pagination')">
    <span class="pager-range">
      {{ t('rows ${a}–${b} of ${n}', { a: from, b: to, n: props.total }) }}
    </span>

    <label v-if="props.fetchLimit !== undefined" class="pager-size pager-fetch">
      <span>{{ t('fetched from server') }}</span>
      <select
        :value="String(props.fetchLimit)"
        :aria-label="t('how many newest rows to fetch from the server')"
        @change="onCeiling"
      >
        <option v-for="n in ceilings" :key="n" :value="String(n)">{{ n }}</option>
      </select>
    </label>

    <label class="pager-size">
      <span>{{ t('per page') }}</span>
      <select :value="String(props.size)" :aria-label="t('rows per page')" @change="onWidth">
        <option v-for="s in widths" :key="s" :value="String(s)">{{ s }}</option>
      </select>
    </label>

    <div class="pager-nav">
      <button
        type="button"
        class="pg-edge"
        :disabled="props.page <= 1"
        :aria-label="t('previous page')"
        @click="go(props.page - 1)"
      >
        ‹
      </button>
      <template v-for="(s, i) in steps" :key="`${s}-${i}`">
        <span v-if="s === 'gap'" class="pg-gap" aria-hidden="true">…</span>
        <button
          v-else
          type="button"
          :class="['pg-num', s === props.page ? 'on' : '']"
          :aria-current="s === props.page ? 'page' : undefined"
          :aria-label="t('page ${n}', { n: s })"
          :disabled="s === props.page"
          @click="go(s)"
        >
          {{ s }}
        </button>
      </template>
      <button
        type="button"
        class="pg-edge"
        :disabled="props.page >= pages"
        :aria-label="t('next page')"
        @click="go(props.page + 1)"
      >
        ›
      </button>
    </div>

    <!-- Its own row (`.pager` wraps), because it qualifies the number the reader
         just looked at: "of ${n}" counts the LOADED set, not the trail. -->
    <p v-if="atCeiling" class="pager-note">
      {{
        t(
          'the server was asked for ${n} rows and returned ${n}, so older calls are almost certainly still there — raise “fetched from server” to read further back',
          { n: props.fetchLimit },
        )
      }}
    </p>
  </nav>
</template>

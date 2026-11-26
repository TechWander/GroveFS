<script setup lang="ts">
/*
  The retention policy, where the operator reads it BEFORE the log: the strip
  sits under the page title, because "how long is this trail kept?" is the
  question that decides how to read every row below it.

  This is the admin-facing half of `--audit-retention`. The window is a policy
  knob the meta-server re-reads on every reaper tick, so saving it needs no
  restart; `never` disarms the sweep without deleting a single existing row
  (disarming is not destroying). Only applying a real duration cuts, and it cuts
  against the NEW cutoff immediately — an operator who shortens retention means
  "trim to that now", the same contract the startup pass runs.

  `apikey.purge` is the one verb here that destroys OTHER credentials' history,
  so a dry-run preview sits beside it: an operator sees the size of the cut
  before taking it, and the danger verb is the one they have to reach for.

  The readout (what is configured) is shown to anyone the parent lets see it; the
  EDIT affordances appear per verb, so a session that can read `store.info` but
  not write `audit.retention.set` gets a policy it cannot click — never a button
  that only ever 403s. A refusal on one call is still shown inline verbatim: the
  server's sentence is the answer.

  How many rows were FETCHED is not this strip's business: it is a property of
  the table below, and it lives in the pager next to that table (see
  `ListPager`). An earlier revision carried a "rows to load" select here, and an
  operator read its number as "how many audit rows exist" — a policy bar is the
  wrong place to answer a reading-depth question.
*/
import { computed, ref, watch } from 'vue'
import { useOps } from '@/composables/useOps'
import { useSessionStore } from '@/stores/session'
import { useUiStore } from '@/stores/ui'
import { messageOf } from '@/utils/error'
import { t } from '@/i18n'
import ActionButton from '@/components/ui/ActionButton.vue'

const { api } = useOps()
const ui = useUiStore()
const session = useSessionStore()

/** What the server currently keeps (`null` = until purged by hand, `undefined`
    = this account may not read it, which is a THIRD sentence). */
const props = defineProps<{ current: string | null | undefined }>()

/** The window being edited; pre-filled with what is configured. */
const draft = ref(props.current ?? 'never')
/** Whether the operator has touched the field: until they do, a refresh that
    reports a different window re-seeds it, so the strip cannot drift from the
    server while nobody is mid-edit. */
const touched = ref(false)
/** The last action's sentence, shown until the next one. Errors and results
    share this line — the server's own wording is what the admin needs to read. */
const status = ref('')
const failed = ref(false)

watch(
  () => props.current,
  (v) => {
    if (!touched.value) draft.value = v ?? 'never'
  },
)

/** `s`/`m`/`h`/`d` counts, or `never`/`0` — the spellings `duration_secs` takes. */
const DUR = /^\d+[smhd]$/
const isNever = computed(() =>
  ['never', 'none', '0', ''].includes(draft.value.trim().toLowerCase()),
)
const isDuration = computed(() => DUR.test(draft.value.trim().toLowerCase()))
/** Purge needs a real cutoff; a `never` window has nothing to cut against. */
const canPurge = computed(() => isDuration.value && session.can('apikey.purge'))
const canSet = computed(() => session.can('audit.retention.set'))

const PRESETS = ['7d', '30d', '90d', '180d', '365d', 'never']

/** What the strip leads with: the configured window, in the server's own word. */
const currentShown = computed(() =>
  props.current === undefined ? '—' : props.current === null ? t('kept until purged') : props.current,
)
const currentSentence = computed(() => {
  if (props.current === undefined) return t('retention is an administrator’s reading')
  return props.current
    ? t('rows older than ${w} are swept', { w: props.current })
    : t('every row is kept until an operator purges it')
})

function usePreset(v: string): void {
  touched.value = true
  draft.value = v
}

function onDraft(): void {
  touched.value = true
}

function say(msg: string, ok = true): void {
  status.value = msg
  failed.value = !ok
}

/** Save the window. A duration also reaps immediately and reports how many rows
    actually went; `never` just disarms and reports zero. */
async function applyWindow(): Promise<void> {
  const w = draft.value.trim()
  if (!isNever.value && !isDuration.value) {
    say(t('use a duration like 45m, 24h, 90d, or `never`'), false)
    return
  }
  try {
    const r = await api('audit.retention.set', { retention: w })
    touched.value = false
    if (r.retention) {
      say(
        t('window set to ${w}; ${n} row(s) older than that were trimmed now', {
          w: r.retention,
          n: r.trimmedNow,
        }),
      )
    } else {
      say(t('sweep disarmed — rows are kept until an operator purges them'))
    }
    ui.reload()
  } catch (e) {
    say(messageOf(e), false)
  }
}

/** Count what a purge at the drafted window WOULD delete, without deleting. */
async function previewPurge(): Promise<void> {
  try {
    const r = await api('apikey.purge', { before: draft.value.trim(), dryRun: true })
    say(t('${n} row(s) are older than ${b} (nothing deleted yet)', { n: r.older, b: r.before }))
  } catch (e) {
    say(messageOf(e), false)
  }
}

/** The real cut: delete everything older than the drafted window. */
async function purgeNow(): Promise<void> {
  try {
    const r = await api('apikey.purge', { before: draft.value.trim(), dryRun: false })
    say(t('deleted ${n} row(s) older than ${b}', { n: r.deleted, b: r.before }))
    ui.reload()
  } catch (e) {
    say(messageOf(e), false)
  }
}
</script>

<template>
  <section class="retbar" :class="canSet ? 'is-edit' : 'is-read'">
    <!-- The policy, stated before it is editable: value first, meaning second. -->
    <div class="retbar-lead">
      <span class="retbar-k">{{ t('retention policy') }}</span>
      <b class="retbar-v">{{ currentShown }}</b>
      <span class="retbar-s">{{ currentSentence }}</span>
    </div>

    <div v-if="canSet" class="retbar-edit">
      <div class="retbar-set">
        <input
          id="retention-window"
          v-model="draft"
          list="retention-presets"
          class="retbar-input"
          :aria-label="t('retention window')"
          placeholder="90d"
          spellcheck="false"
          autocomplete="off"
          @input="onDraft"
        />
        <datalist id="retention-presets">
          <option v-for="p in PRESETS" :key="p" :value="p"></option>
        </datalist>
        <div class="retbar-chips" role="group" :aria-label="t('retention window')">
          <button
            v-for="p in PRESETS"
            :key="p"
            type="button"
            :class="['chip', 'dotless', { on: draft === p }]"
            :aria-pressed="draft === p ? 'true' : 'false'"
            @click="usePreset(p)"
          >
            {{ p }}
          </button>
        </div>
      </div>
      <div class="retbar-verbs">
        <ActionButton
          v-if="canPurge"
          :action="previewPurge"
          :label="'preview cut'"
          variant="mini"
          :title="t('count rows older than the drafted window, delete nothing')"
        />
        <ActionButton
          v-if="canPurge"
          :action="purgeNow"
          :label="'purge now'"
          variant="mini danger"
          :title="t('delete rows older than the drafted window')"
        />
        <ActionButton
          :action="applyWindow"
          :label="'save window'"
          variant="primary"
          :title="t('apply the retention policy (a duration also trims now)')"
        />
      </div>
    </div>

    <p v-if="status" :class="['retbar-msg', failed ? 'err' : 'muted']">{{ status }}</p>
  </section>
</template>

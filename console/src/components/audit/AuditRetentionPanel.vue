<script setup lang="ts">
/*
  Set the audit-retention window and trim the trail NOW — from the page.

  This panel is the admin-facing half of `--audit-retention`. The window is a
  policy knob the meta-server re-reads on every reaper tick, so saving it needs
  no restart; `never` disarms the sweep without deleting a single existing row
  (disarming is not destroying). Only applying a real duration cuts, and it cuts
  against the NEW cutoff immediately — an operator who shortens retention means
  "trim to that now", the same contract the startup pass runs.

  `apikey.purge` is the one verb here that destroys OTHER credentials' history,
  so a dry-run preview sits beside it: an operator sees the size of the cut
  before taking it, and the danger verb is the one they have to reach for.

  Every button here is a `Super`-gated op (`audit.retention.set`, `apikey.purge`),
  so the parent renders this only when the session can route them. A refusal on
  one call is still shown inline verbatim — the server's sentence is the answer.
*/
import { computed, ref } from 'vue'
import { useOps } from '@/composables/useOps'
import { useUiStore } from '@/stores/ui'
import { messageOf } from '@/utils/error'
import { t } from '@/i18n'
import PanelBox from '@/components/layout/PanelBox.vue'
import FieldRow from '@/components/layout/FieldRow.vue'
import ActionButton from '@/components/ui/ActionButton.vue'

const { api } = useOps()
const ui = useUiStore()

/** What the server currently keeps (`null` = until purged by hand). Seeded by the
    parent so the field opens on the live value, not an empty box. */
const props = defineProps<{ current: string | null }>()

/** The window being edited; pre-filled with what is configured. */
const draft = ref(props.current ?? 'never')
/** The last action's sentence, shown until the next one. Errors and results
    share this line — the server's own wording is what the admin needs to read. */
const status = ref('')
const failed = ref(false)

/** `s`/`m`/`h`/`d` counts, or `never`/`0` — the spellings `duration_secs` takes. */
const DUR = /^\d+[smhd]$/
const isNever = computed(() =>
  ['never', 'none', '0', ''].includes(draft.value.trim().toLowerCase()),
)
const isDuration = computed(() => DUR.test(draft.value.trim().toLowerCase()))
/** Purge needs a real cutoff; a `never` window has nothing to cut against. */
const canPurge = computed(() => isDuration.value)

const PRESETS = ['7d', '30d', '90d', '180d', '365d', 'never']

function usePreset(v: string): void {
  draft.value = v
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
  <PanelBox :title="'Audit retention'">
    <FieldRow for="retention-window" label="retention window">
      <input
        id="retention-window"
        v-model="draft"
        list="retention-presets"
        placeholder="90d"
        spellcheck="false"
        autocomplete="off"
      />
      <datalist id="retention-presets">
        <option v-for="p in PRESETS" :key="p" :value="p"></option>
      </datalist>
      <template #hint>
        {{ t('rows older than this are swept; “never” keeps them until purged by hand') }}
      </template>
    </FieldRow>

    <div class="chips" role="group" :aria-label="t('retention window')">
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

    <p v-if="status" :class="failed ? 'err' : 'muted'">{{ status }}</p>
    <p v-else class="muted">
      {{
        props.current
          ? t('currently: rows older than ${w} are swept', { w: props.current })
          : t('currently: every row is kept until an operator purges it')
      }}
    </p>

    <div class="panel-foot">
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
  </PanelBox>
</template>

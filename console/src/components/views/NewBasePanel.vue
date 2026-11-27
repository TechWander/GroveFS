<script setup lang="ts">
/*
  Create a base view — the one act on this page that chooses where bytes live.

  The instance dropdown lists the control plane's own registry (via
  `store.info`), never a copy typed into this file. It is not a PostgreSQL-only
  affordance: a treedb deployment keeps its own named book
  (`treedb_instances.conf`, managed from the Storage page), and `view.create`'
  `instance` argument pins the home on both engines. One entry means there is no
  decision to make, so the row hides itself instead of asking the operator to
  confirm the obvious.

  "Leaving it empty" means different rules per engine, and the form says which.
  PostgreSQL takes the least-loaded instance — countable here off the same
  ledger the balancer reads. treedb hashes the NAME across its serving
  addresses, which no count can predict, so that answer is ASKED of the server
  (`view.plan`) rather than reimplemented in the browser; the plan also reports a
  name that already exists, which is the refusal the create button would get.

  When the signed-in account cannot create one, the panel does not offer a button
  guaranteed to be refused — it says whose act it is instead. That is the single
  place in the console where a capability is hidden rather than refused, because
  here the refusal would arrive on every page load, not on one verb.
*/
import { computed, onMounted, ref, watch } from 'vue'
import { useCatalogStore } from '@/stores/catalog'
import { useOps } from '@/composables/useOps'
import { useSessionStore } from '@/stores/session'
import { useUiStore } from '@/stores/ui'
import type { PlacementPlan } from '@/api/ops'
import { t } from '@/i18n'
import PanelBox from '@/components/layout/PanelBox.vue'
import FieldRow from '@/components/layout/FieldRow.vue'
import ActionButton from '@/components/ui/ActionButton.vue'

const catalog = useCatalogStore()
const session = useSessionStore()
const ui = useUiStore()
const { api, soft } = useOps()

/* `instance` pins the target when the form is opened FROM an instance card;
   the field stays editable so the operator can still change their mind. */
const props = defineProps<{ instance?: string }>()
const emit = defineEmits<{ created: [name: string] }>()

const name = ref('')
const instance = ref(props.instance ?? '')
watch(
  () => props.instance,
  (v) => {
    if (v) instance.value = v
  },
)

/* Every registry row the snapshot reported, before the engine filters it. */
interface Row {
  name: string
  views: number | null
  control: boolean
  serving?: boolean
}
const rows = ref<Row[]>([])
const engine = computed(() => catalog.engine)

/* The rows a new tree may actually be pinned to. A treedb registry can carry
   inventory this server's `--meta-store` does not route to, and pinning one is
   refused by name — so it is not a choice worth offering. PostgreSQL has no
   such flag: every registry row routes. */
const instances = computed(() =>
  engine.value === 'treedb' ? rows.value.filter((i) => i.serving !== false) : rows.value,
)
const showInstanceField = computed(
  () => (engine.value === 'postgres' || engine.value === 'treedb') && instances.value.length > 1,
)

onMounted(async () => {
  // Placement ledger first (per-instance counts come from the control plane);
  // if it cannot be read, fall back to the storage snapshot's own counts, and
  // failing or ACL-refused calls leave the list empty, which hides the row:
  // creating with auto placement stays possible either way.
  const r = await soft('store.info', {})
  if (!r.ok) return
  const ledger = r.r?.pgInstances?.instances
  if (Array.isArray(ledger)) {
    rows.value = ledger
    return
  }
  const st = r.r?.storage
  if (
    (st?.backend === 'postgres' || st?.backend === 'treedb') &&
    Array.isArray(st.instances)
  )
    rows.value = st.instances.map((i) => ({
      name: i.name,
      views: i.placedViews ?? null,
      control: i.control,
      serving: i.serving,
    }))
})

/* treedb's auto placement, asked of the server that owns the rule. Debounced by
   sequence number: typing a name fires several lookups and only the last one may
   land. An old binary that does not route `view.plan`, or a name too short to be
   real, leaves it null and the option falls back to the plain wording. */
const plan = ref<PlacementPlan | null>(null)
let planSeq = 0
watch(name, async (v) => {
  const view = v.trim()
  if (engine.value !== 'treedb' || view.length < 2) {
    plan.value = null
    return
  }
  const seq = ++planSeq
  const r = await soft('view.plan', { view })
  if (seq !== planSeq) return
  plan.value = r.ok ? (r.r ?? null) : null
})

// The auto option IS the engine's own rule, so say which instance that currently
// means. PostgreSQL: the fewest-views-then-registry-order rule read off the same
// ledger the balancer uses (missing counts fall back to the plain wording).
// treedb: the engine the load-aware pick chooses right now, with the grounds it
// used, plus the honest statement of the rule while the name is still empty.
const autoLabel = computed(() => {
  if (engine.value === 'treedb') {
    const p = plan.value
    if (!p) return t('auto — least loaded of the serving instances')
    const at = p.instance || p.addr || ''
    if (p.exists) return t('auto — ${n} (a tree of this name is already there)', { n: at })
    // The server already said WHICH engine and on what grounds; quote it. A
    // rule other than the normal one (no writable engine reachable) is itself
    // the news, so that sentence goes up instead of a fake count.
    const why = p.rule === 'least loaded' ? (p.detail ?? '') : p.rule
    return why ? t('auto — ${n} (${d})', { n: at, d: why }) : t('auto — ${n}', { n: at })
  }
  const known = instances.value.filter((i) => i.views !== null)
  if (known.length === 0 || !showInstanceField.value) return t('auto — least loaded')
  const min = Math.min(...known.map((i) => i.views as number))
  const winner = known.find((i) => (i.views as number) === min)
  return winner
    ? t('auto — currently ${n} (${v} view(s))', { n: winner.name, v: winner.views })
    : t('auto — least loaded')
})

async function create(): Promise<void> {
  const view = name.value.trim()
  const args: Record<string, unknown> = { view }
  if (showInstanceField.value && instance.value) args.instance = instance.value
  const r = await api('view.create', args)
  // A created view may have taken a name that already had a definition, and the
  // server's own note about that IS the answer — shown, not swallowed.
  ui.toast(typeof r === 'string' ? r : t('view created'))
  catalog.sel = view
  emit('created', view)
  ui.reload()
}
</script>

<template>
  <div v-if="!session.can('view.create')" id="view-new" class="panel">
    <p class="muted">
      {{ t('creating a BASE view is an administrator’s act — it chooses where bytes live.') }}
    </p>
  </div>

  <PanelBox v-else id="view-new" :title="'New base view'">
    <FieldRow for="new-base-name" label="name">
      <input id="new-base-name" v-model="name" placeholder="project-x" />
    </FieldRow>
    <FieldRow v-if="showInstanceField" for="new-base-instance" :label="'instance'">
      <select id="new-base-instance" v-model="instance">
        <option value="">{{ autoLabel }}</option>
        <option v-for="i in instances" :key="i.name" :value="i.name">
          {{ i.views === null ? i.name : `${i.name} — ${i.views} view(s)` }}
        </option>
      </select>
      <template #hint>
        {{
          engine === 'treedb'
            ? t(
                'the tree is created on this treedb server; later writes always follow it. No username or password is asked for: treedb trusts the network address.',
              )
            : t('the new view’s tree is created on this instance; later writes always follow it')
        }}
      </template>
    </FieldRow>
    <p v-if="plan && plan.exists" class="err">
      {{
        t(
          'a tree named ${n} already exists on ${i} — Create would be refused, so pick another name',
          { n: name.trim(), i: plan.instance || plan.addr || '' },
        )
      }}
    </p>
    <p class="muted">
      {{ t('a base view owns physical deletion; lenses, aliases and forks reshape one') }}
    </p>
    <div class="panel-foot">
      <ActionButton :action="create" :label="'Create'" variant="primary" />
    </div>
  </PanelBox>
</template>

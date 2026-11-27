<script setup lang="ts">
/*
  Where the trees actually live — and, on a server-managed registry, how an
  operator changes that from the browser instead of editing a text file.

  Every other page reads the namespace as one list. This one shows the
  deployment underneath it: the backend, its instances (one card each) and the
  placement that says which instance holds which view.

  The card grid is now SHARED between PostgreSQL and treedb, because the two
  differ in what a card MEANS and the card itself says so (see `InstanceCard`):
  PG's first instance carries the control plane and its URI is a handover, not an
  edit; treedb's cards all say `experimental`, none carries the control plane
  (that is local SQLite) and the one matching `--meta-store` is `serving`.

  Write verbs (register / edit URI / relocate / set control / delete) appear only
  when the registry is SERVER-MANAGED (a `file:` book the meta-server adopted at
  start) — an operator-owned registry stays read-only and says why. Each write
  runs the SAME guard the CLI does, so the delete path is preview → confirm and
  every refusal is the server's own sentence, printed not paraphrased.

  Two hard-won rules still live in the load path:
    · a refresh that FAILS must not erase a deployment already on screen — the
      grid stays under a quiet banner, because an error replacing the whole page
      is exactly the "PG cards disappeared" scare;
    · after a create ONE reload runs (`ui.reload()` bumps the nonce); re-calling
      `load()` here too fetched the snapshot twice back to back.
*/
import { computed, ref } from 'vue'
import type { StoreInfo, StoreInstance } from '@/types'
import { useOps } from '@/composables/useOps'
import { usePageLoad } from '@/composables/usePageLoad'
import { useCatalogStore } from '@/stores/catalog'
import { useSessionStore } from '@/stores/session'
import { useUiStore } from '@/stores/ui'
import { instanceKey, treeEngine } from '@/utils/engine'
import { t } from '@/i18n'
import PageHead from '@/components/layout/PageHead.vue'
import PanelBox from '@/components/layout/PanelBox.vue'
import BarRow from '@/components/layout/BarRow.vue'
import KvList from '@/components/layout/KvList.vue'
import KvRow from '@/components/layout/KvRow.vue'
import StateBadge from '@/components/ui/StateBadge.vue'
import ActionButton from '@/components/ui/ActionButton.vue'
import LoadingHint from '@/components/ui/LoadingHint.vue'
import NewBasePanel from '@/components/views/NewBasePanel.vue'
import InstanceCard from '@/components/storage/InstanceCard.vue'
import AddInstancePanel from '@/components/storage/AddInstancePanel.vue'
import EditUriModal from '@/components/storage/EditUriModal.vue'
import RemovePreviewModal from '@/components/storage/RemovePreviewModal.vue'
import RelocateModal from '@/components/storage/RelocateModal.vue'
import SetControlModal from '@/components/storage/SetControlModal.vue'

/* The two banner sentences, as dictionary keys: one for the shape the server
   reports per deployment, one for a treedb that named no serving address. */
const ROUTING_NOTE =
  'treedb places new trees on the least-loaded writable engine among ${n} serving address(es); other entries are inventory, not placement targets'
const NO_ROUTING_NOTE =
  'treedb is EXPERIMENTAL: the control plane (definitions, keys, accounts) stays in local SQLite and this registry is inventory, not a router.'

const { api } = useOps()
const catalog = useCatalogStore()
const session = useSessionStore()
const ui = useUiStore()

const info = ref<StoreInfo | null>(null)
const names = ref<string[]>([])
/** Which cards have their connection string hidden. Local to this visit. */
const hidden = ref<Set<string>>(new Set())
/** The instance a new base is being created on (null = form closed). */
const createOn = ref<string | null>(null)
/** The register-instance form. */
const showAdd = ref(false)
/** The four guarded dialogs, each keyed to the instance it acts on. */
const editTarget = ref<StoreInstance | null>(null)
const removeTarget = ref<StoreInstance | null>(null)
const relocateTarget = ref<StoreInstance | null>(null)
const setControlTarget = ref<StoreInstance | null>(null)

async function load(): Promise<void> {
  const [snapshot, physical] = await Promise.all([api('store.info', {}), api('physical.list', {})])
  info.value = snapshot
  names.value = physical ?? []
}

const { pending, error } = usePageLoad(load)

const storage = computed(() => info.value?.storage ?? null)
/*
  The engine, from the ONE field that names it (`metadataBackend`, confirmed by
  `storage.backend`). `info.backend` is deliberately NOT part of this decision:
  it is the CONTROL-plane backend, and a treedb deployment keeps its control
  plane in local SQLite — reading it as the engine made this page choose the
  SQLite layout on a treedb box, and the reverse mistake shows a SQLite
  deployment the treedb cards it does not have. `unknown` is shown as unknown
  rather than guessed into one of the three.
*/
const engine = computed(() => treeEngine(info.value))
const isSqlite = computed(() => engine.value === 'sqlite')
const isPg = computed(() => engine.value === 'postgres')
const isTreedb = computed(() => engine.value === 'treedb')
const instances = computed(() => storage.value?.instances ?? [])
const managed = computed(() => storage.value?.managed === true)
const pendingControl = computed(() => storage.value?.pendingControl ?? null)
const multi = computed(() => instances.value.length > 1)

/** The treedb banner's sentence. The server owns the FACT — how many addresses
    it routes to — and the console owns the wording an operator reads, so the
    count is lifted out of the server's English and re-rendered through the
    dictionary. An unrecognized shape shows the server's own text rather than a
    sentence we invented. */
const routingNoteText = computed(() => {
  const note = storage.value?.routingNote
  if (!note) return t(NO_ROUTING_NOTE)
  const n = /^treedb places new trees on the least-loaded writable engine among (\d+) serving address\(es\)/.exec(note)
  return n
    ? t(ROUTING_NOTE, { n: n[1] })
    : note
})

/** The views resolved onto one card, keyed by `name@uri`. The registry
    addresses an instance by NAME and an older treedb adoption wrote every
    serving address as `primary` — one key, two servers, and the second card's
    view count overwrote the first. The address is what distinguishes them. */
const perInstance = computed(() => {
  const map = new Map<string, string[]>()
  for (const inst of instances.value) map.set(instanceKey(inst), [])
  if (isTreedb.value) {
    for (const inst of instances.value) {
      map.set(instanceKey(inst), inst.viewNames ?? [])
    }
    return map
  }
  for (const view of names.value) {
    const home = catalog.instanceOf(view)
    if (!home) continue
    const inst = instances.value.find((i) => i.name === home)
    if (inst) map.get(instanceKey(inst))?.push(view)
  }
  return map
})
const viewsFor = (inst: StoreInstance): string[] => perInstance.value.get(instanceKey(inst)) ?? []

/** Names with no placement row (a multi-instance PG view is hash-routed). */
const unplaced = computed(() =>
  isPg.value && multi.value ? names.value.filter((v) => !storage.value?.placement[v]) : [],
)

/*
  treedb's ledger. There is no placement table under `treedb://`, so PG's
  placed/unplaced pair does not transfer: a tree's home is whatever engine
  reports holding it. The four buckets are instead the four ways that home
  CANNOT BE NAMED — an address with no registry name, a registry row this server
  does not route to, an engine that did not answer, and one name on two engines.
  The server computes them from the SAME sweep the balancer reads, so the page
  and the pick cannot disagree.
*/
const ledger = computed(() => (isTreedb.value ? (storage.value?.ledger ?? null) : null))
const ledgerNotes = computed(() => {
  const l = ledger.value
  if (!l) return []
  const out: { text: string; bad: boolean }[] = []
  if (l.unnamedHome.length)
    out.push({
      text: t(
        '${n} serving address(es) hold trees no registry name points at: ${list} — the console cannot show them and a named create cannot select them.',
        { n: l.unnamedHome.length, list: l.unnamedHome.join(', ') },
      ),
      bad: false,
    })
  if (l.offServing.length)
    out.push({
      text: t(
        '${n} registered instance(s) are inventory only — this server does not route to ${list}, so their trees are not readable here.',
        { n: l.offServing.length, list: l.offServing.join(', ') },
      ),
      bad: false,
    })
  if (l.unreachable.length)
    out.push({
      text: t('${n} serving address(es) did not answer: ${list}.', {
        n: l.unreachable.length,
        list: l.unreachable.join(', '),
      }),
      bad: true,
    })
  if (l.duplicates.length)
    out.push({
      text: t(
        '${n} view name(s) sit on two reachable engines (${list}) — one tree per name is the contract, and reads of these names are refused until it is reconciled.',
        { n: l.duplicates.length, list: l.duplicates.join(', ') },
      ),
      bad: true,
    })
  return out
})

/** The busiest card sets the meter's full length, so the bars compare across
    the grid rather than each filling alone. */
const maxCount = computed(() => {
  let m = 0
  for (const list of perInstance.value.values()) m = Math.max(m, list.length)
  return m
})
const placedTotal = computed(() => names.value.length - unplaced.value.length)

/** Every instance a relocated view could move onto (the source excluded). */
const relocateTargets = computed(() =>
  relocateTarget.value
    ? instances.value.filter((i) => i.name !== relocateTarget.value?.name)
    : [],
)

function toggleHidden(key: string): void {
  const next = new Set(hidden.value)
  if (next.has(key)) next.delete(key)
  else next.add(key)
  hidden.value = next
}
function refresh(): void {
  ui.reload()
}
/** NewBasePanel already fired `ui.reload()`; only close the form here. */
function created(): void {
  createOn.value = null
}
</script>

<template>
  <div id="page-storage">
    <PageHead
      kicker="Metadata"
      title="Storage"
      :desc="
        t('Where the namespace physically lives — the backend, its instances and the placement of each view.')
      "
    >
      <template #actions>
        <StateBadge v-if="engine !== 'unknown'" :label="engine" tone="admin" />
        <ActionButton :action="refresh" :label="'refresh'" variant="link" />
      </template>
    </PageHead>

    <!-- A failure only takes over the page when there is nothing loaded yet;
         with data on screen it drops to a banner so the deployment stays put. -->
    <LoadingHint v-if="pending && !info" />
    <p v-else-if="error && !info" class="err">{{ error }}</p>

    <template v-else-if="info">
      <p v-if="error" class="banner-warn">
        {{ t('Refresh failed — showing the last loaded snapshot.') }}
        <button type="button" class="link" @click="refresh">{{ t('retry') }}</button>
      </p>

      <!-- ── SQLite: one embedded store per view, no instances to choose ── -->
      <PanelBox v-if="isSqlite" :title="'Embedded SQLite'">
        <KvList>
          <KvRow label="base directory" :value="info.base" mono />
          <KvRow label="views" :value="info.views ?? names.length" />
          <KvRow label="definitions" :value="info.definitions" />
          <KvRow label="resident engines" :value="info.residentEngines" />
          <KvRow label="resident views" :value="info.residentViews" />
        </KvList>
        <p class="muted">
          {{
            t(
              'One database file per view under <base>/meta/. High availability ships VACUUM INTO snapshots between nodes — there is no instance to place a view on.',
            )
          }}
        </p>
      </PanelBox>

      <!-- ── PostgreSQL / treedb: the instance grid ── -->
      <template v-else-if="isPg || isTreedb">
        <!-- treedb is honest about its limits up front, not buried in a card.
             The sentence belongs to the dictionary, not to the wire: the server
             owns the FACT (how many addresses it routes to), the console owns the
             wording an operator reads — a passthrough of `routingNote` used to
             leave this banner in English on a Chinese page. The count is lifted
             out of the server's sentence and re-inserted, so a future change of
             phrasing server-side cannot silently change the number shown here;
             an unrecognized shape falls back to the server's own text. -->
        <div v-if="isTreedb" class="exp-banner">
          <StateBadge tone="warn" :label="t('experimental')" />
          <span>{{ routingNoteText }}</span>
        </div>

        <!-- One number per fact: instances, views, placed, still hash-routed. -->
        <BarRow :title="'This deployment'">
          <span class="muted">{{ t('counts read from the registry the balancer uses') }}</span>
        </BarRow>
        <div class="storage-stats">
          <div class="sstat">
            <span class="sstat-n">{{ instances.length }}</span>
            <span class="sstat-l">{{ t('instances') }}</span>
          </div>
          <div class="sstat">
            <span class="sstat-n">{{ names.length }}</span>
            <span class="sstat-l">{{ t('views') }}</span>
          </div>
          <div v-if="isPg" class="sstat">
            <span class="sstat-n">{{ placedTotal }}</span>
            <span class="sstat-l">{{ t('placed') }}</span>
          </div>
          <div v-if="isTreedb && ledger" class="sstat">
            <span class="sstat-n">{{ ledger.placedTrees }}</span>
            <span class="sstat-l">{{ t('placed') }}</span>
          </div>
          <div v-if="unplaced.length" class="sstat warn">
            <span class="sstat-n">{{ unplaced.length }}</span>
            <span class="sstat-l">{{ t('unplaced') }}</span>
          </div>
          <div v-if="ledger?.unnamedHome.length" class="sstat warn">
            <span class="sstat-n">{{ ledger.unnamedHome.length }}</span>
            <span class="sstat-l">{{ t('unnamed home(s)') }}</span>
          </div>
          <div v-if="ledger?.offServing.length" class="sstat warn">
            <span class="sstat-n">{{ ledger.offServing.length }}</span>
            <span class="sstat-l">{{ t('inventory only') }}</span>
          </div>
          <div v-if="ledger?.unreachable.length" class="sstat err">
            <span class="sstat-n">{{ ledger.unreachable.length }}</span>
            <span class="sstat-l">{{ t('unreachable') }}</span>
          </div>
          <div v-if="ledger?.duplicates.length" class="sstat err">
            <span class="sstat-n">{{ ledger.duplicates.length }}</span>
            <span class="sstat-l">{{ t('name clash(es)') }}</span>
          </div>
        </div>

        <p v-if="storage?.instanceError" class="err">{{ storage.instanceError }}</p>
        <div v-if="ledgerNotes.length" class="ledger-notes">
          <p v-for="n in ledgerNotes" :key="n.text" :class="n.bad ? 'err' : 'muted'">
            {{ n.text }}
          </p>
        </div>

        <!-- A staged PG control handover stays visible until the restart lands
             it, so the operator knows the badge is about to move and why. -->
        <div v-if="isPg && pendingControl" class="banner-warn">
          {{
            t('A control-plane handover to ${name} is staged — restart the meta-server to apply it.', {
              name: pendingControl,
            })
          }}
        </div>

        <!-- The verb opens the zone it acts on, and the form appears under it
             in place — not on a separate screen, and not after the grid it is
             meant to feed. -->
        <BarRow :title="'Instances'">
          <ActionButton
            v-if="managed && session.can('storage.add')"
            :action="() => (showAdd = true)"
            :label="'Register instance'"
            variant="mini"
          />
        </BarRow>
        <AddInstancePanel v-if="showAdd && managed" :is-pg="isPg" @close="showAdd = false" />

        <div v-if="!instances.length" class="muted">
          {{ t('No instances are registered yet.') }}
        </div>

        <div class="inst-grid">
          <InstanceCard
            v-for="inst in instances"
            :key="instanceKey(inst)"
            :inst="inst"
            :backend="isPg ? 'postgres' : 'treedb'"
            :managed="managed"
            :views="viewsFor(inst)"
            :max-count="maxCount"
            :can-hide="!!inst.uri"
            :hidden="hidden.has(instanceKey(inst))"
            @addbase="createOn = $event"
            @edit="editTarget = $event"
            @relocate="relocateTarget = $event"
            @setcontrol="setControlTarget = $event"
            @remove="removeTarget = $event"
            @toggle="toggleHidden(instanceKey($event))"
          />
        </div>

        <p v-if="!managed && (isPg || isTreedb)" class="muted">
          {{
            t(
              'This registry is owned by a --meta-store file the operator edits by hand — the page is read-only here.',
            )
          }}
        </p>

        <!-- PG: the unplaced set can be the whole pre-migration namespace. It is
             a fact, not a headline: one line, expandable when someone asks. -->
        <details v-if="unplaced.length" class="unplaced">
          <summary>
            {{
              t('${n} view(s) have no placement row and route by name hash.', {
                n: unplaced.length,
              })
            }}
          </summary>
          <p class="mono">{{ unplaced.join(' · ') }}</p>
        </details>

        <NewBasePanel
          v-if="createOn && session.can('view.create')"
          :key="createOn"
          :instance="createOn"
          @created="created"
        />
        <p v-if="createOn && session.can('view.create')" class="muted">
          <button type="button" class="link" @click="createOn = null">{{ t('close') }}</button>
        </p>

        <BarRow v-if="isPg" :title="'Placement'" />
        <p v-if="isPg" class="muted">
          {{
            t(
              'A new view lands on the instance with the fewest placed views unless the caller names one. A view’s tree lives on exactly one instance; forks and grafts cannot cross instances.',
            )
          }}
        </p>
        <p v-else-if="isTreedb" class="muted">
          {{ t('control plane: ${where}', { where: storage?.controlPlane ?? t('local SQLite') }) }}
        </p>
      </template>

      <!-- ── unknown backend: still name what the server said ── -->
      <PanelBox v-else :title="'Storage'">
        <KvList>
          <KvRow label="tree engine" :value="info.metadataBackend || '—'" />
          <KvRow label="control plane" :value="info.backend || '—'" />
          <KvRow label="describe" :value="info.describe" />
          <KvRow label="base" :value="info.base" mono />
        </KvList>
      </PanelBox>

      <PanelBox :title="'Capacity'">
        <KvList>
          <KvRow label="live sessions" :value="info.liveSessions" />
          <KvRow label="resident engines" :value="info.residentEngines" />
          <KvRow label="resident views" :value="info.residentViews" />
          <KvRow label="audit retention" :value="info.auditRetention" />
        </KvList>
      </PanelBox>

      <p v-if="!storage" class="muted">
        {{ t('Instance details are an administrator’s view.') }}
      </p>
    </template>

    <!-- The guarded write dialogs, mounted only for the instance they act on. -->
    <EditUriModal v-if="editTarget" :inst="editTarget" :is-pg="isPg" @close="editTarget = null" />
    <RemovePreviewModal
      v-if="removeTarget"
      :inst="removeTarget"
      :is-pg="isPg"
      @close="removeTarget = null"
    />
    <RelocateModal
      v-if="relocateTarget"
      :inst="relocateTarget"
      :views="viewsFor(relocateTarget)"
      :targets="relocateTargets"
      @close="relocateTarget = null"
    />
    <SetControlModal
      v-if="setControlTarget"
      :inst="setControlTarget"
      @close="setControlTarget = null"
    />
  </div>
</template>

<style scoped>
.exp-banner {
  display: flex;
  gap: var(--sp-2);
  align-items: center;
  margin: 0 0 var(--sp-4);
  padding: var(--sp-2) var(--sp-3);
  border-left: 3px solid var(--warn);
  background: var(--bg-subtle);
  border-radius: var(--r-sm);
  font-size: 0.8125rem;
}
.inst-toolbar {
  margin: var(--sp-3) 0;
}
</style>

<script setup lang="ts">
/*
  Where the trees actually live.

  Every other page reads the namespace as one list. This one shows the
  deployment underneath it: the backend, the PostgreSQL book (one card per
  named instance, the first carrying the control plane) and the placement map
  that says which instance holds which view.

  The shapes are deliberately different per backend. SQLite is one embedded
  store per view, treedb is one experimental tree server, and only PostgreSQL
  can have several instances — rendering a one-item "grid" for the first two
  would describe the deployment as something it is not.

  Connection strings are shown VERBATIM, credentials included. `store.info` is
  Super-only and this page exists so an operator can verify what is configured;
  a masked string cannot be verified. The Hide toggle is a shoulder-surfing
  affordance, not a security boundary.

  Two hard-won rules live in the load path below:
    · a refresh that FAILS must not erase a deployment already on screen. The
      grid stays, with a quiet banner — the alternative (an error replacing the
      whole page) makes a transient blip look like the instances vanished, which
      is exactly the "PG cards disappeared after I created a view" scare;
    · after a create, ONE reload runs. `NewBasePanel` already bumps `ui.reload()`;
      calling `run()` here too fetched the snapshot twice back to back.
*/
import { computed, ref } from 'vue'
import type { StoreInfo } from '@/types'
import { useOps } from '@/composables/useOps'
import { usePageLoad } from '@/composables/usePageLoad'
import { useCatalogStore } from '@/stores/catalog'
import { useNav } from '@/composables/useNav'
import { useSessionStore } from '@/stores/session'
import { useUiStore } from '@/stores/ui'
import { t } from '@/i18n'
import PageHead from '@/components/layout/PageHead.vue'
import PanelBox from '@/components/layout/PanelBox.vue'
import BarRow from '@/components/layout/BarRow.vue'
import KvList from '@/components/layout/KvList.vue'
import KvRow from '@/components/layout/KvRow.vue'
import StateBadge from '@/components/ui/StateBadge.vue'
import ActionButton from '@/components/ui/ActionButton.vue'
import CopyButton from '@/components/ui/CopyButton.vue'
import LoadingHint from '@/components/ui/LoadingHint.vue'
import NewBasePanel from '@/components/views/NewBasePanel.vue'

const { api } = useOps()
const catalog = useCatalogStore()
const session = useSessionStore()
const ui = useUiStore()
const { openInstance } = useNav()

const info = ref<StoreInfo | null>(null)
const names = ref<string[]>([])
/** Which cards have their connection string hidden. Local to this visit. */
const hidden = ref<Set<string>>(new Set())
/** The instance a new base is being created on (null = form closed). */
const createOn = ref<string | null>(null)

/** Segments in an instance's load meter — a class-only gauge, because the
    gateway's `style-src 'self'` CSP forbids a runtime inline `style` width. */
const SEGS = 8

async function load(): Promise<void> {
  const [snapshot, physical] = await Promise.all([api('store.info', {}), api('physical.list', {})])
  info.value = snapshot
  names.value = physical ?? []
}

const { pending, error } = usePageLoad(load)

const storage = computed(() => info.value?.storage ?? null)
const backend = computed(() => storage.value?.backend ?? info.value?.backend ?? 'sqlite')
const isPg = computed(() => backend.value === 'postgres')
const instances = computed(() => storage.value?.instances ?? [])
const multi = computed(() => instances.value.length > 1)

/** The views placed on each instance, in namespace order. */
const perInstance = computed(() => {
  const map = new Map<string, string[]>()
  for (const inst of instances.value) map.set(inst.name, [])
  for (const view of names.value) {
    const home = catalog.instanceOf(view)
    if (home) map.get(home)?.push(view)
  }
  return map
})

/** Names with no placement row (a multi-instance view is hash-routed). */
const unplaced = computed(() =>
  multi.value ? names.value.filter((v) => !storage.value?.placement[v]) : [],
)

/** The busiest instance sets the meter's full length, so the bars read as a
    comparison across cards rather than eight near-empty gauges. */
const maxCount = computed(() => {
  let m = 0
  for (const list of perInstance.value.values()) m = Math.max(m, list.length)
  return m
})
function filledFor(name: string): number {
  const c = (perInstance.value.get(name) ?? []).length
  if (!maxCount.value) return 0
  return c === 0 ? 0 : Math.max(1, Math.round((c / maxCount.value) * SEGS))
}
/** Views that landed on some instance (the rest hash-route by name). */
const placedTotal = computed(() => names.value.length - unplaced.value.length)

function toggleHidden(name: string): void {
  const next = new Set(hidden.value)
  if (next.has(name)) next.delete(name)
  else next.add(name)
  hidden.value = next
}

function visibleUri(uri: string | null): string {
  return uri ?? '—'
}

function refresh(): void {
  ui.reload()
}
/** The panel already fired `ui.reload()` for the new view; all that is left
    here is to close the form. Re-running `load()` would double the fetch. */
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
        <StateBadge v-if="backend" :label="backend" tone="admin" />
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
      <PanelBox v-if="backend === 'sqlite'" :title="'Embedded SQLite'">
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

      <!-- ── treedb: experimental tree server, control plane still local ── -->
      <PanelBox v-else-if="backend === 'treedb'" :title="'Experimental tree engine'">
        <KvList>
          <KvRow label="address" :value="instances[0]?.uri ?? null" mono />
          <KvRow label="control plane" :value="t('local SQLite')" />
          <KvRow label="views" :value="info.views ?? names.length" />
          <KvRow label="definitions" :value="info.definitions" />
        </KvList>
        <p class="muted">
          {{
            t(
              'Trees live in the treedb server; definitions, keys and accounts stay in the local SQLite control plane. This backend is experimental and runs as a single instance.',
            )
          }}
        </p>
      </PanelBox>

      <!-- ── PostgreSQL: the instance book ── -->
      <template v-else-if="isPg">
        <!-- One number per fact: how many instances, how many views, how many
             placed, how many still hash-routed. The backend's `describe` string
             said the same in prose, so it is gone from here. -->
        <div class="storage-stats">
          <div class="sstat">
            <span class="sstat-n">{{ instances.length }}</span>
            <span class="sstat-l">{{ t('instances') }}</span>
          </div>
          <div class="sstat">
            <span class="sstat-n">{{ names.length }}</span>
            <span class="sstat-l">{{ t('views') }}</span>
          </div>
          <div class="sstat">
            <span class="sstat-n">{{ placedTotal }}</span>
            <span class="sstat-l">{{ t('placed') }}</span>
          </div>
          <div v-if="unplaced.length" class="sstat warn">
            <span class="sstat-n">{{ unplaced.length }}</span>
            <span class="sstat-l">{{ t('unplaced') }}</span>
          </div>
        </div>

        <p v-if="storage?.instanceError" class="err">{{ storage.instanceError }}</p>

        <div class="inst-grid">
          <article
            v-for="inst in instances"
            :key="inst.name"
            class="inst-card"
            :class="{ control: inst.control }"
          >
            <header class="inst-head">
              <h3 class="mono">{{ inst.name }}</h3>
              <StateBadge
                v-if="inst.control"
                tone="admin"
                :label="t('control plane')"
                :title="t('keys, definitions, accounts, audit and placement live here')"
              />
            </header>

            <div class="inst-uri">
              <div class="inst-uri-label">
                <span>{{ t('connection') }}</span>
                <span class="cred">{{ t('contains credentials') }}</span>
              </div>
              <code v-if="!hidden.has(inst.name)">{{ visibleUri(inst.uri) }}</code>
              <code v-else class="masked">{{ '••••••••••••••••••••' }}</code>
              <div class="inst-uri-actions">
                <button
                  type="button"
                  class="mini"
                  :aria-pressed="hidden.has(inst.name)"
                  @click="toggleHidden(inst.name)"
                >
                  {{ hidden.has(inst.name) ? t('Show') : t('Hide') }}
                </button>
                <CopyButton v-if="inst.uri" :text="inst.uri" />
              </div>
            </div>

            <!-- One load line per card: the count of views this instance holds,
                 with a gauge showing it against the busiest instance. The old
                 card printed two near-identical counts here. -->
            <div class="inst-load">
              <div class="inst-meter" :aria-hidden="true">
                <span
                  v-for="i in SEGS"
                  :key="i"
                  :class="['seg', i <= filledFor(inst.name) ? 'on' : '']"
                ></span>
              </div>
              <span class="inst-count">
                <b>{{ (perInstance.get(inst.name) ?? []).length }}</b>
                {{ t('view(s) here') }}
              </span>
            </div>

            <!-- A registry's first card can hold the whole pre-migration
                 namespace; show the head of the list and hand the rest to the
                 filtered Views page rather than growing the card to a column. -->
            <ul class="inst-views">
              <li v-for="v in (perInstance.get(inst.name) ?? []).slice(0, 8)" :key="v" class="mono">
                {{ v }}
              </li>
              <li v-if="!(perInstance.get(inst.name) ?? []).length" class="muted">
                {{ t('no view is placed here yet') }}
              </li>
              <li
                v-else-if="(perInstance.get(inst.name) ?? []).length > 8"
                class="more"
                :title="t('open the filtered list for the rest')"
              >
                +{{ (perInstance.get(inst.name) ?? []).length - 8 }}
              </li>
            </ul>

            <div class="inst-actions">
              <button type="button" @click="openInstance(inst.name)">
                {{ t('Open these views') }}
              </button>
              <button
                v-if="session.can('view.create')"
                type="button"
                class="primary"
                @click="createOn = inst.name"
              >
                {{ t('New base here') }}
              </button>
            </div>
          </article>
        </div>

        <!-- The unplaced set can be the whole pre-migration namespace. It is a
             fact, not a headline: one line, expandable when someone asks. -->
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
          :pg-instance="createOn"
          @created="created"
        />
        <p v-if="createOn && session.can('view.create')" class="muted">
          <button type="button" class="link" @click="createOn = null">{{ t('close') }}</button>
        </p>

        <BarRow :title="'Placement'" />
        <p class="muted">
          {{
            t(
              'A new view lands on the instance with the fewest placed views unless the caller names one. A view’s tree lives on exactly one instance; forks and grafts cannot cross instances.',
            )
          }}
        </p>
      </template>

      <!-- ── unknown backend: still name what the server said ── -->
      <PanelBox v-else :title="'Storage'">
        <KvList>
          <KvRow label="backend" :value="backend" />
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
  </div>
</template>

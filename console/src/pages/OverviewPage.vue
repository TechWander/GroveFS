<script setup lang="ts">
/*
  The first page: what this deployment is, how far your account reaches, and what
  the governance plane did last.

  Two accounts, two pages. An administrator sees the store's own counters and a
  panel of the facts an operator quotes in a ticket — the backend WITH its
  describe line, the namespace SPLIT into trees and definitions, credentials
  SPLIT into live and expired, and which process answered. A delegated account
  sees only the base views it governs, because a deployment-wide count is not
  that account's to read; the server's `whoami` already carries the list, so the
  page asks for nothing it is not entitled to.

  Every block is loaded separately and every block says what it could not load:
  a page that is part answer and one refusal sentence needs the sentence where it
  belongs, not as a toast over a half-blank screen.

  The blocks also ARRIVE separately, which is what the hand-written page did: it
  appended the cards as soon as `info` and `store.info` had answered and left the
  recent-calls table to fill in after its own call. Gating the whole page on the
  slowest of the three would turn a 180 ms screen into a 260 ms one for no other
  reason than tidiness, so the gate is per block and each block still has its own
  loading line.
*/
import { computed, ref } from 'vue'
import type { Soft } from '@/api/ops'
import type { AuditRow, InfoResult, StoreInfo } from '@/types'
import { useOps } from '@/composables/useOps'
import { usePageLoad } from '@/composables/usePageLoad'
import { useSessionStore } from '@/stores/session'
import { useNav } from '@/composables/useNav'
import { t } from '@/i18n'
import PageHead from '@/components/layout/PageHead.vue'
import BarRow from '@/components/layout/BarRow.vue'
import StatLine from '@/components/layout/StatLine.vue'
import LoadingHint from '@/components/ui/LoadingHint.vue'
import StatCard from '@/components/overview/StatCard.vue'
import AuditTable from '@/components/audit/AuditTable.vue'

const session = useSessionStore()
const { api, soft } = useOps()
const { openView, to } = useNav()

const info = ref<InfoResult | null>(null)
const store = ref<StoreInfo | null>(null)
const calls = ref<Soft<AuditRow[]> | null>(null)
/** A base this account governs, and how many names resolve through it — `null`
    when the server would not answer for it. */
const reach = ref<{ name: string; derived: number | null }[]>([])
/** Whether that list is finished, not just started: an empty list while the
    per-base calls are still in flight would tell a delegated account it governs
    nothing, which is a different sentence from the true one. */
const reachDone = ref(false)

async function load(): Promise<void> {
  info.value = null
  store.value = null
  calls.value = null
  reach.value = []
  reachDone.value = false
  info.value = await api('info', {})
  if (session.isSuper) {
    store.value = await api('store.info', {})
  } else {
    for (const b of session.bases.slice(0, 50)) {
      const s = await soft('view.stats', { view: b })
      reach.value.push({ name: b, derived: s.ok ? (s.r?.family ?? []).length : null })
    }
    reachDone.value = true
  }
  calls.value = await soft('apikey.calls', { limit: 8 })
}

const { pending, error } = usePageLoad(load)

/** What the cards need, and nothing else: an administrator's eight numbers come
    from `store.info`, a delegated account's one card comes from `whoami`. */
const cardsReady = computed(() => info.value !== null && (!session.isSuper || store.value !== null))

/** The one control on this page that navigates rather than fetches. */
function toAudit(): void {
  void to('audit')
}

/** A governed base is a jump: the next question is what that name looks like. */
function openBase(name: string): void {
  void openView(name)
}

/** `base` only means "the trees live here" on a SQLite backend — on PG and
    treedb it holds no trees at all, so the line names what it actually is.
    Keyed off `metadataBackend` (where the TREES live), NOT `backend` (the
    CONTROL-plane backend): a treedb deployment's control plane is still local
    SQLite, so `backend` says "sqlite" while its trees sit in the engine —
    trusting `backend` would mislabel a treedb box "metadata root". The
    vocabulary lives here because a comparison inside a bound prop reads to the
    i18n audit as an untranslated display string. */
const baseLabel = computed(() =>
  store.value?.metadataBackend === 'sqlite' ? 'metadata root' : 'local anchor (--meta-dir)',
)
</script>

<template>
  <PageHead
    kicker="Deployment"
    title="Overview"
    desc="Deployment snapshot, your reach and the latest governance activity."
  />

  <p v-if="error" class="err">{{ error }}</p>
  <LoadingHint v-else-if="!cardsReady && pending" />

  <template v-else-if="info">
    <div class="cards">
      <StatCard
        label="backend"
        :value="info.backend"
        :sub="'one store for metadata and content'"
        tone="mute"
      />
      <StatCard
        label="signed in as"
        :value="info.actor || session.account"
        :sub="info.role || ''"
        tone="mute"
      />

      <template v-if="store">
        <StatCard
          label="base views"
          :value="store.views"
          :sub="'the trees mounts open'"
          page="views"
        />
        <StatCard
          label="definitions"
          :value="store.definitions"
          :sub="'forks, aliases and lenses'"
          page="views"
        />
        <StatCard
          label="accounts"
          :value="store.accounts"
          :sub="'principles on the governance plane'"
          page="accounts"
        />
        <StatCard
          label="live keys"
          :value="store.liveKeys"
          :sub="store.expiredKeys || 0 ? '${n} expired and refused' : 'none expired'"
          :vars="{ n: store.expiredKeys }"
          page="keys"
          :tone="store.expiredKeys || 0 ? 'warn' : 'ok'"
        />
        <StatCard
          label="mount sessions"
          :value="store.liveSessions"
          :sub="'clients reading trees now'"
          page="views"
        />
        <StatCard
          label="open trees"
          :value="store.residentEngines"
          :sub="'${n} names resident here'"
          :vars="{ n: store.residentViews }"
          page="views"
        />
      </template>

      <StatCard
        v-else
        label="base views you govern"
        :value="session.bases.length"
        :sub="'and everything derived from them'"
        page="views"
      />
    </div>

    <template v-if="store">
      <BarRow :title="'This deployment'">
        <span class="muted">{{ t('read live from the meta-server that answered this page') }}</span>
      </BarRow>
      <div class="detail">
        <div class="grid2">
          <StatLine label="storage backend">
            <b>{{ info.backend || '—' }}</b>
            <span class="muted">· {{ store.describe || '' }}</span>
          </StatLine>
          <StatLine :label="baseLabel">
            <code>{{ store.base || '—' }}</code>
            <span v-if="store.backendLayout && store.metadataBackend !== 'sqlite'" class="muted">
              · {{ store.backendLayout }}
            </span>
          </StatLine>
          <StatLine label="server role">
            <b>{{ store.role || '—' }}</b>
            <span class="muted">{{ t('the process this console is talking to') }}</span>
          </StatLine>
          <!-- The cards above already carry every counter this page knows. What
               stays here is the substrate an operator quotes in a ticket — where
               the bytes live, which process answered, and how long the trail is
               kept — so the panel does not repeat the row of figures it sits
               under. -->
          <StatLine label="audit trail">
            <b v-if="store.auditRetention">
              {{ t('rows older than ${w} are swept', { w: store.auditRetention }) }}
            </b>
            <b v-else>{{ t('every row is kept until an operator purges it') }}</b>
          </StatLine>
        </div>
        <!-- A PG deployment's real question is WHERE each tree lives, and the
             placement table answers it exactly — the same ledger the balancer
             and the create-view dropdown read, so the three faces cannot
             drift apart. SQLite keeps one db file per view under the base and
             treedb one tree per view in its server: nothing to spread, so no
             ledger. -->
        <ul v-if="store.pgInstances" class="list">
          <li v-for="i in store.pgInstances.instances" :key="i.name" class="static">
            <b>{{ i.name }}</b>
            <span v-if="i.control" class="muted">· {{ t('control plane') }}</span>
            <span class="muted">
              ·
              {{
                i.views === null
                  ? t('placement count unreadable')
                  : t('${v} view(s) placed', { v: i.views })
              }}
            </span>
          </li>
          <li v-if="store.pgInstances.unplaced" class="static muted">
            {{
              t(
                '${n} view(s) have no placement row and are hash-routed',
                { n: store.pgInstances.unplaced },
              )
            }}
          </li>
          <li v-if="store.pgInstances.dangling" class="static err">
            {{
              t(
                '${n} view(s) point at an instance the registry no longer defines — their mount is refused until the registry or placement is fixed',
                { n: store.pgInstances.dangling },
              )
            }}
          </li>
        </ul>
        <p class="muted">
          {{
            t(
              'Every count is a snapshot of the instant this page loaded; anything created elsewhere appears after a refresh.',
            )
          }}
        </p>
      </div>
    </template>

    <template v-else>
      <BarRow :title="'Your reach'" />
      <p class="muted">
        {{
          t(
            'Deployment-wide counts belong to an administrator. You govern these base views and everything derived from them:',
          )
        }}
      </p>
      <LoadingHint v-if="!reachDone" />
      <ul v-else class="list">
        <li v-if="!reach.length" class="static muted">{{ t('no base views delegated to you') }}</li>
        <li
          v-for="b in reach"
          :key="b.name"
          role="button"
          tabindex="0"
          @click="openBase(b.name)"
          @keydown="
            (ev) => {
              if (ev.key === 'Enter' || ev.key === ' ') {
                ev.preventDefault()
                openBase(b.name)
              }
            }
          "
        >
          <span class="mono">{{ b.name }}</span>
          <span class="kind">
            {{ b.derived === null ? t('unreachable') : t('${n} derived', { n: b.derived }) }}
          </span>
        </li>
      </ul>
    </template>

    <BarRow :title="'Recent governance calls'">
      <button v-if="calls?.ok" type="button" class="mini" @click="toAudit">
        {{ t('open the audit page') }}
      </button>
    </BarRow>
    <LoadingHint v-if="!calls" />
    <p v-else-if="!calls.ok" class="err">{{ calls.error }}</p>
    <AuditTable v-else :rows="calls.r ?? []" />
  </template>
</template>

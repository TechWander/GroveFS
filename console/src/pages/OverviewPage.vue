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
import { treeEngine, type Engine } from '@/utils/engine'
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
    Keyed off the TREE ENGINE (`metadataBackend`), NOT `backend` (the
    CONTROL-plane backend): a treedb deployment's control plane is still local
    SQLite, so `backend` says "sqlite" while its trees sit in the engine —
    trusting `backend` would mislabel a treedb box "metadata root". The
    vocabulary lives here because a comparison inside a bound prop reads to the
    i18n audit as an untranslated display string. */
const engine = computed<Engine>(() => treeEngine(store.value))
const baseLabel = computed(() =>
  engine.value === 'sqlite' ? 'metadata root' : 'local anchor (--meta-dir)',
)

/*
  The engine, and the words that go with it.

  The first card used to print `info.backend` under the label "backend" with the
  SQLite sentence underneath, so a treedb box announced itself as
  "backend: sqlite / one store for metadata and content" two feet away from a
  Storage page showing two treedb servers. The card now answers the question an
  operator is actually asking — WHERE the trees are — and says the control
  plane's engine separately, in its own row. A delegated account cannot read
  `store.info`, so it gets the honest version of what it CAN see: the control
  plane, named as such.
*/
const ENGINE_SUB: Record<Engine, string> = {
  sqlite: 'one store for metadata and content',
  postgres: 'one schema per view, instances from the registry',
  treedb: 'trees in the treedb servers; control plane in local SQLite',
  unknown: 'the server named a backend this console does not know',
}
/** What to PRINT as the engine: the server's own word when it has one, an em
    dash when it has none — never a name this console invented. */
const engineText = computed(() =>
  engine.value === 'unknown' ? store.value?.metadataBackend || '—' : engine.value,
)
const engineCard = computed(() =>
  store.value
    ? { label: 'tree engine', value: engineText.value, sub: ENGINE_SUB[engine.value] }
    : { label: 'control plane', value: info.value?.backend || '—', sub: 'the store that answered this page' },
)

/*
  The sentence the two rows cannot say.

  `tree engine: treedb` beside `control plane: sqlite` reads as a contradiction,
  and the operator's question — why does it still say sqlite? — is only
  answerable in prose. One sentence per engine, keyed off the TREE ENGINE, shown
  ABOVE the two rows so the reader has the frame before the figures. The keys are
  looked up through this map rather than passed to `t()` as a literal, so
  `zh.ts` carries them by hand — the same way `ENGINE_SUB` does.
*/
const ENGINE_NOTE: Record<Engine, string> = {
  sqlite:
    'One store does everything here: the trees, the definitions, the credentials, the accounts and the audit trail all live in the SQLite files under the base directory.',
  postgres:
    'The trees are PostgreSQL schemas, while definitions, credentials, accounts and the audit trail live in the instance marked control — one product playing two roles.',
  treedb:
    'The trees live in the treedb servers; the control plane — definitions, credentials, accounts and this audit trail — is local SQLite on the meta-server you are talking to. Two engine names on this page are two answers to two different questions, not one deployment reported twice.',
  unknown:
    'The server named a backend this console does not know, so nothing here is inferred from that name.',
}
const engineNote = computed(() => t(ENGINE_NOTE[engine.value]))
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
    <BarRow :title="'At a glance'">
      <span class="muted">{{ t('every figure is a count this page can click through') }}</span>
    </BarRow>
    <div class="cards">
      <StatCard
        :label="engineCard.label"
        :value="engineCard.value"
        :sub="engineCard.sub"
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
      <!-- The order the six nav items are meant to be used in. It lives here
           because the overview is the page both a first-time and a returning
           operator reads, and nowhere else does the console state that a base
           view comes before a delegation, or a delegation before a key. -->
      <BarRow :title="'Typical path'" />
      <ol class="flow">
        <li>
          <button type="button" class="link" @click="to('views')">
            {{ t('Create the base view') }}
          </button>
          <span class="muted">
            {{ t('a name and a storage home — every lens, alias and fork is shaped out of one') }}
          </span>
        </li>
        <li>
          <button type="button" class="link" @click="to('accounts')">
            {{ t('Delegate it to an account') }}
          </button>
          <span class="muted">
            {{ t('the account governs that base and everything derived from it') }}
          </span>
        </li>
        <li>
          <button type="button" class="link" @click="to('keys')">
            {{ t('Issue the mount key') }}
          </button>
          <span class="muted">{{ t('the credential a client mounts the view with') }}</span>
        </li>
        <li>
          <button type="button" class="link" @click="to('audit')">
            {{ t('Read what happened') }}
          </button>
          <span class="muted">
            {{ t('every call above is written to the trail as it is made') }}
          </span>
        </li>
      </ol>

      <BarRow :title="'This deployment'">
        <span class="muted">{{ t('read live from the meta-server that answered this page') }}</span>
      </BarRow>
      <div class="detail">
        <p class="engine-note">{{ engineNote }}</p>
        <!-- Two groups, two labels: where a tree and a credential physically
             live, and what the deployment keeps over time. Four rows of the
             first answer the question "which machine do I log into"; the second
             answers "how much history is there". Mixing them in one grid is what
             made this panel read as a wall of pairs. -->
        <h3 class="det-h">{{ t('Where the data lives') }}</h3>
        <div class="grid2">
          <!-- Two names, two rows: the engine the trees live in (with the
               server's own layout line, which is about exactly that) and the
               control plane that answers governance calls. They are the same
               store on SQLite and PostgreSQL, and different ones on treedb —
               which is why they are never merged into one "backend" value. -->
          <StatLine label="tree engine">
            <b>{{ engineText }}</b>
            <span v-if="store.backendLayout" class="muted">· {{ store.backendLayout }}</span>
          </StatLine>
          <StatLine label="control plane">
            <b>{{ info.backend || '—' }}</b>
            <span class="muted">· {{ store.describe || '' }}</span>
          </StatLine>
          <StatLine :label="baseLabel">
            <code>{{ store.base || '—' }}</code>
          </StatLine>
          <StatLine label="server role">
            <b>{{ store.role || '—' }}</b>
            <span class="muted">{{ t('the process this console is talking to') }}</span>
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
        <!-- The cards above already carry every counter this page knows. What
             stays here is the substrate an operator quotes in a ticket — how
             long the trail is kept — so the panel does not repeat the row of
             figures it sits under. -->
        <h3 class="det-h">{{ t('How far the trail goes') }}</h3>
        <div class="grid2">
          <StatLine label="audit trail">
            <b v-if="store.auditRetention">
              {{ t('rows older than ${w} are swept', { w: store.auditRetention }) }}
            </b>
            <b v-else>{{ t('every row is kept until an operator purges it') }}</b>
          </StatLine>
        </div>
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

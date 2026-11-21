<script setup lang="ts">
/*
  The credential registry: mint on top, the list below, and every verb in the
  row itself.

  Filtering is CLIENT-side over the whole registry, and the state chips carry
  their own counts — "how many are expired" is a question an operator asks before
  clicking anything, and answering it only after a click would cost a page load
  for a number. The search box covers the fields a person actually greps for
  (label, id, note, owner, who minted it, view) plus the fingerprint, so a paste
  of a `a1b2…c3d4` hint from a log line finds the row.

  The help panel is part of this page, not a footnote: a page of verbs the user
  has to guess at is a page that gets used wrong, and the one thing the security
  model refuses (why the middle of the key is gone) is spelled out where it is
  noticed.
*/
import { computed, ref } from 'vue'
import type { KeyRow } from '@/types'
import { useOps } from '@/composables/useOps'
import { usePageLoad } from '@/composables/usePageLoad'
import { useCatalogStore } from '@/stores/catalog'
import { useFilterStore } from '@/stores/filters'
import { useUiStore } from '@/stores/ui'
import type { ChipItem } from '@/components/ui/chips'
import { KEY_BUCKETS, keyBucket, type KeyBucket } from '@/utils/keys'
import { counted } from '@/utils/text'
import { t } from '@/i18n'
import PageHead from '@/components/layout/PageHead.vue'
import HelpPanel from '@/components/layout/HelpPanel.vue'
import FldRow from '@/components/layout/FldRow.vue'
import FilterBar from '@/components/ui/FilterBar.vue'
import ChipGroup from '@/components/ui/ChipGroup.vue'
import ActionButton from '@/components/ui/ActionButton.vue'
import LoadingHint from '@/components/ui/LoadingHint.vue'
import MintPanel from '@/components/keys/MintPanel.vue'
import KeyTable from '@/components/keys/KeyTable.vue'

const catalog = useCatalogStore()
const filters = useFilterStore()
const ui = useUiStore()
const { api } = useOps()

const rows = ref<KeyRow[]>([])

async function load(): Promise<void> {
  await catalog.loadViews()
  rows.value = (await api('apikey.list', {})) ?? []
}

const { pending, error } = usePageLoad(load)

function refresh(): void {
  ui.reload()
}

/** The chip row, with a live count per bucket. */
const stateChips = computed<ChipItem[]>(() =>
  KEY_BUCKETS.map((b: KeyBucket) => ({
    v: b,
    t: b === 'all' ? 'all' : b === 'expiring' ? 'expiring soon' : b,
    n: b === 'all' ? rows.value.length : rows.value.filter((r) => keyBucket(r) === b).length,
  })),
)

const hit = computed(() => {
  const f = filters.keyFilter
  const q = f.q.trim().toLowerCase()
  return rows.value.filter((r) => {
    if (
      q &&
      ![r.label, r.id, r.owner, r.created_by, r.view, r.note, r.secret_hint].some(
        (x) => x && String(x).toLowerCase().includes(q),
      )
    )
      return false
    if (f.state !== 'all' && keyBucket(r) !== f.state) return false
    if (f.scope !== 'all' && r.scope !== f.scope) return false
    if (f.view !== 'all' && (r.view || '') !== f.view) return false
    return true
  })
})

/** The four planes, offered whether or not this account may mint for them: the
    registry is a list to read, and an `admin` row is still an `admin` row. */
const PLANE_VALUES = ['mount', 'mcp', 'admin', 'any']

const HELP: [string, string][] = [
  [
    'Create key',
    'mints a 16-character id and a 32-hex-digit secret; the client uses `<id>:<secret>`. The dialog that follows is the first display of it — whether the registry also keeps a copy an operator can read back is said there, and depends on the store.',
  ],
  [
    'the key column',
    'shows the stored fingerprint: the first and last four hex digits of the secret. The digest GroveFS authenticates against is one-way and gives no hint back, so no list or dump assembles the middle from this — the full string comes back only through Reveal.',
  ],
  [
    'Reveal',
    'reads the stored copy of a secret back and shows it, unwrapped under the store’s own key, to the key’s owner or a super administrator. The call is written to the audit trail naming the id it opened. Only offered where a copy exists: a row minted before the store had wrapping material has none, and Rotate is how a usable secret is obtained then.',
  ],
  [
    'Rotate',
    'replaces the secret for the SAME id: label, scope, view binding, note, deadline and audit history stay. The old secret stops working at its next handshake; a live mount on it stays open until it reconnects.',
  ],
  [
    'Revoke',
    'retires the credential for good. Nothing can un-revoke it and a revoked row is not editable — delete it if you also want it gone from the registry. It takes effect on the key’s NEXT call, so a live mount keeps its open trees until then.',
  ],
  [
    'Delete',
    'removes the credential row entirely; the id and secret stop authenticating at the next handshake. Only the key’s owner or a super administrator may delete it, and the console asks first. Audit rows the key wrote stay in the log with their original attribution — deleting a key never shortens the trail.',
  ],
  [
    'Make ro / Make rw',
    'flips the write permission without touching the secret, so a mount can be made read-only without handing anyone a new credential.',
  ],
  [
    'Edit',
    'changes the bookkeeping only: the note and the deadline. Scope, view binding and id never change here, because a key an application is already holding would otherwise acquire a reach its owner did not ask for.',
  ],
  [
    'Expires',
    'the same spelling as `grovefs apikey create --ttl`: a count and `s`, `m`, `h` or `d`, or `never`. Past its deadline a key is refused at the next handshake, and a live mount is cut within about a second.',
  ],
  [
    'plane',
    '`mount` authenticates a client mount, `mcp` an agent, `admin` the governance console, `any` everything. An `admin` key can change accounts and keys — treat it as a password.',
  ],
  [
    'view binding',
    'a bound key can only open that view (and what derives from it); an unbound one reaches every tree. Only an administrator may mint an unbound key.',
  ],
  [
    'minted by',
    'the account that created the key. It is shown beside the owner, who is the account the key belongs to — the two differ whenever an administrator mints for a colleague.',
  ],
]
</script>

<template>
  <!-- The id is not decoration: the stylesheet's one page-specific rule,
       `#page-keys .panel`, gives the mint form its tighter five-column grid —
       the same hook the hand-written page's `<section id="page-keys">` gave it. -->
  <div id="page-keys">
    <PageHead
      kicker="Credentials"
      title="API keys"
      desc="Mint, rotate, revoke and delete the keys clients mount and agents call with."
    >
      <template #actions>
        <ActionButton :action="refresh" :label="'refresh'" variant="link" />
      </template>
    </PageHead>

    <MintPanel />

    <HelpPanel :summary="'What each action does — and what GroveFS keeps'" :pairs="HELP" />

    <LoadingHint v-if="pending && !rows.length" />
    <p v-else-if="error" class="err">{{ error }}</p>

    <template v-else>
      <FilterBar :count="counted(hit.length, rows.length)">
        <template #search>
          <input
            v-model="filters.keyFilter.q"
            type="search"
            :placeholder="t('filter by label, id, note, owner or view…')"
            :aria-label="t('filter by label, id, note, owner or view…')"
          />
        </template>

        <template #opts>
          <FldRow :label="'state'">
            <ChipGroup v-model="filters.keyFilter.state" :items="stateChips" />
          </FldRow>
          <FldRow :label="'plane'" for="key-scope">
            <select id="key-scope" v-model="filters.keyFilter.scope">
              <option value="all">{{ t('every plane') }}</option>
              <option v-for="v in PLANE_VALUES" :key="v" :value="v">{{ v }}</option>
            </select>
          </FldRow>
          <FldRow :label="'view'" for="key-view">
            <select id="key-view" v-model="filters.keyFilter.view">
              <option value="all">{{ t('every view') }}</option>
              <option value="">{{ t('unbound keys') }}</option>
              <option v-for="v in catalog.views" :key="v.name" :value="v.name">{{ v.name }}</option>
            </select>
          </FldRow>
        </template>
      </FilterBar>

      <KeyTable
        :rows="hit"
        openable
        :empty="rows.length ? 'no keys match the filter' : 'no keys you can see'"
      />
    </template>
  </div>
</template>

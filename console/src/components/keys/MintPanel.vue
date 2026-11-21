<script setup lang="ts">
/*
  Mint a credential.

  Five questions, and every one of them is a decision the operator owns: what to
  call it, what it is for, which plane it authenticates, which view it may open
  and how long it lives. What this form deliberately does NOT ask is the secret —
  GroveFS generates it, and only the store knows whether it also kept a
  recoverable copy of what it hands back. That is why the dialog that follows
  says which of the two happened instead of assuming either.

  A delegated account sees a shorter form: the two planes it may use, no "no view
  binding" choice (an unbound key reaches every tree, so it is an administrator's
  to mint) and the sentence saying so under the panel.

  Only `label` and `scope` are always sent. An empty note, view or ttl is ABSENT
  from the request rather than empty in it, because the server treats "not given"
  and "given as nothing" as two different answers.
*/
import { computed, ref } from 'vue'
import type { MintKeyArgs } from '@/api/ops'
import { useOps } from '@/composables/useOps'
import { useCatalogStore } from '@/stores/catalog'
import { useSessionStore } from '@/stores/session'
import { useUiStore } from '@/stores/ui'
import { NOTE_MAX } from '@/utils/text'
import { t } from '@/i18n'
import PanelBox from '@/components/layout/PanelBox.vue'
import FieldRow from '@/components/layout/FieldRow.vue'
import ActionButton from '@/components/ui/ActionButton.vue'
import TtlSelect from './TtlSelect.vue'

const catalog = useCatalogStore()
const session = useSessionStore()
const ui = useUiStore()
const { api } = useOps()

const label = ref('')
const note = ref('')
const scope = ref('mount')
const view = ref('')
const ttl = ref('')
const ro = ref(false)

/** The planes this account may mint for: an `admin` or `any` credential is a
    key over the governance console itself. */
const scopes = computed(() =>
  session.isSuper
    ? [
        { v: 'mount', t: 'mount — a client mount' },
        { v: 'mcp', t: 'mcp — an agent' },
        { v: 'admin', t: 'admin — the governance plane' },
        { v: 'any', t: 'any — every plane' },
      ]
    : [
        { v: 'mount', t: 'mount — a client mount' },
        { v: 'mcp', t: 'mcp — an agent' },
      ],
)

async function create(): Promise<void> {
  const args: MintKeyArgs = { label: label.value.trim(), scope: scope.value, ro: ro.value }
  if (view.value) args.view = view.value
  if (note.value.trim()) args.note = note.value.trim()
  if (ttl.value) args.ttl = ttl.value
  const r = await api('apikey.create', args)
  // The two free-text fields are what gets retyped; the choices are what a
  // second key usually keeps, so they stay as set.
  label.value = ''
  note.value = ''
  // The reply carries the plaintext `<id>:<secret>` and says whether the
  // registry can hand it back later; `view` is what THIS request asked for,
  // because the create reply reports the key, not the row.
  ui.showSecret({
    title: 'New api key',
    detailed: true,
    copy: r.stored ? 'stored' : 'once',
    secret: r,
    view: args.view ?? null,
  })
  ui.reload()
}
</script>

<template>
  <PanelBox :title="'Mint a key'">
    <FieldRow label="label" for="mint-label">
      <input id="mint-label" v-model="label" placeholder="etl job" />
    </FieldRow>

    <FieldRow label="note" for="mint-note">
      <input
        id="mint-note"
        v-model="note"
        type="text"
        :placeholder="t('what this key is for…')"
        :maxlength="NOTE_MAX"
      />
      <template #hint>
        {{ t('shown in the list and searchable; it never affects what the key can do') }}
      </template>
    </FieldRow>

    <FieldRow label="plane" for="mint-scope">
      <select id="mint-scope" v-model="scope">
        <option v-for="o in scopes" :key="o.v" :value="o.v">{{ t(o.t) }}</option>
      </select>
      <template v-if="!session.isSuper" #hint>
        {{ t('a delegated account mints client keys only') }}
      </template>
    </FieldRow>

    <FieldRow label="view" for="mint-view">
      <select id="mint-view" v-model="view">
        <option value="">
          {{ session.isSuper ? t('(no view binding — every tree)') : t('(choose a view)') }}
        </option>
        <option v-for="v in catalog.views" :key="v.name" :value="v.name">
          {{ `${v.name} [${v.kind}]` }}
        </option>
      </select>
    </FieldRow>

    <FieldRow label="valid for" for="mint-ttl">
      <TtlSelect v-model="ttl" />
      <template #hint>
        {{ t('a key past its deadline is refused at the next handshake') }}
      </template>
    </FieldRow>

    <span v-if="!session.isSuper" class="muted">
      {{ t('an unbound key reaches every tree, so a delegated account must name one') }}
    </span>

    <div class="panel-foot">
      <label class="check" for="mint-ro">
        <input id="mint-ro" v-model="ro" type="checkbox" />
        {{ t('read-only') }}
      </label>
      <ActionButton :action="create" :label="'Create key'" variant="primary" />
    </div>
  </PanelBox>
</template>

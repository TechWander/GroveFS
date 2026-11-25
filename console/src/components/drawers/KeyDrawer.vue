<script setup lang="ts">
/*
  One credential's whole record in the side pane.

  The table shows the facts you compare rows on; everything else about a key —
  its full timestamps, who minted it, whether it has been used at all, what it
  was last used FOR — lives here, with the verbs that act on it in the footer,
  where the record they are about to change is on screen while you read it.

  The last-calls block is a SECOND read, not a slice of the audit page: it asks
  for 300 rows and keeps this key's first 8, so the pane answers "has it been
  used since I rotated it" whatever the list behind it is filtered to. A
  refusal there degrades to one `.err` line inside the pane — the credential's
  own record is still worth reading.
*/
import { computed, onMounted, ref } from 'vue'
import type { AuditRow, KeyRow } from '@/types'
import { useOps } from '@/composables/useOps'
import { useNav } from '@/composables/useNav'
import { keyLabel, keyTone, keyWhy } from '@/utils/keys'
import { humanLeft } from '@/utils/time'
import { t } from '@/i18n'
import BaseDrawer from '@/components/overlay/BaseDrawer.vue'
import DrawerSection from '@/components/overlay/DrawerSection.vue'
import KvList from '@/components/layout/KvList.vue'
import KvRow from '@/components/layout/KvRow.vue'
import ActionButton from '@/components/ui/ActionButton.vue'
import MonoText from '@/components/ui/MonoText.vue'
import MaskText from '@/components/ui/MaskText.vue'
import StateBadge from '@/components/ui/StateBadge.vue'
import CopyButton from '@/components/ui/CopyButton.vue'
import TimeCell from '@/components/ui/TimeCell.vue'
import LoadingHint from '@/components/ui/LoadingHint.vue'
import AuditTable from '@/components/audit/AuditTable.vue'
import KeyRowActions from '@/components/keys/KeyRowActions.vue'

const props = defineProps<{ row: KeyRow }>()
const emit = defineEmits<{ close: [] }>()

const { soft } = useOps()
const { openView, openAccount, openAuditOfKey } = useNav()

const recent = ref<{ error: string } | null>(null)
const mine = ref<AuditRow[]>([])
const pending = ref(true)

/**
  The sentence under the credential block. It is composed here rather than in the
  template because that sentence quotes the client's own spelling, `<id>:<secret>`:
  Prettier's Vue parser reads a `<id>` inside an interpolation as a tag and
  refuses to format the file at all, and Vue itself is happy either way. A
  computed, not a constant, so a language switch while the pane is open re-says
  it. The dictionary key is the same string the hand-written page used.

  The second half depends on the ROW: whether this store ever wrote a recoverable
  copy of this secret is what separates "Reveal hands it back" from "Rotate is
  the only way to get a usable one", and a pane that says the first about a row
  from before the copies would be sending the operator away with a promise it
  cannot keep.
 */
const credentialNote = computed(() =>
  [
    t(
      'A client presents `<id>:<secret>` (`mount --api-key`, or the `GROVEFS_API_KEY` environment variable).',
    ),
    props.row.secret_available
      ? t(
          'A handshake reads only the stored digest; the secret itself lives wrapped under the store key, so Reveal can hand it back — and that read is audited with the id it opened.',
        )
      : t(
          'No recoverable copy of this secret is stored, so nothing here can read it back: Rotate is the way to obtain a usable one.',
        ),
  ].join(' '),
)

onMounted(async () => {
  const res = await soft('apikey.calls', { limit: 300 })
  pending.value = false
  if (res.ok) mine.value = (res.r ?? []).filter((c) => c.key_id === props.row.id).slice(0, 8)
  else recent.value = { error: res.error }
})
</script>

<template>
  <BaseDrawer :title="props.row.label" @close="emit('close')">
    <template #badges>
      <StateBadge
        :label="keyLabel(props.row)"
        :tone="keyTone(props.row)"
        :title="keyWhy(props.row)"
      />
      <StateBadge
        :label="props.row.ro ? t('read-only') : t('read-write')"
        :tone="props.row.ro ? 'warn' : undefined"
      />
    </template>

    <DrawerSection :heading="'Credential'">
      <KvList>
        <KvRow label="id">
          <span class="cell-badges">
            <MonoText :text="props.row.id" />
            <CopyButton :text="props.row.id" />
          </span>
        </KvRow>
        <KvRow label="key"><MaskText :hint="props.row.secret_hint" /></KvRow>
        <KvRow label="plane"><MonoText :text="props.row.scope" /></KvRow>
        <KvRow label="view">
          <button
            v-if="props.row.view"
            type="button"
            class="mini mono"
            :title="t('open this view')"
            @click="openView(String(props.row.view))"
          >
            {{ props.row.view }}
          </button>
          <span v-else class="muted">{{ t('unbound — every tree') }}</span>
        </KvRow>
      </KvList>
      <p class="muted">{{ credentialNote }}</p>
    </DrawerSection>

    <DrawerSection :heading="'Bookkeeping'">
      <KvList>
        <KvRow label="note">
          <span v-if="props.row.note">{{ props.row.note }}</span>
          <span v-else class="muted">{{ t('no note') }}</span>
        </KvRow>
        <KvRow label="owner">
          <button
            v-if="props.row.owner"
            type="button"
            class="mini mono"
            :title="t('open this account')"
            @click="openAccount(String(props.row.owner))"
          >
            {{ props.row.owner }}
          </button>
          <span v-else class="muted">{{ t('unassigned') }}</span>
        </KvRow>
        <KvRow label="minted by">
          <button
            v-if="props.row.created_by"
            type="button"
            class="mini mono"
            :title="t('open this account')"
            @click="openAccount(String(props.row.created_by))"
          >
            {{ props.row.created_by }}
          </button>
          <span v-else class="muted">
            {{ t('unknown — this row predates the bookkeeping columns') }}
          </span>
        </KvRow>
        <KvRow label="created"><TimeCell :ts="props.row.created_at" /></KvRow>
        <KvRow label="last used">
          <TimeCell v-if="props.row.last_used_at" :ts="props.row.last_used_at" />
          <span v-else class="muted">{{ t('never used') }}</span>
        </KvRow>
        <KvRow label="valid until">
          <template v-if="props.row.expires_at">
            <MonoText :text="props.row.expires_at" />
            <span class="muted">
              {{
                props.row.expired
                  ? t('expired')
                  : t('in ${left}', { left: humanLeft(props.row.expires_at) })
              }}
            </span>
          </template>
          <template v-else>{{ t('never expires') }}</template>
        </KvRow>
        <KvRow label="revoked">
          <TimeCell v-if="props.row.revoked_at" :ts="props.row.revoked_at" />
          <span v-else class="muted">{{ t('not revoked') }}</span>
        </KvRow>
      </KvList>
    </DrawerSection>

    <DrawerSection :heading="'Last calls with it'">
      <LoadingHint v-if="pending" />
      <p v-else-if="recent" class="err">{{ recent.error }}</p>
      <AuditTable v-else-if="mine.length" :rows="mine" :empty="'no calls recorded for it yet'" />
      <p v-else class="muted">{{ t('no calls recorded for it yet') }}</p>
      <!-- Eight rows is a sample; the credential's WHOLE trail — every window,
           the op and verdict filters included — is the audit page with this key
           already picked. -->
      <ActionButton
        :action="() => void openAuditOfKey(props.row.id)"
        :label="'open the audit page'"
        variant="link"
      />
    </DrawerSection>

    <template #left>
      {{ t('a fingerprint identifies a key; it does not authorize anything') }}
    </template>
    <template #footer>
      <KeyRowActions :row="props.row" place="drawer" @deleted="emit('close')" />
    </template>
  </BaseDrawer>
</template>

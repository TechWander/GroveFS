<script setup lang="ts">
/*
  One audit row, in full.

  The table gives you six clipped columns; this gives you the message the server
  actually wrote, un-wrapped. Line breaks and indentation in a governance refusal
  separate "why" from "what to do next", which is why it is a `<pre>` and not a
  paragraph the browser is allowed to flatten.
*/
import type { AuditRow } from '@/types'
import { computed } from 'vue'
import { useUiStore } from '@/stores/ui'
import { useFilterStore } from '@/stores/filters'
import { useNav } from '@/composables/useNav'
import { auditAccount, auditActor, mintedKey } from '@/utils/audit'
import { t } from '@/i18n'
import BaseDrawer from '@/components/overlay/BaseDrawer.vue'
import DrawerSection from '@/components/overlay/DrawerSection.vue'
import KvList from '@/components/layout/KvList.vue'
import KvRow from '@/components/layout/KvRow.vue'
import MonoText from '@/components/ui/MonoText.vue'
import TwoLineCell from '@/components/ui/TwoLineCell.vue'
import StateBadge from '@/components/ui/StateBadge.vue'
import ActionButton from '@/components/ui/ActionButton.vue'

const props = defineProps<{ row: AuditRow }>()

const ui = useUiStore()
const filters = useFilterStore()
const { openKeysOf, openAccount } = useNav()
const who = (): string | null => auditActor(props.row)
/** The row's account, when an ACCOUNT signed the call — the only case where
    "open this account" is a jump that lands somewhere. */
const account = computed(() => auditAccount(props.row))

/** The credential this row MINTED, when it minted one. The store keeps no
    parent→child column, so the audit message is the only place the link lives —
    and a row older than that link says so instead of pretending the key minted
    nothing. */
const child = computed(() => mintedKey(props.row))

function showChild(): void {
  filters.auditFilter.key = child.value ?? 'all'
  ui.closeDrawer()
}
</script>

<template>
  <BaseDrawer
    :title="props.row.tool || props.row.method || t('governance call')"
    @close="ui.closeDrawer()"
  >
    <template #badges>
      <StateBadge
        :label="props.row.ok ? t('ok') : t('failed')"
        :tone="props.row.ok ? 'good' : 'bad'"
      />
    </template>

    <DrawerSection>
      <KvList>
        <KvRow label="when" mono :value="props.row.ts || '—'" />
        <KvRow label="who">
          <button
            v-if="account"
            type="button"
            class="mini mono"
            :title="t('open this account')"
            @click="openAccount(account)"
          >
            {{ account }}
          </button>
          <span v-else-if="who()">{{ who() }}</span>
          <span v-else class="muted">{{ t('no account on it') }}</span>
        </KvRow>
        <KvRow label="plane" mono :value="props.row.method" />
        <KvRow label="op" mono :value="props.row.tool" />
        <KvRow label="key">
          <TwoLineCell
            v-if="props.row.key_label"
            :top="props.row.key_label"
            :sub="props.row.key_id"
            sub-mono
          />
          <MonoText v-else-if="props.row.key_id" :text="props.row.key_id" />
          <template v-else>—</template>
          <!-- The registry row behind this id: the keys page searched to it. -->
          <ActionButton
            v-if="props.row.key_id"
            :action="() => void openKeysOf(String(props.row.key_id))"
            :label="'open in keys page'"
            variant="link"
          />
        </KvRow>
        <KvRow v-if="props.row.tool === 'apikey.create'" label="minted">
          <template v-if="child">
            <MonoText :text="child" />
            <ActionButton :action="showChild" label="show this key’s calls" variant="link" />
          </template>
          <span v-else class="muted">{{ t('no minted key recorded on this row') }}</span>
        </KvRow>
      </KvList>
    </DrawerSection>

    <DrawerSection heading="What the server said">
      <pre class="plain">{{ props.row.detail || t('nothing recorded') }}</pre>
    </DrawerSection>
  </BaseDrawer>
</template>

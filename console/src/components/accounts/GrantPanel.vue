<script setup lang="ts">
/*
  Delegate a base view — the sentence that gives a `user` something to govern.

  A delegation is over a WHOLE subtree: granting `project-x` governs every lens,
  fork and alias derived from it, now and later. That is what the heading next to
  this panel says, and it is the reason the picker offers bases only (a
  delegation over a derived name would be a second, drifting answer to the same
  question).

  When the name typed resolves somewhere else, the reply says so: `asNamed`
  false means the server recorded the base it actually resolved to, which is the
  fact an operator needs before telling a colleague what they can now open.
*/
import { computed, ref } from 'vue'
import type { AccountRow } from '@/types'
import { useOps } from '@/composables/useOps'
import { useCatalogStore } from '@/stores/catalog'
import { useUiStore } from '@/stores/ui'
import { t } from '@/i18n'
import PanelBox from '@/components/layout/PanelBox.vue'
import FieldRow from '@/components/layout/FieldRow.vue'
import ActionButton from '@/components/ui/ActionButton.vue'

const props = defineProps<{ accounts: AccountRow[] }>()

const catalog = useCatalogStore()
const ui = useUiStore()
const { api } = useOps()

const account = ref('')
const base = ref('')

const names = computed(() => props.accounts.map((a) => a.account))

/** Bases first — the picker's whole point is that a delegation is over a
    subtree — and if no base exists yet, every name, so the verb is still
    reachable on a deployment that is one `view.create` away from working. */
const baseNames = computed(() =>
  catalog.baseNames.length ? catalog.baseNames : catalog.views.map((v) => v.name),
)

async function grant(): Promise<void> {
  const r = await api('grant.add', { account: account.value, base: base.value })
  ui.toast(
    r.asNamed
      ? t('${account} governs ${base}', { account: r.account, base: r.base })
      : t('${account} governs ${base} — that is what the name you chose resolves to', {
          account: r.account,
          base: r.base,
        }),
  )
  ui.reload()
}
</script>

<template>
  <PanelBox :title="'Delegate a base view'">
    <FieldRow label="account" for="grant-account">
      <select id="grant-account" v-model="account">
        <option v-for="a in names" :key="a" :value="a">{{ a }}</option>
      </select>
    </FieldRow>

    <FieldRow label="base view" for="grant-base">
      <select id="grant-base" v-model="base">
        <option v-for="b in baseNames" :key="b" :value="b">{{ b }}</option>
      </select>
    </FieldRow>

    <ActionButton :action="grant" :label="'Grant'" variant="primary" />
  </PanelBox>
</template>

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

  A refusal is shown INLINE, in two lines: what to do next, then the server's own
  sentence under it. The panel already knows the namespace, so a name that is not
  in it is caught before the call — the case that used to surface as
  "view 'x' has neither a definition nor a tree", a governance phrase nobody
  thinks to mean "that view was deleted".
*/
import { computed, ref } from 'vue'
import type { AccountRow } from '@/types'
import { useOps } from '@/composables/useOps'
import { useCatalogStore } from '@/stores/catalog'
import { useUiStore } from '@/stores/ui'
import { useNav } from '@/composables/useNav'
import { refusalOf, type Refusal } from '@/utils/error'
import { t } from '@/i18n'
import PanelBox from '@/components/layout/PanelBox.vue'
import FieldRow from '@/components/layout/FieldRow.vue'
import ActionButton from '@/components/ui/ActionButton.vue'

const props = defineProps<{ accounts: AccountRow[] }>()

const catalog = useCatalogStore()
const ui = useUiStore()
const { api } = useOps()
const { to } = useNav()

const account = ref('')
const base = ref('')
/** The last refusal, cleared by the next attempt. */
const refusal = ref<Refusal | null>(null)

const names = computed(() => props.accounts.map((a) => a.account))

/** Bases first — the picker's whole point is that a delegation is over a
    subtree — and if no base exists yet, every name, so the verb is still
    reachable on a deployment that is one `view.create` away from working. */
const baseNames = computed(() =>
  catalog.baseNames.length ? catalog.baseNames : catalog.views.map((v) => v.name),
)

async function grant(): Promise<void> {
  refusal.value = null
  if (!account.value || !base.value) {
    refusal.value = { headline: t('choose both the account and the base view'), detail: '' }
    return
  }
  // The picker offers names from the namespace, but the namespace is a cache:
  // a view deleted in another tab is still in this dropdown. Asking the server
  // costs a round trip and returns a sentence about definitions and trees;
  // answering it here returns the one action that fixes it.
  if (!catalog.views.some((v) => v.name === base.value)) {
    refusal.value = {
      headline: t(
        '${n} is not in the namespace this page loaded — it was deleted, or it was never created. Create the view first, then delegate it.',
        { n: base.value },
      ),
      detail: '',
      page: 'views',
    }
    return
  }
  try {
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
  } catch (e) {
    // Not rethrown: a toast over a dialog is gone in three seconds, and the
    // operator still has to read the form to fix it.
    refusal.value = refusalOf(e)
  }
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
      <select id="grant-base" v-model="base" @change="refusal = null">
        <option v-for="b in baseNames" :key="b" :value="b">{{ b }}</option>
      </select>
    </FieldRow>

    <div v-if="refusal" class="refusal" role="alert">
      <p class="refusal-head">{{ refusal.headline }}</p>
      <p v-if="refusal.detail" class="refusal-detail mono">{{ refusal.detail }}</p>
      <button v-if="refusal.page" type="button" class="link" @click="to(refusal.page)">
        {{ t('open that page') }}
      </button>
    </div>

    <div class="panel-foot">
      <ActionButton :action="grant" :label="'Grant'" variant="primary" />
    </div>
  </PanelBox>
</template>

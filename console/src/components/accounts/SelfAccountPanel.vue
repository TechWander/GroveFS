<script setup lang="ts">
/*
  The account page for someone who may not read the account list.

  `account.list` is an administrator's op, so a delegated account is refused —
  and this block is why the page does not simply go blank on that refusal. The
  one thing a `user` may always do here is change its own password (the server's
  `SelfAccount` gate), and its own delegations are its own business, so both are
  shown from the session the gateway already handed out, with the server's
  refusal sentence quoted underneath rather than hidden.
*/
import { computed, ref } from 'vue'
import { useCatalogStore } from '@/stores/catalog'
import { useSessionStore } from '@/stores/session'
import { t } from '@/i18n'
import BarRow from '@/components/layout/BarRow.vue'
import StatLine from '@/components/layout/StatLine.vue'
import WhoCell from '@/components/ui/WhoCell.vue'
import MonoText from '@/components/ui/MonoText.vue'
import StateBadge from '@/components/ui/StateBadge.vue'
import ActionButton from '@/components/ui/ActionButton.vue'
import BasePills from './BasePills.vue'
import PasswdDialog from '@/components/dialogs/PasswdDialog.vue'

const props = defineProps<{ error: string }>()

const session = useSessionStore()
const catalog = useCatalogStore()

const passwdOpen = ref(false)

/** What this account governs, in the two numbers that matter: the bases it was
    delegated, and every name that therefore resolves through them. */
const derived = computed(
  () =>
    catalog.views.filter((v) => v.kind !== 'base' && !!v.base && session.bases.includes(v.base))
      .length,
)

function openPasswd(): void {
  passwdOpen.value = true
}
</script>

<template>
  <BarRow :title="'Your account'" />

  <div class="detail">
    <div class="head">
      <WhoCell :name="session.account" :sub="t('signed in here')" />
      <StateBadge :label="session.role" :tone="session.role === 'admin' ? 'admin' : undefined" />
    </div>

    <p class="muted">
      {{ t('The account list is an administrator’s view: ${e}', { e: props.error }) }}
    </p>

    <div class="grid2">
      <div>
        <h3 class="det-h">{{ t('Your own record') }}</h3>
        <StatLine label="account"><MonoText :text="session.account" /></StatLine>
        <StatLine label="role">
          <b>{{ session.role }}</b>
        </StatLine>
        <StatLine label="password">
          <ActionButton :action="openPasswd" :label="'Change my password'" />
        </StatLine>
      </div>

      <div>
        <h3 class="det-h">{{ t('What you govern') }}</h3>
        <StatLine label="base views">
          <b>{{ session.bases.length }}</b>
          <span v-if="session.bases.length" class="muted">
            {{ t('· ${n} derived name(s)', { n: derived }) }}
          </span>
        </StatLine>
        <p v-if="!session.bases.length" class="muted">
          {{ t('You govern no base view yet — ask an administrator for a delegation.') }}
        </p>
        <div v-else class="muted">
          <BasePills :bases="session.bases" />
        </div>
      </div>
    </div>
  </div>

  <PasswdDialog v-if="passwdOpen" :account="session.account" @close="passwdOpen = false" />
</template>

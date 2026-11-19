<script setup lang="ts">
/*
  The shell, and the one state machine the whole page hangs off.

  `booting` → the brand and a line saying the session is being read (the
  hand-written page showed literally nothing for that round trip);
  `anonymous` → the sign-in card;
  `ready` → header, router view, and the two overlays the console owns.

  The drawers and the prompt live HERE rather than in a page because they are
  not a page's property: an audit row opened from the overview and one opened
  from the audit page are the same record shown the same way, a confirmation that
  survives leaving the page that raised it is a confirmation that can be answered,
  and a one-time secret that survived the drawer behind it closing is a
  credential that still exists tomorrow.

  `document.documentElement.lang` and `document.title` follow the UI language, as
  they did in `applyStatic()` — a Chinese page announced as English is read with
  the wrong pronunciation by every screen reader.

  The signed-in branch needs no wrapper of its own: the hand-written page put
  `header` and `main` inside `<div id="app">`, a name no rule in the stylesheet
  ever keyed on and one `index.html` already uses for the mount point itself.
*/
import { onMounted, watch } from 'vue'
import { useI18n, t } from '@/i18n'
import { useSessionStore } from '@/stores/session'
import { useCatalogStore } from '@/stores/catalog'
import { useFilterStore } from '@/stores/filters'
import AppHeader from '@/components/chrome/AppHeader.vue'
import LoginCard from '@/components/chrome/LoginCard.vue'
import BrandMark from '@/components/chrome/BrandMark.vue'
import AskModal from '@/components/overlay/AskModal.vue'
import ToastHost from '@/components/overlay/ToastHost.vue'
import AppDrawer from '@/components/drawers/AppDrawer.vue'
import SecretDialog from '@/components/dialogs/SecretDialog.vue'
import LoadingHint from '@/components/ui/LoadingHint.vue'

const { lang } = useI18n()
const session = useSessionStore()
const catalog = useCatalogStore()
const filters = useFilterStore()

function applyDocumentLang(): void {
  document.documentElement.lang = lang.value === 'zh' ? 'zh-CN' : 'en'
  document.title = lang.value === 'zh' ? 'GroveFS 控制台' : 'GroveFS console'
}

watch(lang, applyDocumentLang, { immediate: true })

watch(
  () => session.status,
  (next, prev) => {
    if (next !== 'ready') return
    // A different person, a different namespace: a cached view list or a search
    // box carried over from the last session hides rows nobody looked for.
    catalog.reset()
    if (prev === 'anonymous') filters.reset()
  },
)

onMounted(() => void session.boot())
</script>

<template>
  <ToastHost />

  <section v-if="session.status === 'booting'" id="login">
    <div class="login-card">
      <div class="brand-row">
        <BrandMark variant="login" />
        <h1>GroveFS</h1>
      </div>
      <p class="sub">{{ t('Governance console') }}</p>
      <p class="muted"><LoadingHint /></p>
    </div>
  </section>

  <LoginCard v-else-if="session.status === 'anonymous'" />

  <template v-else>
    <AppHeader />
    <main>
      <RouterView />
    </main>
  </template>

  <AppDrawer />
  <SecretDialog />
  <AskModal />
</template>

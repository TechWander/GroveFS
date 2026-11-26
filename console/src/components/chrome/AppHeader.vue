<script setup lang="ts">
/*
  The sticky chrome: what the console is, where you are, who you are.

  The five destinations come from the router's own table (`PAGES`), so a page
  cannot be listed in the nav and missing from the routes — the old file kept
  that list in three places and relied on nobody editing just one of them.

  The current page is marked twice, as `.on` (the raised chip the stylesheet
  draws) and as `aria-current`, because a colour is not an announcement.
*/
import { computed } from 'vue'
import { useRoute } from 'vue-router'
import { PAGES, router } from '@/router'
import { useSessionStore } from '@/stores/session'
import { useUiStore } from '@/stores/ui'
import { t } from '@/i18n'
import BrandMark from './BrandMark.vue'
import LangToggle from './LangToggle.vue'
import ThemeToggle from './ThemeToggle.vue'
import NavIcon from './NavIcon.vue'

const session = useSessionStore()
const ui = useUiStore()
const route = useRoute()

const current = computed(() => String(route.name ?? 'overview'))

/** Going to another page mounts it, and a page loads on mount. Pressing the
    tab you are already on is the one case that needs saying so: that is a
    refresh, exactly as the old `go(page)` was. */
function go(name: string): void {
  if (current.value === name) ui.reload()
  else void router.push('/' + name)
}

async function signOut(): Promise<void> {
  await session.signOut()
  ui.toast(t('signed out'))
}
</script>

<template>
  <header>
    <span class="brand">
      <BrandMark variant="header" />
      GroveFS
    </span>
    <nav :aria-label="t('Sections of the console')">
      <button
        v-for="(p, i) in PAGES"
        :key="p.name"
        type="button"
        :class="{ on: current === p.name }"
        :aria-current="current === p.name ? 'page' : undefined"
        :title="t(p.label)"
        :aria-label="t(p.label)"
        @click="go(p.name)"
      >
        <span class="idx" aria-hidden="true">{{ String(i + 1).padStart(2, '0') }}</span>
        <NavIcon :name="p.icon" />
        <span>{{ t(p.label) }}</span>
      </button>
    </nav>
    <div class="who">
      <ThemeToggle />
      <LangToggle />
      <span id="who-account">{{ session.account }}</span>
      <span id="who-role" :class="['badge', session.role === 'admin' ? 'admin' : '']">
        {{ session.role }}
      </span>
      <button id="logout" type="button" class="link" @click="signOut">{{ t('Sign out') }}</button>
    </div>
  </header>
</template>

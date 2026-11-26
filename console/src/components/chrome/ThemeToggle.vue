<script setup lang="ts">
/*
  The light/dark switch, parked beside the language toggle in the header.

  It is an icon button, not a word: a sun and a moon are read the same in every
  language, so — unlike the language toggle — it needs no dictionary key and no
  "which language am I reading" reasoning. The `title` names the theme it
  switches TO in both languages at once, the way the language toggle's tooltip
  does.

  The glyphs are inline SVG because the gateway's CSP is `img-src 'self'` with
  no icon CDN; an inline `<svg>` is markup, not a fetched resource, so it renders
  where a `<img src="…data:…">` would be blocked.
*/
import { toggleTheme, useTheme } from '@/theme'

const { theme } = useTheme()
</script>

<template>
  <button
    type="button"
    class="theme-btn"
    :aria-label="theme === 'dark' ? 'Switch to light theme' : 'Switch to dark theme'"
    :aria-pressed="theme === 'dark'"
    title="深色 / 浅色 · dark / light"
    @click="toggleTheme"
  >
    <svg
      v-if="theme === 'dark'"
      viewBox="0 0 24 24"
      width="16"
      height="16"
      aria-hidden="true"
      fill="none"
      stroke="currentColor"
      stroke-width="1.7"
      stroke-linecap="round"
    >
      <circle cx="12" cy="12" r="4.2" />
      <path d="M12 2.5v2.2M12 19.3v2.2M4.2 4.2l1.6 1.6M18.2 18.2l1.6 1.6M2.5 12h2.2M19.3 12h2.2M4.2 19.8l1.6-1.6M18.2 5.8l1.6-1.6" />
    </svg>
    <svg
      v-else
      viewBox="0 0 24 24"
      width="16"
      height="16"
      aria-hidden="true"
      fill="none"
      stroke="currentColor"
      stroke-width="1.7"
      stroke-linecap="round"
      stroke-linejoin="round"
    >
      <path d="M20 14.5A8.2 8.2 0 0 1 9.5 4 8.2 8.2 0 1 0 20 14.5z" />
    </svg>
  </button>
</template>

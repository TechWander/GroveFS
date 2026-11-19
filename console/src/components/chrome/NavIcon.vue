<script setup lang="ts">
/*
  The five nav glyphs, inline, single-path, `currentColor`.

  They inherit the button's colour, so the segmented rail's hover and `.on`
  states recolour icon and label together — the reason these are `<svg>` DOM and
  not images the CSP would have to be widened for.
*/
const PATHS: Record<string, string> = {
  overview: 'M1 1h6v6H1V1zm8 0h6v6H9V1zM1 9h6v6H1V9zm8 0h6v6H9V9z',
  views:
    'M2 1.5A1.5 1.5 0 0 1 3.5 0h5A1.5 1.5 0 0 1 10 1.5v3A1.5 1.5 0 0 1 8.5 6H7v3h5.5A1.5 1.5 0 0 1 14 10.5v3a1.5 1.5 0 0 1-1.5 1.5h-3A1.5 1.5 0 0 1 8 13.5V13H7v.5A1.5 1.5 0 0 1 5.5 15h-3A1.5 1.5 0 0 1 1 13.5v-3A1.5 1.5 0 0 1 2.5 9H5V6H3.5A1.5 1.5 0 0 1 2 4.5v-3zM3 2v3h5V2H3zm0 9v3h3v-3H3zm7 0v3h3v-3h-3z',
  keys: 'M10.5 0a5.5 5.5 0 0 0-5.36 6.75L.44 11.44a1.5 1.5 0 0 0-.44 1.06V15a1 1 0 0 0 1 1h3a1 1 0 0 0 1-1v-1h1a1 1 0 0 0 1-1v-1h1c.4 0 .77-.16 1.06-.44l.38-.38A5.5 5.5 0 1 0 10.5 0zm2 5.5a1.5 1.5 0 1 1-3 0 1.5 1.5 0 0 1 3 0z',
  accounts:
    'M8 0a3.5 3.5 0 1 1 0 7 3.5 3.5 0 0 1 0-7zM2 14.5C2 11.5 4.7 9.5 8 9.5s6 2 6 5a1.5 1.5 0 0 1-1.5 1.5h-9A1.5 1.5 0 0 1 2 14.5z',
  audit:
    'M2 0h9l3 3v13a1 1 0 0 1-1 1H2a1 1 0 0 1-1-1V1a1 1 0 0 1 1-1zm2 5h8v1.5H4V5zm0 3.5h8V10H4V8.5zm0 3.5h5v1.5H4V12z',
}

const props = defineProps<{ name: string }>()
</script>

<template>
  <svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true">
    <path fill="currentColor" :d="PATHS[props.name] ?? ''" />
  </svg>
</template>

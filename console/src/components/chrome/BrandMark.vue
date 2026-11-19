<script setup lang="ts">
/*
  The GroveFS mark: three data blocks growing into a tree (the same drawing as
  `/favicon.svg`).

  Inline `<svg>` DOM, not a `data:` image, because the gateway's CSP allows
  `img-src 'self'` only — and an inline mark inherits colours and scales without
  a second request. `login` is the gradient card mark, `header` the flat one.
*/
const props = withDefaults(defineProps<{ variant?: 'login' | 'header' }>(), { variant: 'header' })
</script>

<template>
  <svg
    v-if="props.variant === 'login'"
    class="logo"
    viewBox="0 0 32 32"
    width="44"
    height="44"
    aria-hidden="true"
  >
    <defs>
      <linearGradient id="lglg" x1="0" y1="0" x2="0" y2="1">
        <stop offset="0" stop-color="#2b82e0" />
        <stop offset="1" stop-color="#1367c8" />
      </linearGradient>
    </defs>
    <rect width="32" height="32" rx="8" fill="url(#lglg)" />
    <g fill="#fff">
      <rect x="11.5" y="4" width="9" height="8" rx="2" />
      <rect x="6.5" y="14" width="9" height="8" rx="2" />
      <rect x="16.5" y="14" width="9" height="8" rx="2" />
      <rect x="14" y="24" width="4" height="5" rx="1.5" />
    </g>
  </svg>
  <svg v-else class="logo" viewBox="0 0 32 32" width="22" height="22" aria-hidden="true">
    <rect width="32" height="32" rx="7" fill="#1367c8" />
    <g fill="#fff">
      <rect x="11.5" y="4" width="9" height="8" rx="2" />
      <rect x="6.5" y="14" width="9" height="8" rx="2" />
      <rect x="16.5" y="14" width="9" height="8" rx="2" />
      <rect x="14" y="24" width="4" height="5" rx="1.5" />
    </g>
  </svg>
</template>

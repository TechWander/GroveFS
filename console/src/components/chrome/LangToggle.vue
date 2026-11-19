<script setup lang="ts">
/*
  The language switch, in two places: the header, and parked top-right on the
  sign-in card (`.lang-login` is what positions it).

  It names the language it switches TO — an English UI offers 中文 and a Chinese
  UI offers "English" — so the button can be read without knowing which language
  you are currently reading. The tooltip says BOTH names at once, in either
  language, which is why it is a literal and not a dictionary key.
*/
import { langButtonLabel, toggleLang } from '@/i18n'

const props = defineProps<{ onLogin?: boolean }>()
</script>

<template>
  <button
    type="button"
    :class="['lang-btn', props.onLogin ? 'lang-login' : 'lang-app']"
    title="切换语言 / switch language"
    @click="toggleLang"
  >
    {{ langButtonLabel() }}
  </button>
</template>

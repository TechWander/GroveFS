<script setup lang="ts">
/*
  The sign-in card — the whole of what an anonymous visitor gets.

  Every id the stylesheet addresses (`#login`, `#login-form`, `#login-btn`,
  `#login-error`, and the two inputs) is kept, because this page's appearance is
  `app.css` and that file was not rewritten along with the script.

  Two behaviours the old file had and this one must not lose:
  · the password field is cleared only when the sign-in TOOK, so a typo in the
    account name does not make someone retype a ten-character secret;
  · the failure line is the SERVER's sentence (`sign-in failed (HTTP 500)` when
    it sent none), because "why am I locked out" is answered by the reason.

  What is added: `autocomplete` is honoured, the account field takes focus when
  the card appears, the button says `aria-busy` while the POST is in flight, and
  the error is a live region — so the answer is announced, not just painted.
*/
import { nextTick, onMounted, ref } from 'vue'
import { useSessionStore } from '@/stores/session'
import { t } from '@/i18n'
import BrandMark from './BrandMark.vue'
import LangToggle from './LangToggle.vue'

const session = useSessionStore()

const account = ref('')
const password = ref('')
const accountEl = ref<HTMLInputElement | null>(null)

async function submit(): Promise<void> {
  const name = account.value.trim()
  if (!name || !password.value) return
  if (await session.signIn(name, password.value)) password.value = ''
}

onMounted(() => {
  void nextTick(() => accountEl.value?.focus())
})
</script>

<template>
  <section id="login">
    <div class="login-card">
      <div class="brand-row">
        <BrandMark variant="login" />
        <h1>GroveFS</h1>
      </div>
      <p class="sub">{{ t('Governance console') }}</p>
      <LangToggle on-login />
      <form id="login-form" autocomplete="on" @submit.prevent="submit">
        <label for="login-account">{{ t('Account') }}</label>
        <input
          id="login-account"
          ref="accountEl"
          v-model="account"
          name="username"
          autocomplete="username"
          spellcheck="false"
          required
        />
        <label for="login-password">{{ t('Password') }}</label>
        <input
          id="login-password"
          v-model="password"
          name="password"
          type="password"
          autocomplete="current-password"
          required
        />
        <button
          id="login-btn"
          type="submit"
          class="primary"
          :disabled="session.busy"
          :aria-busy="session.busy ? 'true' : undefined"
        >
          {{ t('Sign in') }}
          <span v-if="session.busy" class="spin" aria-hidden="true"></span>
        </button>
        <p id="login-error" class="err" role="alert" aria-live="assertive">
          {{ session.notice }}
        </p>
      </form>
    </div>
  </section>
</template>

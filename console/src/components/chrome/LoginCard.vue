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
    <div class="login-shell">
      <!-- The left pane is a drawing, not a pitch: a meadow of canopies at
           dusk. It is inline SVG because the gateway's CSP allows `img-src
           'self'` only, and it scales with the card rather than shipping a
           bitmap at one size. -->
      <aside class="login-aside" aria-hidden="true">
        <svg
          class="login-scene"
          viewBox="0 0 640 560"
          preserveAspectRatio="xMidYMid slice"
          xmlns="http://www.w3.org/2000/svg"
        >
          <defs>
            <linearGradient id="scene-sky" x1="0" y1="0" x2="0" y2="1">
              <stop offset="0" stop-color="#0b1d16" />
              <stop offset="0.55" stop-color="#153026" />
              <stop offset="1" stop-color="#1d3f2f" />
            </linearGradient>
            <radialGradient id="scene-moon" cx="0.5" cy="0.5" r="0.5">
              <stop offset="0" stop-color="#f6eed8" stop-opacity="0.95" />
              <stop offset="0.45" stop-color="#e9dfc2" stop-opacity="0.28" />
              <stop offset="1" stop-color="#e9dfc2" stop-opacity="0" />
            </radialGradient>
            <linearGradient id="scene-hill" x1="0" y1="0" x2="0" y2="1">
              <stop offset="0" stop-color="#1a3a2c" />
              <stop offset="1" stop-color="#0e241a" />
            </linearGradient>
            <linearGradient id="scene-meadow" x1="0" y1="0" x2="0" y2="1">
              <stop offset="0" stop-color="#24523c" />
              <stop offset="0.5" stop-color="#173b2b" />
              <stop offset="1" stop-color="#0a1c14" />
            </linearGradient>
            <linearGradient id="scene-canopy" x1="0" y1="0" x2="0" y2="1">
              <stop offset="0" stop-color="#2f6f52" />
              <stop offset="1" stop-color="#16402f" />
            </linearGradient>
            <linearGradient id="scene-canopy-light" x1="0" y1="0" x2="0" y2="1">
              <stop offset="0" stop-color="#3d8563" />
              <stop offset="1" stop-color="#21553d" />
            </linearGradient>
          </defs>

          <rect width="640" height="560" fill="url(#scene-sky)" />
          <circle cx="500" cy="96" r="148" fill="url(#scene-moon)" />
          <circle cx="500" cy="96" r="44" fill="#f4ecd6" opacity="0.92" />
          <circle cx="484" cy="82" r="42" fill="#12291f" opacity="0.2" />

          <!-- far ridge -->
          <path
            d="M0 300 C 90 278 170 292 250 280 C 340 266 400 296 480 282 C 550 270 600 282 640 272 L640 560 L0 560 Z"
            fill="#132c22"
            opacity="0.85"
          />
          <!-- near ridge -->
          <path
            d="M0 340 C 90 318 170 332 260 320 C 360 306 430 338 520 324 C 570 316 610 322 640 316 L640 560 L0 560 Z"
            fill="url(#scene-hill)"
          />

          <!-- crown tree, mid-left -->
          <g class="sway s1">
            <path d="M165 432 L165 332 M165 374 L150 354 M165 390 L182 364" stroke="#0d2219" stroke-width="8" stroke-linecap="round" fill="none" />
            <ellipse cx="165" cy="300" rx="78" ry="66" fill="url(#scene-canopy)" />
            <ellipse cx="123" cy="330" rx="44" ry="38" fill="url(#scene-canopy)" />
            <ellipse cx="207" cy="326" rx="46" ry="40" fill="url(#scene-canopy)" />
            <ellipse cx="165" cy="280" rx="40" ry="30" fill="url(#scene-canopy-light)" opacity="0.5" />
          </g>

          <!-- crown tree, right and taller -->
          <g class="sway s2">
            <path d="M470 444 L470 318 M470 364 L452 340 M470 382 L492 352" stroke="#0d2219" stroke-width="9" stroke-linecap="round" fill="none" />
            <ellipse cx="470" cy="286" rx="72" ry="62" fill="url(#scene-canopy)" />
            <ellipse cx="430" cy="316" rx="40" ry="34" fill="url(#scene-canopy)" />
            <ellipse cx="508" cy="312" rx="42" ry="36" fill="url(#scene-canopy)" />
            <ellipse cx="470" cy="266" rx="36" ry="28" fill="url(#scene-canopy-light)" opacity="0.48" />
          </g>

          <!-- crown tree, left edge -->
          <g class="sway s3">
            <path d="M72 460 L72 414 M72 436 L63 424" stroke="#0d2219" stroke-width="5" stroke-linecap="round" fill="none" />
            <ellipse cx="72" cy="400" rx="38" ry="32" fill="url(#scene-canopy)" />
            <ellipse cx="56" cy="418" rx="22" ry="18" fill="url(#scene-canopy)" />
            <ellipse cx="90" cy="416" rx="23" ry="19" fill="url(#scene-canopy)" />
          </g>

          <!-- the meadow -->
          <path
            d="M0 396 C 110 380 210 402 316 392 C 420 382 512 400 640 386 L640 560 L0 560 Z"
            fill="url(#scene-meadow)"
          />
          <path
            d="M0 486 C 120 470 260 492 392 480 C 490 472 560 484 640 476 L640 560 L0 560 Z"
            fill="#0a1c14"
            opacity="0.7"
          />

          <!-- foreground crowns, rooted in the grass -->
          <g class="sway s3">
            <path d="M104 528 L104 476 M104 500 L92 486" stroke="#0d2219" stroke-width="5" stroke-linecap="round" fill="none" />
            <ellipse cx="104" cy="462" rx="36" ry="30" fill="url(#scene-canopy)" />
            <ellipse cx="86" cy="480" rx="21" ry="17" fill="url(#scene-canopy)" />
            <ellipse cx="122" cy="478" rx="22" ry="18" fill="url(#scene-canopy)" />
          </g>
          <g class="sway s2">
            <path d="M272 544 L272 514" stroke="#0d2219" stroke-width="3.5" stroke-linecap="round" />
            <ellipse cx="272" cy="504" rx="24" ry="20" fill="url(#scene-canopy)" />
            <ellipse cx="258" cy="516" rx="15" ry="12" fill="url(#scene-canopy)" />
            <ellipse cx="286" cy="514" rx="16" ry="12" fill="url(#scene-canopy)" />
          </g>
          <g class="sway s1">
            <path d="M566 528 L566 482 M566 506 L556 494 M566 512 L578 496" stroke="#0d2219" stroke-width="5.5" stroke-linecap="round" fill="none" />
            <ellipse cx="566" cy="470" rx="40" ry="34" fill="url(#scene-canopy)" />
            <ellipse cx="546" cy="490" rx="23" ry="19" fill="url(#scene-canopy)" />
            <ellipse cx="586" cy="488" rx="24" ry="20" fill="url(#scene-canopy)" />
            <ellipse cx="566" cy="456" rx="20" ry="15" fill="url(#scene-canopy-light)" opacity="0.4" />
          </g>

          <!-- grass, in three tonal passes -->
          <g stroke="#2c6148" stroke-width="2.2" stroke-linecap="round" fill="none" opacity="0.9">
            <path d="M26 512 q4 -18 2 -28" />
            <path d="M48 518 q-3 -22 1 -32" />
            <path d="M76 508 q5 -17 3 -25" />
            <path d="M118 522 q-4 -20 0 -30" />
            <path d="M154 514 q4 -18 2 -27" />
            <path d="M202 526 q-3 -22 1 -32" />
            <path d="M250 512 q4 -17 3 -25" />
            <path d="M304 526 q-4 -20 0 -30" />
            <path d="M350 518 q4 -18 2 -27" />
            <path d="M402 528 q-3 -22 1 -32" />
            <path d="M452 516 q4 -17 3 -25" />
            <path d="M502 526 q-4 -20 0 -30" />
            <path d="M552 518 q4 -18 2 -27" />
            <path d="M598 528 q-3 -22 1 -32" />
          </g>
          <g stroke="#3f7f5e" stroke-width="2" stroke-linecap="round" fill="none" opacity="0.75">
            <path d="M60 546 q4 -20 2 -32" />
            <path d="M104 552 q-3 -24 0 -36" />
            <path d="M172 542 q5 -19 3 -29" />
            <path d="M226 552 q-3 -24 0 -36" />
            <path d="M282 542 q5 -19 3 -29" />
            <path d="M336 552 q-3 -24 0 -36" />
            <path d="M392 544 q5 -19 3 -29" />
            <path d="M444 554 q-3 -24 0 -36" />
            <path d="M500 544 q5 -19 3 -29" />
            <path d="M556 554 q-3 -24 0 -36" />
          </g>
          <g stroke="#1b4734" stroke-width="2.8" stroke-linecap="round" fill="none" opacity="0.9">
            <path d="M14 560 q7 -26 3 -40" />
            <path d="M84 560 q-5 -30 0 -44" />
            <path d="M140 560 q8 -25 4 -38" />
            <path d="M210 560 q-5 -30 0 -44" />
            <path d="M264 560 q8 -25 4 -38" />
            <path d="M348 560 q-5 -30 0 -44" />
            <path d="M414 560 q8 -25 4 -38" />
            <path d="M470 560 q-5 -30 0 -44" />
            <path d="M530 560 q8 -25 4 -38" />
            <path d="M606 560 q-5 -30 0 -44" />
          </g>

          <!-- a few quiet lights over the grass -->
          <g fill="#e8d9a8">
            <circle cx="128" cy="452" r="2.2" opacity="0.55" />
            <circle cx="246" cy="470" r="1.7" opacity="0.4" />
            <circle cx="356" cy="446" r="2" opacity="0.5" />
            <circle cx="438" cy="472" r="1.5" opacity="0.35" />
            <circle cx="512" cy="456" r="1.9" opacity="0.45" />
          </g>
        </svg>
        <!-- The one line the drawing is actually saying. It sits in the empty
             sky at the top-left: the moon is at x≈500/640 and the canopies
             start below y≈230, so nothing here is ever covered — and because
             `preserveAspectRatio="…slice"` crops the SVG vertically from the
             centre, an HTML overlay (not SVG <text>) is what stays put. Both
             languages are shown at once on purpose: this is a brand lockup, not
             a string the console translates, so it deliberately does NOT go
             through t(). The pane is aria-hidden, and a slogan is exactly what
             that attribute is for. -->
        <div class="login-caption">
          <p class="login-caption-en">One Grove, Govern All Trees</p>
          <p class="login-caption-zh">一域成林，统御诸树。</p>
        </div>
      </aside>

      <div class="login-main">
        <LangToggle on-login />
        <div class="login-card">
          <div class="login-mobile-brand">
            <BrandMark variant="header" />
            <span>GroveFS</span>
          </div>
          <p class="login-eyebrow">{{ t('Governance console') }}</p>
          <h2>{{ t('Sign in') }}</h2>
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
      </div>
    </div>
  </section>
</template>

import { ref } from 'vue'
import { ZH } from './zh'

/*
  Two-language UI, with the dictionary's own rules kept intact.

  `t()` is called with the ENGLISH sentence as the key, so code reads
  naturally, a missing entry degrades to that sentence instead of throwing, and
  only one table (`zh.ts`) needs maintenance. Interpolated sentences use their
  template as the key — `${var}` is substituted AFTER lookup, so nothing is ever
  concatenated into a key.

  This is deliberately not vue-i18n: the message compiler the plain
  `t('… {n} …')` API needs is a runtime one (`new Function`), and the gateway's
  CSP is `script-src 'self'` without `unsafe-eval`. Pre-compiling every message
  through `unplugin-vue-i18n` would work, but it would also mean re-writing 430
  sentences whose text doubles as their key — including `@`, `$` and `|`, which
  that engine reads as its own syntax. The semantics below are the ones the
  hand-written console shipped with, now reactive: `lang` is a ref, so every
  component that called `t()` in a template re-renders on a switch.
*/

export type Lang = 'en' | 'zh'

/** One interpolation table; `null`/`undefined` renders as the CLI would. */
export type Vars = Record<string, string | number | null | undefined>

const STORAGE_KEY = 'gfs.lang'

function detect(): Lang {
  try {
    const saved = localStorage.getItem(STORAGE_KEY)
    if (saved === 'en' || saved === 'zh') return saved
  } catch {
    /* storage denied: the default stands */
  }
  return (navigator.language || '').toLowerCase().startsWith('zh') ? 'zh' : 'en'
}

/** Module-level, because the sign-in card and the app chrome need the same
    answer before any store exists. */
const lang = ref<Lang>(detect())

export function t(key: string, vars?: Vars): string {
  let s = (lang.value === 'zh' && ZH[key]) || key
  if (vars) {
    for (const [name, value] of Object.entries(vars)) {
      s = s.split('${' + name + '}').join(String(value))
    }
  }
  return s
}

export function setLang(next: Lang): void {
  lang.value = next
  try {
    localStorage.setItem(STORAGE_KEY, next)
  } catch {
    /* non-persistent is fine */
  }
}

export function toggleLang(): void {
  setLang(lang.value === 'zh' ? 'en' : 'zh')
}

/** The label of the toggle itself: it names the language it switches TO. */
export const langButtonLabel = () => (lang.value === 'zh' ? 'English' : '中文')

export function useI18n() {
  return { lang, t, setLang, toggleLang, langButtonLabel }
}

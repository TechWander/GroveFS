import { ref } from 'vue'

/*
  The light/dark switch, alongside the language switch.

  It mirrors the i18n module exactly for one reason: the sign-in card and the
  app chrome both need the same answer BEFORE any Pinia store exists, so the
  state is a module-level `ref` rather than a store, and it persists under its
  own localStorage key. The default is not hardcoded to light — with no saved
  choice it follows the OS `prefers-color-scheme`, so an operator who runs the
  whole desktop dark gets a dark console on first load.

  The mechanism is a single attribute on the root element:
  `<html data-theme="dark">`. console.css keys its dark token overrides off
  `:root[data-theme='dark']`, so flipping one attribute moves every component
  at once and nothing needs to re-render through Vue.

  CSP note: the gateway serves `style-src 'self'` with no inline styles, so the
  theme CANNOT be a runtime-written `style` attribute — it is an attribute the
  stylesheet reads, which is exactly what works under that policy.
*/

export type Theme = 'light' | 'dark'

const STORAGE_KEY = 'gfs.theme'

function detect(): Theme {
  try {
    const saved = localStorage.getItem(STORAGE_KEY)
    if (saved === 'light' || saved === 'dark') return saved
  } catch {
    /* storage denied: fall through to the OS preference */
  }
  try {
    return window.matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light'
  } catch {
    return 'light'
  }
}

/** Module-level, so the theme is decided before the shell mounts. */
const theme = ref<Theme>(detect())

function applyToDocument(): void {
  document.documentElement.dataset.theme = theme.value
}

/** Wire the attribute once at import; the caller in main.ts does not have to
    remember to, so a page can never paint in the wrong theme for a frame. */
applyToDocument()

export function setTheme(next: Theme): void {
  theme.value = next
  applyToDocument()
  try {
    localStorage.setItem(STORAGE_KEY, next)
  } catch {
    /* non-persistent is fine */
  }
}

export function toggleTheme(): void {
  setTheme(theme.value === 'dark' ? 'light' : 'dark')
}

/** The label names the theme it switches TO, like the language toggle: a light
    UI offers "Dark", a dark UI offers "Light". */
export const themeButtonLabel = () => (theme.value === 'dark' ? 'Light' : 'Dark')

export function useTheme() {
  return { theme, setTheme, toggleTheme, themeButtonLabel }
}

import { t } from '@/i18n'
import { useUiStore } from '@/stores/ui'

/*
  Copy to the clipboard, the way it actually works on a management network.

  The console usually runs on plain HTTP inside the cluster, where the async
  Clipboard API is unavailable (it is secure-context only), so the deprecated
  but working `execCommand` path stays — and its probe textarea is styled by
  CLASS, because the gateway's CSP (`style-src 'self'`) forbids the inline
  `style` attribute a framework would otherwise write here.
*/

export function useClipboard() {
  const ui = useUiStore()

  /** Must be called from inside the click handler: the fallback loses its right
      to touch the clipboard as soon as the gesture is gone. */
  function copy(text: string): void {
    const done = () => ui.toast(t('copied to clipboard'), 'good')
    const fallback = () => {
      const ta = document.createElement('textarea')
      ta.value = text
      ta.className = 'copy-probe'
      document.body.appendChild(ta)
      ta.select()
      try {
        document.execCommand('copy')
        done()
      } catch {
        /* nothing else to try */
      } finally {
        ta.remove()
      }
    }
    if (navigator.clipboard && window.isSecureContext) {
      void navigator.clipboard.writeText(text).then(done, fallback)
      return
    }
    fallback()
  }

  return { copy }
}

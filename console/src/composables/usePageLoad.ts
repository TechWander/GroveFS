import { onMounted, ref, watch } from 'vue'
import { useUiStore } from '@/stores/ui'
import { messageOf } from '@/utils/error'

/*
  What the old `go(page)` did for every page, in one place.

  It painted `loading…` into the section, ran the page, and — if the page threw —
  replaced the section with the server's sentence in an `.err` paragraph. Two
  states every page therefore has, and no page should get to forget one of them:
  a console whose list silently stays empty is indistinguishable from a console
  whose list is legitimately empty.

  It also subscribes the page to `ui.reload()`, which is what an action calls
  after it changes something. A page that refetches on mount but not after a verb
  shows a record that no longer exists.
*/
export function usePageLoad(load: () => Promise<void>) {
  const pending = ref(false)
  const error = ref('')
  const ui = useUiStore()

  async function run(): Promise<void> {
    pending.value = true
    error.value = ''
    try {
      await load()
    } catch (e) {
      error.value = messageOf(e)
    } finally {
      pending.value = false
    }
  }

  onMounted(() => void run())
  watch(
    () => ui.nonce,
    () => void run(),
  )

  return { pending, error, run }
}

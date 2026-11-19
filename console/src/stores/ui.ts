import { ref } from 'vue'
import { defineStore } from 'pinia'
import type { Vars } from '@/i18n'
import type { AccountRow, AuditRow, GrantRow, KeyRow, KeySecret } from '@/types'

/*
  The three overlays the console owns: toasts, the confirmation prompt, and the
  row drawer.

  A toast is the loud answer (it wraps, so a two-line ACL sentence stays
  readable) and a refusal that a dialog CAN show inline does exactly that, in
  the dialog the user is looking at, where the text they just typed is still on
  screen. Only one drawer exists at a time: opening a second row replaces the
  first, and a reload closes it — a stale pane beside fresh rows is worse than
  no pane, because its record may have been edited, revoked or deleted in the
  meantime.
*/

export type ToastKind = 'info' | 'good' | 'bad'

export interface ToastItem {
  id: number
  msg: string
  kind: ToastKind
}

/** The irreversible-action prompt. It shows the NAME and the consequence,
    because "Are you sure?" is answered without reading it.

    Every field is a DICTIONARY KEY (with `vars` for the interpolated one), so a
    language switch while the prompt is open re-renders it like everything else.
    `confirm` is the verb, not "OK": the button says what it will do. */
export interface AskState {
  title: string
  body: string
  vars?: Vars
  confirm: string
}

/** One record's whole row in the second pane. `keys: null` means the key
    registry is not this account's to read — a different answer from "no keys". */
export type DrawerEntry =
  | { kind: 'audit'; row: AuditRow }
  | { kind: 'key'; row: KeyRow }
  | {
      kind: 'account'
      row: AccountRow
      grants: GrantRow[]
      keys: KeyRow[] | null
      isSelf: boolean
    }

/** The one-time secret. It lives at shell level, and not in the page or the
    drawer that minted it, for one reason: a reload closes the drawer it was
    opened from, and a secret that vanishes because the list behind it refreshed
    is a credential lost for good — `Rotate` is the only way to get one back.

    `detailed` is the create flow (the whole record, so there is nothing to
    guess about what was minted); the rotate flow says less, because the row is
    already on screen and only the secret changed. */
export interface SecretState {
  title: string
  detailed: boolean
  secret: KeySecret
  /** The view the REQUEST asked for; the reply reports the key, not the row. */
  view: string | null
}

export const useUiStore = defineStore('ui', () => {
  const toasts = ref<ToastItem[]>([])
  const asking = ref<AskState | null>(null)
  const drawer = ref<DrawerEntry | null>(null)
  const secret = ref<SecretState | null>(null)

  let seq = 0
  const timers = new Map<number, number>()
  /** The prompt's still-open promise, kept outside `state` so a template can
      never resolve it twice. */
  let settle: ((value: boolean) => void) | null = null

  /** A failure stays up five times as long as a success: it is the one message
      the user has to read before the page moves on. */
  function toast(msg: string, kind: ToastKind = 'info'): void {
    const item: ToastItem = { id: ++seq, msg, kind }
    toasts.value = [...toasts.value, item]
    timers.set(
      item.id,
      window.setTimeout(() => dismissToast(item.id), kind === 'bad' ? 15000 : 5000),
    )
  }

  function dismissToast(id: number): void {
    const handle = timers.get(id)
    if (handle !== undefined) window.clearTimeout(handle)
    timers.delete(id)
    toasts.value = toasts.value.filter((x) => x.id !== id)
  }

  /** Resolves `false` when the prompt is dismissed (Cancel, Esc, a replaced
      prompt) and `true` only when the named verb is pressed. */
  function ask(title: string, body: string, vars?: Vars, confirmText?: string): Promise<boolean> {
    settle?.(false)
    return new Promise<boolean>((resolve) => {
      settle = resolve
      asking.value = { title, body, vars, confirm: confirmText ?? 'Confirm' }
    })
  }

  function answer(value: boolean): void {
    const resolve = settle
    settle = null
    asking.value = null
    resolve?.(value)
  }

  function openDrawer(entry: DrawerEntry): void {
    drawer.value = entry
  }

  function closeDrawer(): void {
    drawer.value = null
  }

  /** The page's "read it again" signal, and the closest thing to the old
      global `reload()`.

      A page watches this and refetches; an action that changed something bumps
      it. It has to be a counter rather than a boolean because a second reload
      of the same page is a different event, and it closes the drawer because a
      drawer is a snapshot of ONE row — after a reload its record may be edited,
      revoked or gone, and a stale pane beside fresh rows is worse than no pane. */
  const nonce = ref(0)

  function reload(): void {
    closeDrawer()
    nonce.value++
  }

  function showSecret(state: SecretState): void {
    secret.value = state
  }

  return {
    toasts,
    asking,
    drawer,
    secret,
    nonce,
    toast,
    dismissToast,
    ask,
    answer,
    openDrawer,
    closeDrawer,
    showSecret,
    reload,
  }
})

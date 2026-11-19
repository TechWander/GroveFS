/*
  Which overlay owns the `Esc` key.

  A dialog can open on top of a drawer (the key pane's Edit verb is a modal
  sitting beside the record it is about to change), and in the hand-written
  console a single global listener closed the drawer no matter what was on top
  of it — so one `Esc` dismissed the pane behind the dialog you were still
  reading. This keeps a stack: an overlay acts on `Esc` only while it is the
  topmost one, and the drawer is never the topmost while a dialog is open.
*/

const stack: number[] = []
let seq = 0

export interface OverlayHandle {
  id: number
  /** Is this still the frontmost overlay? Checked per key event, not cached. */
  isTop: () => boolean
  pop: () => void
}

export function pushOverlay(): OverlayHandle {
  const id = ++seq
  stack.push(id)
  return {
    id,
    isTop: () => stack[stack.length - 1] === id,
    pop: () => {
      const at = stack.indexOf(id)
      if (at >= 0) stack.splice(at, 1)
    },
  }
}

/** `Esc` is the one keyboard affordance a second pane needs, and it belongs to
    the overlay itself rather than to whichever page happens to be open. */
export function onEscape(isTop: () => boolean, handler: () => void): () => void {
  const listener = (ev: KeyboardEvent) => {
    if (ev.key === 'Escape' && isTop()) handler()
  }
  document.addEventListener('keydown', listener)
  return () => document.removeEventListener('keydown', listener)
}

import type { StorageInfo, StoreInfo, StoreInstance } from '@/types'

/*
  Which engine carries the TREES — the one fact the console must not get wrong.

  A deployment answers with TWO backend names, and they legitimately differ:

    · `backend`         — the CONTROL plane: definitions, keys, accounts, audit.
                          A treedb deployment keeps it in local SQLite, so this
                          field says `sqlite` while the trees sit in the engine.
    · `metadataBackend` — where the physical trees live: `sqlite`, `postgres`
                          or `treedb`. `storage.backend` says the same thing
                          from inside the storage snapshot.

  Reading the first as if it were the second is how a treedb box announced
  itself as a SQLite deployment — and the reverse mistake shows a reader the
  wrong engine's blocks (instance cards on an embedded store, a "local SQLite"
  note on a PG box). Every engine-dependent block in this console therefore
  asks THIS module and nothing else, and never falls back to the control-plane
  name: an unknown answer is shown as unknown rather than guessed into one of
  the three layouts.
*/

export type Engine = 'sqlite' | 'postgres' | 'treedb' | 'unknown'

const KNOWN = ['sqlite', 'postgres', 'treedb'] as const

function pick(name?: string | null): Engine {
  return (KNOWN as readonly string[]).includes(name ?? '') ? (name as Engine) : 'unknown'
}

/** The engine from a `store.info` snapshot: the trees' own name first, then the
    storage block's (the same fact, one read apart), then unknown. */
export function treeEngine(info: StoreInfo | null | undefined): Engine {
  if (!info) return 'unknown'
  if (info.metadataBackend) return pick(info.metadataBackend)
  return pick(info.storage?.backend)
}

/** The same answer from a bare `storage` block (the catalog's cached slice). */
export function storageEngine(s: StorageInfo | null | undefined): Engine {
  return s ? pick(s.backend) : 'unknown'
}

/**
  One card's identity. The registry addresses an instance by NAME, and a name
  is only unique once the operator stops hand-editing the book — an old treedb
  adoption wrote every serving address as `primary`, which collapsed two cards
  into one key and made the second server unreachable from every verb. The
  address is what actually distinguishes them, so pairing the two is the only
  key that cannot collide; the page still PRINTS the name.
*/
export function instanceKey(i: StoreInstance): string {
  return `${i.name}@${i.uri ?? ''}`
}

/** The control plane, in the words the deployment itself uses. */
export function controlPlaneName(info: StoreInfo | null | undefined): string {
  return info?.backend || '—'
}

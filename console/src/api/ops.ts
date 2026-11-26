import type {
  AccountRow,
  AuditRow,
  GrantRow,
  ImportResult,
  InfoResult,
  KeyReveal,
  KeyRow,
  KeySecret,
  MintKeyArgs,
  RelocateResult,
  StoreInfo,
  StoragePreview,
  ViewStats,
} from '@/types'

/*
  The console's own promise about what a user can click.

  `Op` is exactly `ALLOWED_OPS` from `web.rs` — the list `/api/session` returns.
  An op absent from it (the four destructive verbs: `gc.scan`/`gc.apply`,
  `refs.rebuild`, `view.rm_base`, `fork.graft`) stays an operator CLI command,
  and a page built against a different binary fails on a NAMED missing op instead
  of a mystery 403. `OpResult` documents what each answer is shaped like; the
  `unknown` rows are the verbs whose payload this UI does not read — the server's
  refusal sentence is what matters there, and it is shown verbatim.
*/

export type Op =
  | 'info'
  | 'whoami'
  | 'store.info'
  | 'def.list'
  | 'def.get'
  | 'def.put'
  | 'def.remove'
  | 'physical.list'
  | 'stamp'
  | 'fork.has'
  | 'fork.create'
  | 'fork.drop'
  | 'view.create'
  | 'view.stats'
  | 'apikey.list'
  | 'apikey.create'
  | 'apikey.revoke'
  | 'apikey.delete'
  | 'apikey.rotate'
  | 'apikey.secret'
  | 'apikey.set_ro'
  | 'apikey.update'
  | 'apikey.calls'
  | 'apikey.purge'
  | 'audit.retention.get'
  | 'audit.retention.set'
  | 'account.list'
  | 'account.create'
  | 'account.passwd'
  | 'account.role'
  | 'account.disable'
  | 'account.delete'
  | 'grant.list'
  | 'grant.add'
  | 'grant.remove'
  | 'storage.add'
  | 'storage.update'
  | 'storage.remove'
  | 'storage.preview'
  | 'storage.set_control'
  | 'storage.relocate'

/** A call whose refusal is worth rendering INLINE instead of as a toast: the
    overview page loads several things at once, and a page that is part answer
    and one refusal sentence needs the sentence where it belongs. */
export type Soft<T> = { ok: true; r: T | null } | { ok: false; error: string }

/** A `def.list` entry: `[name, text]` pairs, or the same as an object. */
export type DefEntry = [string, string] | { name: string; text: string }

export interface OpResult {
  info: InfoResult
  whoami: unknown
  'store.info': StoreInfo
  'def.list': DefEntry[]
  'def.get': unknown
  'def.put': unknown
  'def.remove': unknown
  'physical.list': string[]
  stamp: unknown
  'fork.has': unknown
  'fork.create': ForkStats
  'fork.drop': unknown
  'view.create': string | { note?: string }
  'view.stats': ViewStats
  'apikey.list': KeyRow[]
  'apikey.create': KeySecret
  'apikey.revoke': unknown
  'apikey.delete': boolean
  'apikey.rotate': KeySecret
  'apikey.secret': KeyReveal
  'apikey.set_ro': unknown
  'apikey.update': unknown
  'apikey.calls': AuditRow[]
  'apikey.purge': PurgeResult
  'audit.retention.get': Retention
  'audit.retention.set': Retention & { trimmedNow: number }
  'account.list': AccountRow[]
  'account.create': CreatedAccount
  'account.passwd': unknown
  'account.role': unknown
  'account.disable': unknown
  'account.delete': DeletedAccount
  'grant.list': GrantRow[]
  'grant.add': GrantedBase
  'grant.remove': unknown
  // The storage plane (Super): the managed-registry CRUD + its guards. `add` /
  // `update` / `remove` / `set_control` answer `{ok,msg}` (the msg is the
  // server's own sentence, shown verbatim); `preview` is a StoragePreview; a
  // no-op is refused server-side, so there is no `ok:false` shape to model.
  'storage.add': { ok: boolean; msg: string }
  'storage.update': { ok: boolean; msg: string }
  'storage.remove': { ok: boolean; msg: string }
  'storage.preview': StoragePreview
  'storage.set_control': { ok: boolean; msg: string; restartRequired: boolean }
  'storage.relocate': RelocateResult
}

/** `fork.create` — the metadata tree that was copied. */
export interface ForkStats {
  nodes: number
  blobs: number
}

/** `account.create` — including the server's own note about the new row. */
export interface CreatedAccount {
  account: string
  role: string
  note: string
}

/** `account.delete` — the reply that says how many keys are still live. */
export interface DeletedAccount {
  account: string
  note: string
}

/** `grant.add` — a delegation, with the resolved base name when the operator
    typed an alias instead. */
export interface GrantedBase {
  account: string
  base: string
  asNamed?: boolean
}

/** `apikey.purge` — the cut the server actually made (or would have made). */
export interface PurgeResult {
  before: string
  cutoff: string
  older: number
  deleted: number
  dryRun: boolean
}

/** The audit-retention window; `null` = rows are kept until purged by hand. */
export interface Retention {
  retention: string | null
}

export type { ImportResult, MintKeyArgs }

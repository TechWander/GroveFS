/*
  The wire types of the gateway's `/api/call` replies.

  These are descriptions of what the meta-server already sends, not a new
  contract: the field names are the JSON's (snake_case, as `server_ext.rs`
  writes them) and the optionality is exactly what the server omits. The console
  may not reshape an answer here — a refusal sentence has to arrive with its
  wording intact, so `CallReply.error` stays a string and nothing is defaulted
  into it.
*/

/** What `/api/session` answers as `who` (the server's own `whoami`). */
export interface Who {
  account: string
  role: string
  super: boolean
  bases?: string[]
}

export interface SessionReply {
  ok?: boolean
  signedIn?: boolean
  csrf?: string
  who?: Who
  ops?: string[]
  error?: string
}

export interface LoginReply {
  ok?: boolean
  error?: string
}

/** One governance call: either `result` or `error`, never both. */
export interface CallReply<T = unknown> {
  ok?: boolean
  result?: T
  error?: string
}

/** `info` — the cheap identity/backend answer any signed-in account gets. */
export interface InfoResult {
  backend?: string
  describe?: string
  actor?: string
  role?: string
}

/** One metadata-store instance, as `store.info.storage` reports it.
    `uri` is the FULL connection string, credentials included — the reply is
    Super-only and this page exists so an operator can verify what is really
    configured. Under treedb `control` is always false (its control plane is
    local SQLite) and `serving` marks the one address this meta-server routes
    to; `viewNames` is the card's live list, `null` when the engine is
    unreachable. */
export interface StoreInstance {
  name: string
  control: boolean
  uri: string | null
  placedViews: number | null
  serving?: boolean
  viewNames?: string[] | null
}

/** Which backend carries the trees, and (under PostgreSQL) which instance
    carries each view. SQLite reports one logical instance; treedb reports an
    inventory whose `routingNote` says it is not a router. */
export interface StorageInfo {
  backend: 'sqlite' | 'postgres' | 'treedb'
  instances: StoreInstance[]
  placement: Record<string, string>
  instanceError: string | null
  /** The console may EDIT this registry (a server-adopted `file:` book). An
      operator-owned registry or a non-managed backend reports false. */
  managed?: boolean
  /** A staged PG control handover (`@control-pending=`) awaiting the next start. */
  pendingControl?: string | null
  /** treedb only: the honest experimental flags the panel must surface. */
  experimental?: boolean
  controlPlane?: string
  routingNote?: string
}

/** One placed view of PG's preview: what deleting this instance would strand.
    `nodes` / `distinctRefs` are counted live off the instance's schema; a view
    whose schema is NOT here yet (`hasSchema` false) still blocks the delete. */
export interface PreviewViewRow {
  view: string
  hasSchema: boolean
  nodes: number
  distinctRefs: number
}

/** One instance row of the `grovefs_refs` consistency scan (PG). `error` means
    the instance could not be read → it is UNVERIFIED, never treated as clean. */
export interface PreviewRefsRow {
  instance: string
  holderRows?: number
  zeroRows?: number
  missingViews?: string[]
  error?: string
}

/** `storage.preview` — the deletion preview the dialog shows before a confirm.
    PG and treedb answer with DIFFERENT shapes and the dialog keys off `isPg`
    (the payload carries no trustworthy `backend` for PG):
      · PG     — `views` is an array of {@link PreviewViewRow} objects, the
        affected names sit in `placedViews`, and `refs` carries the shared
        `grovefs_refs` consistency scan.
      · treedb — `views` is an array of view-name STRINGS (`null` when the
        engine is unreachable), counted in `viewCount`; `note` states the
        inventory-only limit.
    `canDelete` is the server's own verdict; `blockReason` is its sentence when
    false (never paraphrased on the client). */
export interface StoragePreview {
  backend?: 'postgres' | 'treedb'
  instance: string
  uri?: string | null
  managed?: boolean
  control?: boolean
  serving?: boolean
  views?: PreviewViewRow[] | string[] | null
  placedViews?: string[]
  placedCount?: number | null
  viewCount?: number | null
  canDelete: boolean
  blockReason?: string | null
  refs?: { instances?: PreviewRefsRow[]; unverified?: string[]; error?: string }
  note?: string
  [k: string]: unknown
}

/** `storage.relocate` — one view's move. `move` mirrors the server's
    `PgMoveReport` (`relocate()` in `pg_store.rs`), `evicted` is how many
    cached engines THIS meta-server dropped so the next op re-opens on the
    target, and `note` states what a single server cannot see about mounts on
    OTHER machines. */
export interface RelocateResult {
  ok: boolean
  move: { view: string; nodes: number; blobs: number; from: string; to: string; sourceDropped: boolean }
  evicted: number
  note: string
}

/** `store.info` — the administrator's deployment snapshot. */
export interface StoreInfo {
  backend?: string
  /** Where the TREES live, which differs from `backend` (the control-plane
      backend) on a treedb deployment: control stays SQLite, trees sit in the
      engine. The base-directory label and layout note key off THIS. */
  metadataBackend?: string
  describe?: string
  base?: string
  role?: string
  views?: number
  definitions?: number
  accounts?: number | null
  liveKeys?: number | null
  expiredKeys?: number | null
  liveSessions?: number
  residentEngines?: number
  residentViews?: number
  /** `null` means rows are kept until an operator purges them by hand. */
  auditRetention?: string | null
  /** Backend + instance book + placement (Super only). */
  storage?: StorageInfo
  /** Registry instances with their placed-view counts; `null` on a non-PG backend. */
  pgInstances?: PgLedger | null
  /** The banner's own words on where trees live and what `base` holds here. */
  backendLayout?: string
}

/** One registry instance as `store.info` reports it; counts are `null` when
    the control plane could not be read. */
export interface PgLedgerRow {
  name: string
  views: number | null
  control: boolean
}

export interface PgLedger {
  instances: PgLedgerRow[]
  /** Trees with NO placement row — the hash-routed set (predate the table, or
      a single-instance spec that writes none). `null` when unreadable. */
  unplaced: number | null
  /** Placement rows naming an instance the registry no longer defines: a
      routing ERROR whose mount is refused, distinct from `unplaced`. */
  dangling: number | null
}

/** The API's own kind spelling; `derived` is what the console calls a lens. */
export type ViewKind = 'base' | 'fork' | 'alias' | 'derived' | 'other'

/** A merged row of `physical.list` + `def.list` — the list's cheap view. */
export interface ViewRow {
  name: string
  kind: ViewKind
  base: string | null
}

/** `view.stats` — the authority on one name, refined for the selected row. */
export interface ViewStats {
  name: string
  kind: ViewKind
  base: string | null
  definition?: string | null
  stamp?: Stamp
  tree?: boolean
  forkTree?: boolean
  family?: string[]
  keys?: number | null
  sessions?: number
  resident?: boolean
  /** Instance whose schema holds this view's tree; `null` = hash-routed
      (multi-instance) or not resolvable. */
  instance?: string | null
}

export type Stamp = string | Record<string, string | number> | null | undefined

/** One row of `apikey.list`. */
export interface KeyRow {
  id: string
  label: string
  scope: string
  view?: string | null
  ro?: boolean
  owner?: string | null
  created_by?: string | null
  note?: string | null
  secret_hint?: string | null
  /** Whether `Reveal` has a wrapped copy to open for this row. A flag only —
      the copy itself never travels in a list payload. */
  secret_available?: boolean
  created_at?: string | null
  expires_at?: string | null
  expired?: boolean
  revoked_at?: string | null
  last_used_at?: string | null
}

/** `apikey.create` / `apikey.rotate` — the only reply that carries a secret. */
export interface KeySecret {
  key: string
  id: string
  label: string
  scope: string
  owner?: string | null
  created_by?: string | null
  view?: string | null
  expires_at?: string | null
  note?: string | null
  /** Does the registry now hold a recoverable copy of `key`, or was this the
      only display it will ever give? The store answers, because wrapping
      material is a property of the deployment, not of the form. */
  stored?: boolean
}

/** `apikey.secret` — the read-back answer. `available` splits it: either the
    credential or the sentence saying why there is none, never both. */
export interface KeyReveal {
  id: string
  available: boolean
  key?: string | null
  why?: string | null
  label: string
  scope: string
  owner?: string | null
  view?: string | null
}

/** One row of `apikey.calls` — the governance audit trail. */
export interface AuditRow {
  ts: string
  method?: string
  tool?: string
  ok?: boolean
  detail?: string | null
  key_label?: string | null
  key_id?: string | null
}

/** One row of `account.list`. */
export interface AccountRow {
  account: string
  role: string
  created_at?: string | null
  last_login_at?: string | null
  disabled_at?: string | null
  bases?: string[]
}

/** One row of `grant.list` — an account's delegation of one base view. */
export interface GrantRow {
  account: string
  base_view: string
  perm?: string
  granted_at?: string | null
  granted_by?: string | null
}

/** `POST /api/import` — a dump's outcome. */
export interface ImportResult {
  nodes: number
  blobs: number
  kind: string
}

/** `apikey.create`'s `args`, and the two optional ones the CLI also has.
    A type alias rather than an interface on purpose: `api()` takes a
    `Record<string, unknown>` payload, and only an alias gets the implicit index
    signature that makes the two types line up without a cast. */
export type MintKeyArgs = {
  label: string
  scope: string
  ro: boolean
  view?: string
  note?: string
  ttl?: string
}

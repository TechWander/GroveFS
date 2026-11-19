import type { ImportResult } from '@/types'

/*
  The transport, and nothing else.

  Four endpoints carry the whole console (`web.rs`): POST /api/login,
  POST /api/logout, GET /api/session, POST /api/call — plus the two dump
  routes. Every request is same-origin with credentials, every mutating one
  carries the per-session `x-grovefs-csrf` header, and a non-JSON answer stays
  `null` here rather than becoming an invented object: the callers say what an
  unexpected HTTP status means, in the user's language.

  Nothing in this file decides permissions. `/api/session` returns the
  gateway's routed-op list, and the meta-server's `gates()` is the authority —
  a refusal is always the server's own sentence, kept verbatim.
*/

export interface Reply<T> {
  status: number
  j: T | null
}

export async function request<T>(path: string, init?: RequestInit): Promise<Reply<T>> {
  const r = await fetch(path, { credentials: 'same-origin', ...init })
  let j: T | null = null
  try {
    j = (await r.json()) as T
  } catch {
    /* an empty or non-JSON answer — `null` is the honest version of that */
  }
  return { status: r.status, j }
}

/** The dump download: a browser navigation, not a fetch, because the gateway
    answers it as a file. */
export function exportUrl(view: string, csrf: string | null | undefined): string {
  const q = new URLSearchParams({ view, csrf: csrf ?? '' })
  return `/api/export?${q.toString()}`
}

/** What `POST /api/import` answers: the usual envelope around a dump result. */
export interface ImportReply {
  ok?: boolean
  error?: string
  result?: ImportResult
}

export interface ImportOptions {
  file: File
  view: string
  csrf: string | null | undefined
  /** Absent means "become a BASE view", which only an administrator may do. */
  asFork?: string
  replace?: boolean
}

/** Upload a metadata dump. The verdict is the server's, including the refusal. */
export async function importDump(
  o: ImportOptions,
): Promise<{ status: number; j: ImportReply | null }> {
  const q = new URLSearchParams({ view: o.view, csrf: o.csrf ?? '' })
  if (o.asFork) q.set('as_fork', o.asFork)
  if (o.replace) q.set('replace', '1')
  const r = await fetch(`/api/import?${q.toString()}`, {
    method: 'POST',
    credentials: 'same-origin',
    headers: { 'content-type': 'application/octet-stream', 'x-grovefs-csrf': o.csrf ?? '' },
    body: o.file,
  })
  let j: ImportReply | null = null
  try {
    j = (await r.json()) as ImportReply
  } catch {
    /* a proxy error page */
  }
  return { status: r.status, j }
}

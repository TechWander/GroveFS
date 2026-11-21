import { t } from '@/i18n'
import type { DefEntry } from '@/api/ops'
import type { Stamp, ViewKind, ViewRow } from '@/types'

/*
  The namespace, read the cheap way.

  `physical.list` and `def.list` are two views of one namespace: a name can have
  a tree, a definition, or (a fork) both. `view.stats` is the authority on the
  kind, but it costs a round trip per name — this merge is the approximation the
  LIST uses, refined for the selected row.
*/

/** The API's kind spelling is the JSON's; the console's own noun for a
    `derived` view is a LENS (the chip row, the badges and the help text all say
    so), so the list and the tree say what the rest of the page says. */
export const KIND_LABEL: Record<string, string> = {
  base: 'base view',
  fork: 'fork',
  alias: 'alias',
  derived: 'lens',
  other: 'definition',
}

/** `ViewKind` is a union of strings, so the parameter is declared as the wider
    `string`: a server that starts answering a new kind should print that kind
    rather than crash the page. */
export function kindName(kind: string | null | undefined): string {
  const s = kind ?? ''
  return t(KIND_LABEL[s] || s || 'other')
}

function defFirst(text: string | null | undefined, kw: string): string | null {
  for (const line of String(text ?? '').split('\n')) {
    const ln = line.trim()
    if (ln.startsWith(`${kw} `) || ln.startsWith(`${kw}\t`)) return ln.split(/\s+/)[1] ?? null
  }
  return null
}

export function defKind(text: string | null | undefined): ViewKind {
  if (defFirst(text, 'alias')) return 'alias'
  if (defFirst(text, 'fork')) return 'fork'
  if (defFirst(text, 'base')) return 'derived'
  return 'other'
}

export function defBase(text: string | null | undefined): string | null {
  return defFirst(text, 'alias') ?? defFirst(text, 'fork') ?? defFirst(text, 'base')
}

/** `def.list` answers either `["name","text"]` pairs or `{name,text}` objects,
    depending on how the meta-server packed it; both have been in the wire. */
export function mergeViews(phys: string[], defs: DefEntry[]): ViewRow[] {
  const out = new Map<string, ViewRow>()
  for (const p of phys) out.set(p, { name: p, kind: 'base', base: p })
  for (const d of defs) {
    const [name, text] = Array.isArray(d) ? d : [d.name, d.text]
    out.set(name, { name, kind: defKind(text), base: defBase(text) })
  }
  return [...out.values()].sort((a, b) => a.name.localeCompare(b.name))
}

/** DIRECT children in the merged namespace: the parent reference written into
    the definition (`alias X`, `fork Y`, `base Z`). `view.stats.family` is the
    server's authoritative flat list of every name that RESOLVES through a base
    — this is the EDGE list, which is what turns that pile of names into the
    flat sibling list the detail pane shows. */
export function viewChildren(views: ViewRow[], name: string): ViewRow[] {
  return views.filter((v) => v.name !== name && v.base === name)
}

/** One name below the selected view, with the edge that reaches it. */
export interface Descendant {
  view: ViewRow
  /** The IMMEDIATE parent when that is not the selected view (so a lens on a
      lens says `via team1`); `null` for a direct child. This is an annotation,
      not a nesting level: every descendant is rendered as one sibling row. */
  via: string | null
}

/** Everything that derives from `name`, as a FLAT list of siblings.
    Breadth-first with a shared `seen` set, so a namespace where two definitions
    point at one name lists it once and a hand-written cycle cannot loop. */
export function viewDescendants(views: ViewRow[], name: string): Descendant[] {
  const out: Descendant[] = []
  const seen = new Set<string>([name])
  const queue: ViewRow[] = viewChildren(views, name)
  for (const v of queue) seen.add(v.name)
  while (queue.length) {
    const v = queue.shift() as ViewRow
    out.push({ view: v, via: v.base === name ? null : v.base })
    for (const child of viewChildren(views, v.name)) {
      if (seen.has(child.name)) continue
      seen.add(child.name)
      queue.push(child)
    }
  }
  return out
}

/** The chain ABOVE `name` (a lens on a fork on a base). Cycle-guarded, because
    two hand-written definitions can point at each other, and a console that
    spins on a data bug is worse than one that draws the tree shallowly. */
export function viewAncestry(views: ViewRow[], name: string): string[] {
  const chain: string[] = []
  const seen = new Set([name])
  let cur = name
  while (chain.length < 12) {
    const v = views.find((x) => x.name === cur)
    const parent = v ? v.base : null
    if (!parent || parent === cur || seen.has(parent)) break
    chain.push(parent)
    seen.add(parent)
    cur = parent
  }
  return chain
}

/** A stamp is a JSON object (`{seq, root, …}`) on the PG backend and a bare
    string on the file one; both are read as `k=v` pairs. */
export function stampText(st: Stamp): string {
  if (st === null || st === undefined) return '—'
  if (typeof st === 'object') {
    return Object.entries(st)
      .map(([k, v]) => `${k}=${v}`)
      .join('  ')
  }
  return String(st)
}

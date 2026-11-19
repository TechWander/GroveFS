import { TREE_DEPTH, viewChildren } from '@/utils/views'
import type { ViewKind, ViewRow } from '@/types'

/*
  The namespace as a tree, built once per render.

  `view.stats.family` already lists every name that resolves through a base, but
  as a flat pile — it cannot say which lens sits on which fork. The edges are in
  the definitions (`alias X`, `fork Y`, `base Z`), so the tree is rebuilt from the
  merged list: one level per derivation, which is the difference between a list
  of names and seeing what breaks if a base moves.

  Two guards, both from the old page:

  · `seen` is shared down the WHOLE walk, so a name reached by two edges (an
    alias and a lens over the same fork) is drawn once, not twice, and cannot
    loop forever;
  · the descent stops at `TREE_DEPTH` and says it did, rather than opening a
    thousand-lens namespace in one frame.
*/

export interface TreeNode {
  name: string
  kind: ViewKind
  kids: TreeNode[]
  /** True when children exist but the depth cap stopped drawing them. */
  truncated: boolean
}

export interface LineageTree {
  root: TreeNode
  /** How many distinct names the walk found below the root. */
  below: number
}

export function buildLineage(views: ViewRow[], name: string, kind: ViewKind): LineageTree {
  const seen = new Set<string>([name])
  const root = node(views, { name, kind, base: name }, seen, 1)
  return { root, below: seen.size - 1 }
}

function node(views: ViewRow[], v: ViewRow, seen: Set<string>, depth: number): TreeNode {
  const all = viewChildren(views, v.name)
  const kids = depth >= TREE_DEPTH ? [] : all.filter((c) => !seen.has(c.name))
  for (const k of kids) seen.add(k.name)
  return {
    name: v.name,
    kind: v.kind,
    kids: kids.map((k) => node(views, k, seen, depth + 1)),
    truncated: depth >= TREE_DEPTH && all.length > 0,
  }
}

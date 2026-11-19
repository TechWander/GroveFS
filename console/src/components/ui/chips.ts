/** One chip in a `ChipGroup`: the value it filters by, the dictionary key of
    its label, optionally how many rows that value currently matches, and
    optionally the colour family (a view kind) it is dotted with. */
export interface ChipItem {
  v: string
  t: string
  n?: number | null
  dot?: string
}

/** The view-kind chips, in the order the picker shows them. */
export const KIND_CHIPS: ChipItem[] = [
  { v: 'all', t: 'all' },
  { v: 'base', t: 'base views', dot: 'base' },
  { v: 'fork', t: 'forks', dot: 'fork' },
  { v: 'alias', t: 'aliases', dot: 'alias' },
  { v: 'derived', t: 'lenses', dot: 'derived' },
]

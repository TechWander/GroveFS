/**
  One table's column set, declared where the table is used.

  The hand-written console had exactly this idea (`keyCols()`), and the reason
  it kept it: one place says what a table's columns ARE, so the views page's
  "keys bound to this view" and the keys page cannot disagree about what a key
  looks like — the second one only drops the column that would repeat its own
  heading.
*/
export interface Column<T = unknown> {
  /** Stable id: what a cell slot switches on. */
  id: string
  /** Dictionary key of the header text (English doubles as the key). */
  label: string
  /** Extra classes on every `<td>` of this column (`mono`, `nowrap`, …). */
  cls?: string
  /** Plain columns name a field; rich ones are rendered by the `cell` slot. */
  field?: keyof T
}

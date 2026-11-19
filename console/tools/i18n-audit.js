/*
  Does every sentence the console can show have a Chinese entry?

  The dictionary's contract (see `src/i18n/index.ts`) is that a key missing from
  `zh.ts` degrades to its English instead of throwing — which is exactly why the
  omission is invisible in development and needs a check like this one: in
  Chinese mode an untranslated line is a UI bug nobody notices until an operator
  quotes it.

  A key reaches `t()` either directly (`t('no keys you can see')`) or through a
  prop this codebase defines as vocabulary (`:label="'state'"`, `:empty="…"`),
  because those components translate inside themselves. So the scan reads prop
  values as well as calls. It also compares against the dictionary the
  hand-written console shipped, which tells a dropped entry apart from a
  sentence this rewrite introduced.

  Run from `console/` with: node tools/i18n-audit.js [path/to/the/old/app.js]

  With no argument it reads the reference out of git: the newest revision of
  `src/webui/app.js` that still contains the hand-written table. That is what
  makes the check reproducible after the Vue build took that path over — the
  file on disk is a bundle by then, so the last version WITH a dictionary is
  found by walking the file's own history instead of by a hardcoded commit.
*/
import fs from 'node:fs'
import path from 'node:path'
import { execFileSync } from 'node:child_process'

/** The repo root, so the git pathspec below is read from the right directory
    (a pathspec is relative to the process cwd, not to the file being run). */
const ROOT = path.resolve(path.dirname(new URL(import.meta.url).pathname), '..', '..')

/** The old file's source, or null if there is no such history here (a tarball,
    a shallow clone). Without a reference the run still answers "did we lose a
    key?", just not "was that key one the old page had?". */
function legacySource() {
  if (process.argv[2]) return fs.readFileSync(process.argv[2], 'utf8')
  try {
    const revs = execFileSync('git', ['log', '--format=%H', '--', 'src/webui/app.js'], {
      cwd: ROOT,
      encoding: 'utf8',
    }).split('\n')
    for (const rev of revs.filter(Boolean).slice(0, 40)) {
      const src = execFileSync('git', ['show', `${rev}:src/webui/app.js`], {
        cwd: ROOT,
        encoding: 'utf8',
        maxBuffer: 32 << 20,
      })
      if (src.includes('const I18N_ZH = {')) return src
    }
  } catch {
    return null
  }
  return null
}

function walk(dir, acc = []) {
  for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
    const p = path.join(dir, e.name)
    if (e.isDirectory()) walk(p, acc)
    else acc.push(p)
  }
  return acc
}

const { ZH } = await import('../src/i18n/zh.ts')
const mine = new Set(Object.keys(ZH))

/** The legacy table, keyed by its own entries.
    Only the block between `const I18N_ZH = {` and its closing brace counts: a
    bare `^  word:` scan elsewhere in the file picks up object fields and SVG
    attributes (`stroke:`) and reports them as lost translations.
    A key is read as the SOURCE text of the entry, so `\n` in it is still two
    characters — `t()` looks the table up with a real newline, hence the decode
    below; without it a multi-line confirm body looks like a dropped entry. */
const LEGACY = legacySource()
const legacy = new Set()
if (LEGACY) {
  const all = LEGACY
  const start = all.indexOf('const I18N_ZH = {')
  const tail = start < 0 ? -1 : all.indexOf('\n};', start)
  const src = tail < 0 ? '' : all.slice(start, tail)
  const decode = (s) =>
    s.replace(/\\(['`\\nrt0xu])/g, (_, c) =>
      c === 'n' ? '\n' : c === 'r' ? '\r' : c === 't' ? '\t' : c,
    )
  for (const m of src.matchAll(/^\s{2}'((?:[^'\\]|\\.)+)':/gm)) legacy.add(decode(m[1]))
  for (const m of src.matchAll(/^\s{2}([A-Za-z_$][\w$]*):/gm)) legacy.add(m[1])
}

const files = walk('src').filter(
  (f) => /\.(vue|ts)$/.test(f) && !f.endsWith('env.d.ts') && !f.endsWith('zh.ts'),
)

/** Props this codebase passes as dictionary keys — the component that receives
    one is the place that calls `t()`. */
const VOCAB = 'label|title|text|empty|summary|desc|heading|confirm|cancel|top|sub'
/**
 * Strings that stay English in every language, on purpose: the API's own nouns
 * (a `mount` key is a `mount` key in the CLI, in the docs and in a ticket), and
 * the `placeholder` hints the hand-written page never ran through the
 * dictionary. Anything listed here is a decision, not a gap.
 */
const UNTRANSLATED = new Set(['mount', 'mcp', 'admin', 'any', 'base', 'fork', 'alias', 'lens'])
const hits = new Map()
const note = (s, f, how) => {
  if (!s || s.length > 600 || !/[A-Za-z]{3}/.test(s)) return
  if (/[\u3000-\u9fff]/.test(s)) return
  if (UNTRANSLATED.has(s)) return
  const prev = hits.get(s)
  if (prev) prev.how.add(how)
  else hits.set(s, { file: f, how: new Set([how]) })
}

for (const f of files) {
  const src = fs.readFileSync(f, 'utf8')
  for (const m of src.matchAll(/\bt\(\s*(['"`])((?:\\.|(?!\1)[\s\S])*?)\1/g)) note(m[2], f, 't()')
  // A bound vocabulary prop only carries keys inside ITS string literals:
  // `:label="row.a ? 'x' : 'y'"` has two, `:title="state.title"` has none.
  for (const m of src.matchAll(new RegExp(`:(?:${VOCAB})="([^"]*)"`, 'g')))
    for (const q of m[1].matchAll(/'([^']*)'/g)) note(q[1], f, 'prop')
  // A static one IS the key: `<PageHead title="Overview" …>`.
  // `\\s` and not `\s`: inside a string the latter is a plain `s`, so the
  // pattern would silently stop matching what its comment describes.
  for (const m of src.matchAll(new RegExp(`(?:^|\\s)(?:${VOCAB})="([^"']*)"`, 'g')))
    note(m[1], f, 'prop')
  // { t: 'key' } chips, `label: 'key'` columns, [ 'term', 'gloss' ] pairs
  for (const m of src.matchAll(/\bt:\s*'([^']+)'/g)) note(m[1], f, 'chip')
  for (const m of src.matchAll(/label:\s*'([^']+)'/g)) note(m[1], f, 'column')
  for (const m of src.matchAll(/\[\s*'([A-Z][^']*)'\s*,\s*'([^']{20,})'/g)) {
    note(m[1], f, 'gloss')
    note(m[2], f, 'gloss')
  }
}

const missing = [...hits].filter(([s]) => !mine.has(s))
const dropped = missing.filter(([s]) => legacy.has(s))
const fresh = missing.filter(([s]) => !legacy.has(s))
// The other direction: an entry the hand-written table had and `zh.ts` does not.
// Nothing here is necessarily a bug — a sentence the rewrite stopped printing
// should lose its entry — but every line has to be explained.
const lost = [...legacy].filter((s) => !mine.has(s))
console.log(`zh.ts entries:        ${mine.size}`)
console.log(`legacy entries:       ${legacy.size}`)
console.log(`strings sent to t():  ${hits.size}`)
console.log(`  of which missing:   ${missing.length}`)
console.log(`legacy entries zh.ts lacks: ${lost.length}`)
if (dropped.length) {
  console.log('\nIN LEGACY BUT NOT IN zh.ts — an entry this rewrite lost:')
  for (const [s, v] of dropped) console.log(`  ${s}   <- ${v.file}`)
}
if (fresh.length) {
  console.log('\nNEW SENTENCES — text this rewrite introduced, needing a translation:')
  for (const [s, v] of fresh)
    console.log(`  [${[...v.how].join(',')}] ${s.slice(0, 150)}\n        ${v.file}`)
}
if (!missing.length) console.log('\nno missing keys')
if (lost.length) {
  console.log('\nENTRIES zh.ts DOES NOT HAVE:')
  for (const s of lost) console.log(`  ${JSON.stringify(s).slice(0, 150)}`)
}

#!/usr/bin/env node
// Keeps FLOWS.md and the dynamic views from drifting apart.
//
// The failure this catches is quiet and certain: someone adds a view and not
// its index row, or renames a view and leaves the document pointing at a name
// that no longer exists. Neither breaks `likec4 validate`, and neither is
// visible until a reader follows a reference that goes nowhere.
//
// Three rules:
//   1. Every `dynamic view` in views/ appears in the FLOWS.md index.
//   2. Every view named by the index exists.
//   3. Every `**view** `name`` heading names a view that exists.
//
// A flow deliberately without a diagram writes `no view` (optionally with a
// reason) or `table below` in its View column — the absence is then a
// statement rather than an omission.
//
// Usage: node docs/architecture/check-flows.mjs   (exit 0 = clean, 1 = findings)

import { readFileSync, readdirSync, statSync } from 'node:fs'
import { join, dirname, relative } from 'node:path'
import { fileURLToPath } from 'node:url'

const here = dirname(fileURLToPath(import.meta.url))

function c4Files(dir) {
  const out = []
  for (const entry of readdirSync(dir)) {
    const full = join(dir, entry)
    if (statSync(full).isDirectory()) out.push(...c4Files(full))
    else if (entry.endsWith('.c4')) out.push(full)
  }
  return out
}

// --- what the model declares ------------------------------------------------

const declared = new Map() // view id -> file it is declared in
for (const file of c4Files(join(here, 'views'))) {
  const text = readFileSync(file, 'utf8')
  for (const m of text.matchAll(/^\s*dynamic view (\w+)\s*\{/gm)) {
    declared.set(m[1], relative(here, file))
  }
}

// --- what the document claims -----------------------------------------------

const doc = readFileSync(join(here, 'FLOWS.md'), 'utf8')

// Only the index table is a flow index. The fault-matrix tables further down
// are three-column tables of scenarios, and reading them as flow rows is how a
// naive parser turns a detection signal into a missing view.
const indexSection = doc.split(/^## /m).find((s) => s.startsWith('Index\n'))
if (!indexSection) {
  console.error('FLOWS.md has no "## Index" section')
  process.exit(2)
}

const indexed = new Map() // view id -> one flow row that names it (several may)
const rowsWithoutView = []
let indexRows = 0
for (const line of indexSection.split('\n')) {
  if (!line.startsWith('| ')) continue
  const cells = line.split('|').map((c) => c.trim())
  if (cells.length < 5) continue
  const [, flow, , view] = cells
  if (flow === 'Flow' || /^-+$/.test(flow)) continue
  indexRows++
  const named = view.match(/`(\w+)`/)
  if (named) indexed.set(named[1], flow)
  else rowsWithoutView.push({ flow, view })
}

const headed = new Set()
for (const m of doc.matchAll(/^\*\*view\*\* `(\w+)`/gm)) headed.add(m[1])

// --- the three rules --------------------------------------------------------

const findings = []

for (const [id, file] of declared) {
  if (!indexed.has(id)) findings.push(`view \`${id}\` (${file}) has no row in the FLOWS.md index`)
}
for (const [id, flow] of indexed) {
  if (!declared.has(id)) findings.push(`FLOWS.md index row "${flow}" names view \`${id}\`, which does not exist`)
}
for (const id of headed) {
  if (!declared.has(id)) findings.push(`a FLOWS.md section heading names view \`${id}\`, which does not exist`)
}
for (const { flow, view } of rowsWithoutView) {
  if (!/^no view\b/.test(view) && !/^table below$/.test(view)) {
    findings.push(`FLOWS.md row "${flow}" has an unreadable View cell: "${view}" — use a \`viewName\`, "no view", or "table below"`)
  }
}

if (findings.length) {
  for (const f of findings) console.error(f)
  console.error(`\n${findings.length} finding(s).`)
  process.exit(1)
}

console.log(
  `flows ok: ${indexRows} flows catalogued, ${declared.size} dynamic views (all indexed), ` +
    `${rowsWithoutView.length} deliberately without a view`,
)

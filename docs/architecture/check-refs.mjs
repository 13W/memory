#!/usr/bin/env node
// Checks the model's links back into the repository.
//
// Three rules, and each one exists because the failure it catches is silent:
//
//   1. Every element carries a `spec` reference. An element nobody can trace to
//      a normative section is a drawing, not a model.
//   2. Every path in `spec`, `plan` and `code` metadata resolves to a file that
//      exists. Renaming a module is exactly when a diagram starts lying, and
//      nothing else in the toolchain notices.
//   3. `spec` points into docs/specification or docs/adr, and `plan` into
//      docs/implementation-plan — a reference to the wrong corpus is a category
//      error rather than a typo.
//
// Deliberately NOT built on `likec4 export json`: that command shells out to a
// package manager and rewrites the lockfile when an AI provider key is present
// in the environment, which is not a side effect a read-only check may have.
// Plain text is enough, because the shape it reads is the shape README.md
// requires.
//
// Usage: node docs/architecture/check-refs.mjs   (exit 0 = clean, 1 = findings)

import { readFileSync, existsSync, readdirSync, statSync } from 'node:fs'
import { join, dirname, resolve, relative } from 'node:path'
import { fileURLToPath } from 'node:url'

const here = dirname(fileURLToPath(import.meta.url))

// Walk up to the workspace root rather than assuming a fixed depth, so moving
// this directory does not silently turn every reference into "does not exist".
function findRepoRoot(from) {
  let dir = from
  for (;;) {
    if (existsSync(join(dir, 'Cargo.toml')) && existsSync(join(dir, 'docs', 'specification'))) return dir
    const up = dirname(dir)
    if (up === dir) {
      console.error('cannot locate the workspace root above ' + from)
      process.exit(2)
    }
    dir = up
  }
}

const repoRoot = findRepoRoot(here)

const ELEMENT_KINDS = [
  'actor',
  'externalSystem',
  'system',
  'container',
  'component',
  'store',
  'queue',
]

const REF_KEYS = ['spec', 'plan', 'code']

const CORPUS = {
  spec: ['docs/specification/', 'docs/adr/'],
  plan: ['docs/implementation-plan/'],
  code: [],
}

function c4Files(dir) {
  const out = []
  for (const entry of readdirSync(dir)) {
    const full = join(dir, entry)
    if (statSync(full).isDirectory()) out.push(...c4Files(full))
    else if (entry.endsWith('.c4')) out.push(full)
  }
  return out.sort()
}

// The body of a `{ ... }` block starting at the brace at `open`, brace-balanced
// and quote-aware, so a `{` inside a description string does not end it early.
function blockBody(text, open) {
  let depth = 0
  let quote = null
  for (let i = open; i < text.length; i++) {
    const ch = text[i]
    if (quote) {
      if (text.startsWith(quote, i)) {
        i += quote.length - 1
        quote = null
      }
      continue
    }
    if (text.startsWith("'''", i)) {
      quote = "'''"
      i += 2
      continue
    }
    if (ch === "'") {
      quote = "'"
      continue
    }
    if (ch === '/' && text[i + 1] === '/') {
      const nl = text.indexOf('\n', i)
      i = nl === -1 ? text.length : nl
      continue
    }
    if (ch === '{') depth++
    else if (ch === '}') {
      depth--
      if (depth === 0) return text.slice(open + 1, i)
    }
  }
  return null
}

function lineOf(text, index) {
  return text.slice(0, index).split('\n').length
}

const findings = []
const seenRefs = new Set()

for (const file of c4Files(here)) {
  const text = readFileSync(file, 'utf8')
  const shown = relative(repoRoot, file)

  // Rule 1 — every element declaration carries a spec reference.
  const decl = new RegExp(
    `^\\s*([A-Za-z][\\w-]*)\\s*=\\s*(${ELEMENT_KINDS.join('|')})\\b[^{]*\\{`,
    'gm',
  )
  for (const m of text.matchAll(decl)) {
    const open = m.index + m[0].length - 1
    const body = blockBody(text, open)
    if (body === null) continue
    // Only this element's own metadata, not a nested child's.
    const nestedAt = body.search(decl.source ? new RegExp(decl.source, 'm') : /$^/)
    const own = nestedAt === -1 ? body : body.slice(0, nestedAt)
    if (!/\bspec\s+'/.test(own)) {
      findings.push(`${shown}:${lineOf(text, m.index)}  ${m[1]} (${m[2]}) has no spec reference`)
    }
  }

  // Rules 2 and 3 — every reference resolves, and into the right corpus.
  const ref = new RegExp(`\\b(${REF_KEYS.join('|')})\\s+'([^']+)'`, 'g')
  for (const m of text.matchAll(ref)) {
    const [, key, value] = m
    // A spec reference is "<path> §<section>"; the section is prose, the path is not.
    const path = value.split('§')[0].trim().replace(/[,;]$/, '')
    const at = `${shown}:${lineOf(text, m.index)}`
    if (!existsSync(join(repoRoot, path))) {
      findings.push(`${at}  ${key} '${path}' does not exist`)
      continue
    }
    const prefixes = CORPUS[key]
    if (prefixes.length && !prefixes.some((p) => path.startsWith(p))) {
      findings.push(`${at}  ${key} '${path}' is outside ${prefixes.join(' or ')}`)
    }
    seenRefs.add(`${key} ${path}`)
  }
}

// --- Rule 4: paths quoted in this directory's markdown resolve too ----------
//
// FLOWS.md names the module that executes each flow. Those are the same claim a
// `code` metadata value makes, and they rot the same way, so they are checked
// the same way. Only strings that look like repository paths are considered —
// a backtick around `retryable` is prose, not a reference.

const REPO_PREFIX = /^(crates|npm|plugin|docs|fixtures|spike)\//

function expandBraces(path) {
  // `crates/store/src/cache/{fts,validate}.rs` names two files.
  const m = path.match(/^(.*)\{([^}]+)\}(.*)$/)
  if (!m) return [path]
  return m[2].split(',').map((part) => `${m[1]}${part.trim()}${m[3]}`)
}

for (const entry of readdirSync(here)) {
  if (!entry.endsWith('.md')) continue
  const text = readFileSync(join(here, entry), 'utf8')
  for (const m of text.matchAll(/`([^`\n]+)`/g)) {
    const raw = m[1].trim()
    if (!REPO_PREFIX.test(raw) || /\s/.test(raw)) continue
    for (const path of expandBraces(raw)) {
      const clean = path.replace(/[.,;)]+$/, '')
      if (!existsSync(join(repoRoot, clean))) {
        findings.push(`${entry}:${lineOf(text, m.index)}  '${clean}' does not exist`)
      } else {
        seenRefs.add(`md ${clean}`)
      }
    }
  }
}

if (findings.length) {
  for (const f of findings) console.error(f)
  console.error(`\n${findings.length} finding(s).`)
  process.exit(1)
}

console.log(`references ok: ${seenRefs.size} distinct targets across .c4 metadata and markdown, all resolve`)

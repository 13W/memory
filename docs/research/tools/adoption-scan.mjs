#!/usr/bin/env node
// adoption-scan.mjs — read-only retrospective measurement of memory-tool adoption
// in Claude Code transcripts.
//
// Answers one question with numbers instead of anecdote: when an agent had this
// product's memory tools available, did it actually use them — and how does that
// change across a compaction boundary, inside a subagent, and under deferred
// tool loading.
//
// Reads ~/.claude/projects/<slug>/*.jsonl (main sessions) and
// <slug>/<session-uuid>/subagents/agent-*.jsonl (+ .meta.json). Never writes
// anything except the report file it is told to write.
//
// PRIVACY: the emitted report contains counts, timestamps and session UUIDs only.
// No prompt text, no tool arguments, no file paths, no repository names other
// than this repository's own. Foreign projects are aliased project-NN.
//
// Usage:
//   node docs/research/tools/adoption-scan.mjs --out report.json [--root DIR]
//        [--since 2026-07-01] [--project SUBSTR] [--session UUID] [--quiet]

import { readdirSync, statSync, existsSync, readFileSync, writeFileSync, createReadStream } from 'node:fs';
import { join } from 'node:path';
import { homedir } from 'node:os';
import { createInterface } from 'node:readline';

const SCRIPT_VERSION = '1.1.0';

// The day D-041 and every task of group 19 landed (24e48a4..3325854, all 2026-08-07).
const EPOCH_BOUNDARY = '2026-08-08T00:00:00.000Z';

// ---------------------------------------------------------------- tool classes

// The memory MCP server is named by the plugin, so match on the server segment
// rather than on a hard-coded prefix: mcp__<server>__<tool>.
const MCP_RE = /^mcp__([^_].*?)__(.+)$/;
const MEMORY_SERVER_RE = /memory/i;

// The three tools the working loop names, plus the rest of the read surface.
const LOOP_TOOLS = new Set(['recall', 'search_code', 'remember']);
const MEMORY_READ_TOOLS = new Set(['recall', 'search_code', 'get_file_context', 'project_overview']);

// Built-in tools the trailer offers to replace.
const FILE_TOOLS = new Set(['Read', 'Grep', 'Glob', 'LS', 'NotebookRead']);

// Bash invocations that are functionally a read or a search. Claude Code's
// "auto mode" actively steers the model to these instead of Read/Grep, which
// makes them invisible both to the trailer's wording and to the spool's
// path extraction (Bash yields no paths by design).
const BASH_READ_RE = /(^|[|;&(\s])(rg|grep|egrep|fgrep|cat|head|tail|less|sed\s+-n|fd|find)\b/;

// Agent types are freely named by whoever spawned them, and in practice they carry
// ticket ids and project nouns. Only the harness's own built-ins survive into the
// report; everything else collapses to "custom", which is all the analysis needs.
const BUILTIN_AGENTS = new Set(['Explore', 'Plan', 'general-purpose', 'fork', 'claude', 'claude-code-guide', 'statusline-setup']);
function normalizeAgentType(t) {
  if (!t) return null;
  return BUILTIN_AGENTS.has(t) ? t : 'custom';
}

// Same reasoning for tool names: a foreign MCP server's name says which vendor tools
// the user has connected. Memory tools stay verbatim because they are the subject.
function reportableToolName(name) {
  const m = MCP_RE.exec(name);
  if (!m) return name;
  return MEMORY_SERVER_RE.test(m[1]) ? name : 'mcp__other';
}

function classifyTool(name) {
  const m = MCP_RE.exec(name);
  if (m && MEMORY_SERVER_RE.test(m[1])) {
    return { group: 'memory', tool: m[2] };
  }
  if (m) return { group: 'mcp_other', tool: name };
  if (FILE_TOOLS.has(name)) return { group: 'file', tool: name };
  if (name === 'Bash') return { group: 'bash', tool: 'Bash' };
  if (name === 'Agent' || name === 'Task') return { group: 'agent', tool: name };
  if (name === 'ToolSearch') return { group: 'toolsearch', tool: name };
  return { group: 'other', tool: name };
}

// ---------------------------------------------------------------- transcript IO

async function* readRecords(file) {
  const rl = createInterface({ input: createReadStream(file, { encoding: 'utf8' }), crlfDelay: Infinity });
  for await (const line of rl) {
    if (!line || line[0] !== '{') continue;
    try { yield JSON.parse(line); } catch { /* a truncated tail line is not fatal */ }
  }
}

function contentBlocks(rec) {
  const c = rec?.message?.content;
  return Array.isArray(c) ? c : [];
}

function isUserPrompt(rec) {
  if (rec.type !== 'user') return false;
  if (rec.isMeta || rec.isCompactSummary || rec.isVisibleInTranscriptOnly) return false;
  const c = rec.message?.content;
  if (typeof c === 'string') return true;
  if (!Array.isArray(c)) return false;
  if (c.some((b) => b?.type === 'tool_result')) return false;
  return c.some((b) => b?.type === 'text');
}

// ---------------------------------------------------------------- analysis core

async function analyzeTranscript(file) {
  const seenUuid = new Set();          // resumed sessions re-serialize prior entries
  const usageByRequest = new Map();    // one API response = N records sharing requestId
  const events = [];                   // ordered stream of the things we measure
  const toolCounts = new Map();
  const injections = new Map();
  const compactions = [];
  const versions = new Set();
  const entrypoints = new Set();
  const models = new Set();

  let sessionId = null;
  let firstTs = null;
  let lastTs = null;
  let turns = 0;
  let slashCommands = 0;
  let memoryBlockInjections = 0;
  let deferredSignals = 0;
  let toolSearchForMemory = 0;

  for await (const rec of readRecords(file)) {
    if (rec.uuid) {
      if (seenUuid.has(rec.uuid)) continue;
      seenUuid.add(rec.uuid);
    }
    if (rec.sessionId && !sessionId) sessionId = rec.sessionId;
    if (rec.version) versions.add(rec.version);
    if (rec.entrypoint) entrypoints.add(rec.entrypoint);
    if (rec.timestamp) {
      if (!firstTs || rec.timestamp < firstTs) firstTs = rec.timestamp;
      if (!lastTs || rec.timestamp > lastTs) lastTs = rec.timestamp;
    }

    if (rec.type === 'assistant') {
      if (rec.message?.model) models.add(rec.message.model);
      if (rec.requestId && rec.message?.usage && !usageByRequest.has(rec.requestId)) {
        const u = rec.message.usage;
        usageByRequest.set(rec.requestId, {
          input: u.input_tokens || 0,
          output: u.output_tokens || 0,
          cache_read: u.cache_read_input_tokens || 0,
          cache_creation: u.cache_creation_input_tokens || 0,
        });
      }
      for (const b of contentBlocks(rec)) {
        if (b?.type !== 'tool_use') continue;
        const cls = classifyTool(b.name);
        const reportable = reportableToolName(b.name);
        toolCounts.set(reportable, (toolCounts.get(reportable) || 0) + 1);

        let kind = cls.group;
        if (cls.group === 'bash') {
          const cmd = typeof b.input?.command === 'string' ? b.input.command : '';
          kind = BASH_READ_RE.test(cmd) ? 'bash_read' : 'bash_other';
        }
        if (cls.group === 'toolsearch') {
          const q = JSON.stringify(b.input || {});
          if (MEMORY_SERVER_RE.test(q)) toolSearchForMemory += 1;
        }
        events.push({ ts: rec.timestamp, kind, tool: cls.tool, name: b.name });
      }
      continue;
    }

    if (rec.type === 'system') {
      if (rec.subtype === 'compact_boundary') {
        const m = rec.compactMetadata || {};
        compactions.push({
          ts: rec.timestamp,
          trigger: m.trigger || null,
          pre_tokens: m.preTokens ?? null,
          post_tokens: m.postTokens ?? null,
          duration_ms: m.durationMs ?? null,
        });
        events.push({ ts: rec.timestamp, kind: 'compact', tool: null, name: 'compact_boundary' });
      }
      continue;
    }

    if (rec.type === 'attachment') {
      const a = rec.attachment || {};
      if (a.type === 'hook_additional_context') {
        const key = a.hookName || a.hookEvent || 'unknown';
        injections.set(key, (injections.get(key) || 0) + 1);
        const text = Array.isArray(a.content) ? a.content.join('\n') : String(a.content ?? '');
        if (text.includes('<memory v=')) memoryBlockInjections += 1;
        events.push({ ts: rec.timestamp, kind: 'inject', tool: null, name: key });
      } else if (a.type === 'hook_success') {
        const key = a.hookName || a.hookEvent || 'unknown';
        injections.set(key, (injections.get(key) || 0) + 1);
        const text = String(a.stdout ?? '');
        if (text.includes('<memory v=')) memoryBlockInjections += 1;
        events.push({ ts: rec.timestamp, kind: 'inject', tool: null, name: key });
      } else if (a.type === 'deferred_tools_delta' || a.type === 'mcp_instructions_delta') {
        deferredSignals += 1;
      }
      continue;
    }

    if (isUserPrompt(rec)) {
      const c = rec.message.content;
      const text = typeof c === 'string' ? c : c.map((b) => b.text || '').join('');
      if (text.startsWith('<command-name>')) slashCommands += 1;
      else turns += 1;
      events.push({ ts: rec.timestamp, kind: 'turn', tool: null, name: 'user' });
    }
  }

  const usage = { input: 0, output: 0, cache_read: 0, cache_creation: 0, requests: usageByRequest.size };
  for (const u of usageByRequest.values()) {
    usage.input += u.input; usage.output += u.output;
    usage.cache_read += u.cache_read; usage.cache_creation += u.cache_creation;
  }

  return {
    session_id: sessionId,
    first_ts: firstTs,
    last_ts: lastTs,
    duration_h: firstTs && lastTs ? +(((Date.parse(lastTs) - Date.parse(firstTs)) / 3.6e6).toFixed(2)) : null,
    versions: [...versions].sort(),
    entrypoints: [...entrypoints].sort(),
    models: [...models].sort(),
    turns,
    slash_commands: slashCommands,
    compactions,
    injections: Object.fromEntries([...injections].sort()),
    memory_block_injections: memoryBlockInjections,
    deferred_signals: deferredSignals,
    tool_search_for_memory: toolSearchForMemory,
    tool_counts: Object.fromEntries([...toolCounts].sort((a, b) => b[1] - a[1])),
    usage,
    events,
  };
}

// A segment is the stretch of a session between two compaction boundaries.
// Segment 0 is the session as it started; segment N is what the model saw after
// the Nth compaction, which is the interesting one.
function segmentize(events) {
  const segments = [];
  let cur = newSegment(0, null);
  for (const e of events) {
    if (e.kind === 'compact') {
      segments.push(cur);
      cur = newSegment(segments.length, e.ts);
      continue;
    }
    countEvent(cur, e);
  }
  segments.push(cur);
  return segments.map(finishSegment);
}

function newSegment(index, startedAt) {
  return {
    index,
    started_at: startedAt,
    turns: 0,
    memory_calls: { recall: 0, search_code: 0, remember: 0, other: 0 },
    file_actions: 0,
    bash_reads: 0,
    agent_spawns: 0,
    injections: 0,
    _firstMemoryRead: null,
    _firstLoopCall: null,
    _firstFileAction: null,
    _firstBuiltinRead: null,
    _firstReadChannel: null,
    _actionIndex: 0,
  };
}

function countEvent(seg, e) {
  if (e.kind === 'turn') { seg.turns += 1; return; }
  if (e.kind === 'inject') { seg.injections += 1; return; }
  if (e.kind === 'agent') { seg.agent_spawns += 1; return; }

  const isFileAction = e.kind === 'file' || e.kind === 'bash_read';
  const isMemory = e.kind === 'memory';
  if (!isFileAction && !isMemory) return;

  seg._actionIndex += 1;
  const at = seg._actionIndex;

  if (isMemory) {
    const t = e.tool;
    if (t in seg.memory_calls) seg.memory_calls[t] += 1;
    else seg.memory_calls.other += 1;
    if (MEMORY_READ_TOOLS.has(t) && seg._firstMemoryRead === null) seg._firstMemoryRead = at;
    if (LOOP_TOOLS.has(t) && seg._firstLoopCall === null) seg._firstLoopCall = at;
    return;
  }

  if (e.kind === 'file') {
    seg.file_actions += 1;
    if (seg._firstBuiltinRead === null) seg._firstBuiltinRead = at;
  } else {
    seg.bash_reads += 1;
  }
  if (seg._firstFileAction === null) {
    seg._firstFileAction = at;
    seg._firstReadChannel = e.kind === 'file' ? 'builtin' : 'bash';
  }
}

function finishSegment(seg) {
  const reads = seg.file_actions + seg.bash_reads;
  const memoryTotal = Object.values(seg.memory_calls).reduce((a, b) => a + b, 0);
  // The rule under test, stated exactly: a memory read before the first file
  // read/grep/search of this stretch. Null = the stretch never read anything,
  // so the rule had no occasion to apply.
  let adherent = null;
  if (reads > 0) {
    adherent = seg._firstMemoryRead !== null && seg._firstMemoryRead < seg._firstFileAction;
  }
  // The trailer literally names "your first file read, grep, or search". Measured
  // twice on purpose: once against any read-shaped action including a Bash cat/rg,
  // and once against only the built-in tools the trailer offers to replace, so the
  // headline number cannot be an artefact of where the regex draws the line.
  let adherentBuiltin = null;
  if (seg.file_actions > 0) {
    adherentBuiltin = seg._firstMemoryRead !== null && seg._firstMemoryRead < seg._firstBuiltinRead;
  }
  return {
    index: seg.index,
    started_at: seg.started_at,
    turns: seg.turns,
    injections: seg.injections,
    memory_calls: seg.memory_calls,
    memory_calls_total: memoryTotal,
    file_actions: seg.file_actions,
    bash_reads: seg.bash_reads,
    reads_total: reads,
    agent_spawns: seg.agent_spawns,
    first_memory_read_at: seg._firstMemoryRead,
    first_file_action_at: seg._firstFileAction,
    first_read_channel: seg._firstReadChannel,
    recall_before_first_read: adherent,
    recall_before_first_builtin_read: adherentBuiltin,
  };
}

// ---------------------------------------------------------------- discovery

function listProjects(root) {
  return readdirSync(root, { withFileTypes: true })
    .filter((d) => d.isDirectory())
    .map((d) => d.name)
    .sort();
}

function listSessions(projectDir) {
  return readdirSync(projectDir, { withFileTypes: true })
    .filter((d) => d.isFile() && d.name.endsWith('.jsonl'))
    .map((d) => d.name)
    .sort();
}

function listSubagents(projectDir, sessionUuid) {
  const dir = join(projectDir, sessionUuid, 'subagents');
  if (!existsSync(dir)) return [];
  return readdirSync(dir, { withFileTypes: true })
    .filter((d) => d.isFile() && d.name.endsWith('.jsonl'))
    .map((d) => {
      const metaPath = join(dir, d.name.replace(/\.jsonl$/, '.meta.json'));
      let meta = {};
      if (existsSync(metaPath)) {
        try { meta = JSON.parse(readFileSync(metaPath, 'utf8')); } catch { /* keep going */ }
      }
      return { file: join(dir, d.name), meta };
    })
    .sort((a, b) => a.file.localeCompare(b.file));
}

// This repository's own project dirs keep their names; everything else is
// aliased so the committed report never carries a foreign repository name.
function aliasFor(slug, counter) {
  if (slug.includes('local-rag-v2')) return slug.replace(/^-opt-soft-/, '');
  if (!counter.map.has(slug)) counter.map.set(slug, `project-${String(++counter.n).padStart(2, '0')}`);
  return counter.map.get(slug);
}

// ---------------------------------------------------------------- main

function parseArgs(argv) {
  const args = { root: join(homedir(), '.claude', 'projects'), out: null, since: null, project: null, session: null, quiet: false };
  for (let i = 2; i < argv.length; i++) {
    const a = argv[i];
    if (a === '--quiet') args.quiet = true;
    else if (a.startsWith('--')) args[a.slice(2)] = argv[++i];
  }
  return args;
}

const args = parseArgs(process.argv);
const counter = { map: new Map(), n: 0 };
const sessions = [];
const projects = listProjects(args.root).filter((p) => !args.project || p.includes(args.project));

for (const slug of projects) {
  const projectDir = join(args.root, slug);
  const alias = aliasFor(slug, counter);
  for (const name of listSessions(projectDir)) {
    const uuid = name.replace(/\.jsonl$/, '');
    if (args.session && uuid !== args.session) continue;
    const file = join(projectDir, name);
    const size = statSync(file).size;
    if (size === 0) continue;

    const t = await analyzeTranscript(file);
    if (args.since && t.first_ts && t.first_ts < args.since) continue;

    const segments = segmentize(t.events);
    const subs = [];
    for (const s of listSubagents(projectDir, uuid)) {
      const st = await analyzeTranscript(s.file);
      const ssegs = segmentize(st.events);
      const total = ssegs.reduce((acc, x) => ({
        memory: acc.memory + x.memory_calls_total,
        reads: acc.reads + x.reads_total,
      }), { memory: 0, reads: 0 });
      subs.push({
        agent_type: normalizeAgentType(s.meta.agentType),
        spawn_depth: s.meta.spawnDepth ?? null,
        turns: st.turns,
        memory_calls: total.memory,
        reads: total.reads,
        tool_calls: Object.values(st.tool_counts).reduce((a, b) => a + b, 0),
        usage: st.usage,
        injections: st.injections,
      });
    }

    delete t.events;
    sessions.push({
      project: alias,
      session_uuid: uuid,
      bytes: size,
      ...t,
      segments,
      subagents: subs,
    });
    if (!args.quiet) process.stderr.write(`. ${alias}/${uuid.slice(0, 8)} seg=${segments.length} sub=${subs.length}\n`);
  }
}

// ---------------------------------------------------------------- aggregation

function rate(num, den) { return den === 0 ? null : +(num / den).toFixed(4); }

// Wilson score interval — a rate over 12 sessions deserves an honest width.
function wilson(k, n, z = 1.96) {
  if (n === 0) return null;
  const p = k / n;
  const d = 1 + (z * z) / n;
  const c = p + (z * z) / (2 * n);
  const s = z * Math.sqrt((p * (1 - p)) / n + (z * z) / (4 * n * n));
  return [+((c - s) / d).toFixed(4), +((c + s) / d).toFixed(4)];
}

function adherenceOver(segs) {
  let yes = 0, no = 0, na = 0;
  for (const s of segs) {
    if (s.recall_before_first_read === true) yes += 1;
    else if (s.recall_before_first_read === false) no += 1;
    else na += 1;
  }
  const n = yes + no;
  // The strict rule is "before the first read". Report the weaker one too —
  // "did a memory tool get called at all in this stretch" — so a zero on the
  // strict metric cannot be mistaken for a zero on usage.
  const withAny = segs.filter((s) => s.memory_calls_total > 0).length;
  let byes = 0, bno = 0;
  for (const s of segs) {
    if (s.recall_before_first_builtin_read === true) byes += 1;
    else if (s.recall_before_first_builtin_read === false) bno += 1;
  }
  const firstBash = segs.filter((s) => s.first_read_channel === 'bash').length;
  return {
    builtin_only: { applicable: byes + bno, adherent: byes, rate: rate(byes, byes + bno), ci95: wilson(byes, byes + bno) },
    first_read_via_bash: firstBash,
    applicable: n,
    adherent: yes,
    non_adherent: no,
    not_applicable: na,
    rate: rate(yes, n),
    ci95: wilson(yes, n),
    stretches: segs.length,
    with_any_memory_call: withAny,
    any_call_rate: rate(withAny, segs.length),
    any_call_ci95: wilson(withAny, segs.length),
  };
}

// A session only counts as "equipped" if the plugin actually reached it: the
// hook injected at least one memory block, or a memory tool was actually called.
const equipped = sessions.filter((s) => s.memory_block_injections > 0 || s.segments.some((x) => x.memory_calls_total > 0));

const firstSegments = equipped.map((s) => s.segments[0]).filter(Boolean);
const postCompactSegments = equipped.flatMap((s) => s.segments.slice(1));
const allSubagents = equipped.flatMap((s) => s.subagents);
const subagentsThatRead = allSubagents.filter((s) => s.reads > 0);

const report = {
  schema_version: '1.0',
  script_version: SCRIPT_VERSION,
  generated_at: new Date().toISOString(),
  root: args.root.replace(homedir(), '~'),
  filters: { since: args.since, project: args.project, session: args.session },
  privacy_note: 'Counts, timestamps and session UUIDs only. Foreign project names are aliased project-NN; the alias map is not emitted.',
  totals: {
    projects_scanned: projects.length,
    sessions_scanned: sessions.length,
    sessions_equipped: equipped.length,
    subagent_transcripts: sessions.reduce((a, s) => a + s.subagents.length, 0),
    compactions: sessions.reduce((a, s) => a + s.compactions.length, 0),
  },
  adherence: {
    definition: 'Within a stretch of session (start, or one compaction boundary to the next), a memory read (recall/search_code/get_file_context/project_overview) issued before the first Read/Grep/Glob or read-shaped Bash command. Null when the stretch read nothing.',
    first_segment: adherenceOver(firstSegments),
    post_compaction_segments: adherenceOver(postCompactSegments),
    subagents: {
      total: allSubagents.length,
      that_read_anything: subagentsThatRead.length,
      that_used_memory: subagentsThatRead.filter((s) => s.memory_calls > 0).length,
      rate: rate(subagentsThatRead.filter((s) => s.memory_calls > 0).length, subagentsThatRead.length),
      ci95: wilson(subagentsThatRead.filter((s) => s.memory_calls > 0).length, subagentsThatRead.length),
      by_agent_type: Object.fromEntries(
        [...new Set(allSubagents.map((s) => s.agent_type || 'unknown'))].sort().map((t) => {
          const g = allSubagents.filter((s) => (s.agent_type || 'unknown') === t);
          const r = g.filter((s) => s.reads > 0);
          return [t, { total: g.length, read_anything: r.length, used_memory: r.filter((s) => s.memory_calls > 0).length }];
        })
      ),
    },
  },
  volume: {
    memory_calls: equipped.reduce((a, s) => a + s.segments.reduce((b, x) => b + x.memory_calls_total, 0), 0),
    reads_builtin: equipped.reduce((a, s) => a + s.segments.reduce((b, x) => b + x.file_actions, 0), 0),
    reads_via_bash: equipped.reduce((a, s) => a + s.segments.reduce((b, x) => b + x.bash_reads, 0), 0),
    memory_block_injections: equipped.reduce((a, s) => a + s.memory_block_injections, 0),
  },
  cuts: {
    note: 'Every cut is over equipped sessions only. EPOCH_BOUNDARY is the day D-041 and all of group 19 landed (commits 24e48a4..3325854, all 2026-08-07), so "after" is the first full day on which every one of the six adoption channels was shipped.',
    epoch_boundary: EPOCH_BOUNDARY,
    by_epoch: {
      before: adherenceOver(equipped.filter((s) => s.first_ts && s.first_ts < EPOCH_BOUNDARY).map((s) => s.segments[0]).filter(Boolean)),
      after: adherenceOver(equipped.filter((s) => s.first_ts && s.first_ts >= EPOCH_BOUNDARY).map((s) => s.segments[0]).filter(Boolean)),
    },
    by_deferral: {
      deferred: adherenceOver(equipped.filter((s) => s.deferred_signals > 0 || s.tool_search_for_memory > 0).map((s) => s.segments[0]).filter(Boolean)),
      inline: adherenceOver(equipped.filter((s) => s.deferred_signals === 0 && s.tool_search_for_memory === 0).map((s) => s.segments[0]).filter(Boolean)),
    },
    // Was the block actually delivered after a compaction? Delivery and behaviour
    // are measured separately on purpose: this is what separates "the rule never
    // arrived" from "the rule arrived and did not fire".
    post_compaction_delivery: {
      sessions_with_compaction: equipped.filter((s) => s.compactions.length > 0).length,
      sessions_with_sessionstart_compact_injection: equipped.filter((s) => Object.keys(s.injections).some((k) => k.includes('compact'))).length,
      total_sessionstart_compact_injections: equipped.reduce((a, s) => a + Object.entries(s.injections).filter(([k]) => k.includes('compact')).reduce((b, [, v]) => b + v, 0), 0),
    },
    read_channel_share: {
      builtin: equipped.reduce((a, s) => a + s.segments.reduce((b, x) => b + x.file_actions, 0), 0),
      via_bash: equipped.reduce((a, s) => a + s.segments.reduce((b, x) => b + x.bash_reads, 0), 0),
      note: 'The trailer names "your first file read, grep, or search"; a read-shaped Bash command is neither, and the spool extracts no path from it either (PATH_LIKE_KEYS covers file_path/path/notebook_path only).',
    },
  },
  sessions,
};

const out = args.out || 'adoption-scan.json';
writeFileSync(out, JSON.stringify(report, null, 2));

if (!args.quiet) {
  const a = report.adherence;
  process.stderr.write('\n');
  process.stderr.write(`sessions scanned/equipped : ${report.totals.sessions_scanned} / ${report.totals.sessions_equipped}\n`);
  process.stderr.write(`compactions observed      : ${report.totals.compactions}\n`);
  process.stderr.write(`adherence, first stretch  : ${a.first_segment.adherent}/${a.first_segment.applicable} = ${a.first_segment.rate}\n`);
  process.stderr.write(`adherence, post-compaction: ${a.post_compaction_segments.adherent}/${a.post_compaction_segments.applicable} = ${a.post_compaction_segments.rate}\n`);
  process.stderr.write(`subagents using memory    : ${a.subagents.that_used_memory}/${a.subagents.that_read_anything} = ${a.subagents.rate}\n`);
  process.stderr.write(`reads: builtin ${report.volume.reads_builtin} vs via bash ${report.volume.reads_via_bash}\n`);
  process.stderr.write(`written: ${out}\n`);
}

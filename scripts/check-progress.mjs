#!/usr/bin/env node
/**
 * Validates state/progress.json.
 *
 * Zero dependencies on purpose: this must run on a bare Node install, in CI, and
 * inside any agent's sandbox without a prior `npm install`.
 *
 * It checks two things:
 *   1. Structure — the file matches state/progress.schema.json (the subset of
 *      JSON Schema this project actually uses).
 *   2. Project rules — the semantic gates from docs/agent/HARNESS.md that a schema
 *      cannot express. These are the ones that actually keep the file honest.
 *
 * Exit codes: 0 ok (warnings allowed), 1 errors found, 2 could not read a file.
 *
 * Usage:
 *   node scripts/check-progress.mjs            # validate
 *   node scripts/check-progress.mjs --strict   # warnings are errors too
 */

import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const PROGRESS = resolve(ROOT, "state/progress.json");
const SCHEMA = resolve(ROOT, "state/progress.schema.json");
const STRICT = process.argv.includes("--strict");

const errors = [];
const warnings = [];
const err = (path, msg) => errors.push(`${path}: ${msg}`);
const warn = (path, msg) => warnings.push(`${path}: ${msg}`);

// ---------------------------------------------------------------- load

let data, schema;
try {
  data = JSON.parse(readFileSync(PROGRESS, "utf8"));
} catch (e) {
  console.error(`FATAL: cannot read or parse state/progress.json — ${e.message}`);
  process.exit(2);
}
try {
  schema = JSON.parse(readFileSync(SCHEMA, "utf8"));
} catch (e) {
  console.error(`FATAL: cannot read or parse state/progress.schema.json — ${e.message}`);
  process.exit(2);
}

// ------------------------------------------------- minimal schema validator
// Supports the keywords this schema uses. Deliberately small, not general.

const deref = (node) => {
  if (node && typeof node === "object" && typeof node.$ref === "string") {
    const parts = node.$ref.replace(/^#\//, "").split("/");
    let cur = schema;
    for (const p of parts) cur = cur?.[p.replace(/~1/g, "/").replace(/~0/g, "~")];
    if (!cur) throw new Error(`unresolvable $ref ${node.$ref}`);
    return deref(cur);
  }
  return node;
};

const typeOf = (v) =>
  v === null ? "null" : Array.isArray(v) ? "array" : typeof v === "number"
    ? (Number.isInteger(v) ? "integer" : "number")
    : typeof v;

const typeMatches = (want, actual) =>
  want === "number" ? actual === "number" || actual === "integer" : want === actual;

function validate(value, node, path) {
  node = deref(node);
  if (!node || typeof node !== "object") return;

  if (node.type !== undefined) {
    const wanted = Array.isArray(node.type) ? node.type : [node.type];
    const actual = typeOf(value);
    if (!wanted.some((w) => typeMatches(w, actual))) {
      err(path, `expected type ${wanted.join("|")}, got ${actual}`);
      return; // further checks would be noise
    }
  }
  if (node.const !== undefined && value !== node.const) {
    err(path, `must equal ${JSON.stringify(node.const)}`);
  }
  if (node.enum && !node.enum.includes(value)) {
    err(path, `must be one of ${node.enum.join(", ")} (got ${JSON.stringify(value)})`);
  }
  if (typeof value === "string") {
    if (node.pattern && !new RegExp(node.pattern).test(value)) {
      err(path, `does not match /${node.pattern}/ (got "${value}")`);
    }
    if (node.minLength !== undefined && value.length < node.minLength) {
      err(path, `shorter than minLength ${node.minLength}`);
    }
    if (node.format === "date-time" && Number.isNaN(Date.parse(value))) {
      err(path, `not a valid ISO 8601 date-time: "${value}"`);
    }
  }
  if (typeof value === "number") {
    if (node.minimum !== undefined && value < node.minimum) err(path, `below minimum ${node.minimum}`);
    if (node.maximum !== undefined && value > node.maximum) err(path, `above maximum ${node.maximum}`);
  }
  if (Array.isArray(value)) {
    if (node.minItems !== undefined && value.length < node.minItems) {
      err(path, `has ${value.length} items, minItems is ${node.minItems}`);
    }
    if (node.maxItems !== undefined && value.length > node.maxItems) {
      err(path, `has ${value.length} items, maxItems is ${node.maxItems}`);
    }
    if (node.items) value.forEach((v, i) => validate(v, node.items, `${path}[${i}]`));
  }
  if (value && typeof value === "object" && !Array.isArray(value)) {
    for (const req of node.required ?? []) {
      if (!(req in value)) err(path, `missing required property "${req}"`);
    }
    for (const [k, v] of Object.entries(value)) {
      const sub = node.properties?.[k];
      if (sub) validate(v, sub, `${path}.${k}`);
      else if (node.additionalProperties === false && k !== "$schema") {
        err(path, `unexpected property "${k}"`);
      }
    }
  }
}

validate(data, schema, "$");

// -------------------------------------------------------- project rules
// These encode docs/agent/HARNESS.md. A schema cannot express them, and they are
// the ones that stop the file drifting away from reality.

const stages = Array.isArray(data.stages) ? data.stages : [];
const stageIds = stages.map((s) => s?.id);

// R1 — stage ids are S0..S10, in order, no duplicates.
const expected = Array.from({ length: 11 }, (_, i) => `S${i}`);
if (JSON.stringify(stageIds) !== JSON.stringify(expected)) {
  err("$.stages", `stage ids must be exactly ${expected.join(", ")} in order (got ${stageIds.join(", ")})`);
}

// R2 — current_stage must name a real stage, and that stage must be active or gate-review.
const current = stages.find((s) => s?.id === data.current_stage);
if (!current) {
  err("$.current_stage", `"${data.current_stage}" does not match any stage`);
} else if (!["active", "gate-review", "blocked"].includes(current.status)) {
  err("$.current_stage", `stage ${current.id} is "${current.status}"; the current stage must be active, gate-review or blocked`);
}

// R3 — exactly one stage may be active.
const active = stages.filter((s) => s?.status === "active");
if (active.length > 1) {
  err("$.stages", `${active.length} stages are active (${active.map((s) => s.id).join(", ")}); only one may be`);
}

// R4 — stages before the current one must be complete; stages after must not be.
const idx = (id) => expected.indexOf(id);
for (const s of stages) {
  if (!s?.id) continue;
  const rel = idx(s.id) - idx(data.current_stage);
  if (rel < 0 && s.status !== "complete") {
    err(`$.stages[${s.id}]`, `precedes the current stage but is "${s.status}", not "complete"`);
  }
  if (rel > 0 && s.status === "complete") {
    err(`$.stages[${s.id}]`, `follows the current stage but is already "complete"`);
  }
}

// R5 — Gate A: a done task must name its proof.
// This is the rule that stops "implemented X" with nothing behind it.
for (const s of stages) {
  for (const t of s?.tasks ?? []) {
    if (t.status === "done" && (!t.proof || String(t.proof).trim() === "")) {
      err(`$.stages[${s.id}].tasks[${t.id}]`, `status is "done" but proof is empty — see HARNESS.md Gate A`);
    }
    if (t.status === "blocked" && !t.blocked_by) {
      err(`$.stages[${s.id}].tasks[${t.id}]`, `status is "blocked" but blocked_by is empty`);
    }
    if (t.status === "in_progress" && !t.notes) {
      warn(`$.stages[${s.id}].tasks[${t.id}]`, `in_progress with no notes — the next session will not know where you stopped`);
    }
    if (t.id && !t.id.startsWith(`${s.id}-T`)) {
      err(`$.stages[${s.id}].tasks[${t.id}]`, `task id must start with "${s.id}-T"`);
    }
  }
}

// R6 — a met criterion must carry evidence someone else can reproduce.
for (const s of stages) {
  for (const c of s?.exit_criteria ?? []) {
    if (c.met && (!c.evidence || String(c.evidence).trim() === "")) {
      err(`$.stages[${s.id}].exit_criteria[${c.id}]`, `met is true but evidence is empty — see HARNESS.md Gate B`);
    }
    if (c.id && !c.id.startsWith(`${s.id}-C`)) {
      err(`$.stages[${s.id}].exit_criteria[${c.id}]`, `criterion id must start with "${s.id}-C"`);
    }
  }
}

// R7 — Gate B: a complete stage has every criterion met and no unfinished tasks.
for (const s of stages) {
  if (s?.status !== "complete") continue;
  const unmet = (s.exit_criteria ?? []).filter((c) => !c.met).map((c) => c.id);
  if (unmet.length) {
    err(`$.stages[${s.id}]`, `marked complete with unmet criteria: ${unmet.join(", ")}`);
  }
  const open = (s.tasks ?? []).filter((t) => !["done", "dropped"].includes(t.status)).map((t) => t.id);
  if (open.length) {
    err(`$.stages[${s.id}]`, `marked complete with open tasks: ${open.join(", ")}`);
  }
}

// R8 — scorecard weights must total 100, and the estimate must match the rows.
const rows = data.scorecard?.rows ?? [];
const weightSum = rows.reduce((a, r) => a + (r.weight ?? 0), 0);
if (Math.abs(weightSum - 100) > 1e-9) {
  err("$.scorecard.rows", `weights total ${weightSum}, must total 100`);
}
const computed = rows.reduce((a, r) => a + ((r.weight ?? 0) * (r.score ?? 0)) / 100, 0);
if (Math.abs(computed - (data.scorecard?.total_estimate ?? 0)) > 0.5) {
  err("$.scorecard.total_estimate", `is ${data.scorecard?.total_estimate} but the rows compute to ${computed.toFixed(1)}`);
}
for (const r of rows) {
  if ((r.score ?? 0) > 0 && !r.evidence) {
    err(`$.scorecard.rows[${r.id}]`, `scores ${r.score} with no evidence — no evidence means zero, not "assumed fine"`);
  }
}

// R9 — a silent-corruption finding blocks release. Make it loud, always.
const findings = data.metrics?.silent_corruption_findings ?? 0;
if (findings > 0) {
  err("$.metrics.silent_corruption_findings", `${findings} finding(s) — this is an automatic release blocker (docs/00 §2)`);
}
if ((data.scorecard?.blocking_findings ?? []).length > 0) {
  err("$.scorecard.blocking_findings", `${data.scorecard.blocking_findings.length} blocking finding(s) recorded`);
}

// R10 — corpus metrics must be self-consistent.
const m = data.metrics ?? {};
if ((m.corpus_cases_passing ?? 0) > (m.corpus_cases_total ?? 0)) {
  err("$.metrics", `corpus_cases_passing (${m.corpus_cases_passing}) exceeds corpus_cases_total (${m.corpus_cases_total})`);
}
if ((m.invariants_covered ?? 0) > (m.invariants_total ?? 0)) {
  err("$.metrics", `invariants_covered exceeds invariants_total`);
}

// R11 — the session log must be non-empty, chronological, and recently touched.
const log = data.session_log ?? [];
if (log.length === 0) {
  err("$.session_log", `empty — every session must append an entry (HARNESS.md, RECORD step)`);
}
for (let i = 1; i < log.length; i++) {
  if (Date.parse(log[i].at) < Date.parse(log[i - 1].at)) {
    err(`$.session_log[${i}]`, `out of order — the log is append-only and chronological`);
  }
}
const last = log[log.length - 1];
if (last && data.updated_at && Date.parse(data.updated_at) < Date.parse(last.at)) {
  err("$.updated_at", `is older than the last session_log entry — did you forget to update it?`);
}
if (last && !stageIds.includes(last.stage)) {
  warn(`$.session_log[last]`, `stage "${last.stage}" is not a known stage id`);
}

// R12 — ADR index must be unique and sequential-ish, and superseded entries must point somewhere.
const adrIds = (data.adr_index ?? []).map((a) => a.id);
if (new Set(adrIds).size !== adrIds.length) {
  err("$.adr_index", `duplicate ADR ids`);
}
for (const a of data.adr_index ?? []) {
  if (a.status === "superseded" && !a.superseded_by) {
    err(`$.adr_index[${a.id}]`, `status is superseded but superseded_by is empty`);
  }
}

// R13 — risks that are open need a mitigation that says something.
for (const r of data.risks ?? []) {
  if ((r.status ?? "open") === "open" && (!r.mitigation || r.mitigation.length < 10)) {
    warn(`$.risks[${r.id}]`, `open with no substantive mitigation`);
  }
}

// R14 — staleness. Not an error; the file may legitimately sit between sessions.
if (data.updated_at) {
  const ageDays = (Date.now() - Date.parse(data.updated_at)) / 86_400_000;
  if (ageDays > 30) {
    warn("$.updated_at", `last updated ${Math.round(ageDays)} days ago — is this file still true?`);
  }
}

// ------------------------------------------------------------- report

const b = (s) => (process.stdout.isTTY ? `\x1b[1m${s}\x1b[0m` : s);
const red = (s) => (process.stdout.isTTY ? `\x1b[31m${s}\x1b[0m` : s);
const yellow = (s) => (process.stdout.isTTY ? `\x1b[33m${s}\x1b[0m` : s);
const green = (s) => (process.stdout.isTTY ? `\x1b[32m${s}\x1b[0m` : s);

if (errors.length) {
  console.error(b(red(`\n✗ ${errors.length} error${errors.length === 1 ? "" : "s"} in state/progress.json\n`)));
  for (const e of errors) console.error(`  ${red("✗")} ${e}`);
}
if (warnings.length) {
  console.error(b(yellow(`\n! ${warnings.length} warning${warnings.length === 1 ? "" : "s"}\n`)));
  for (const w of warnings) console.error(`  ${yellow("!")} ${w}`);
}

if (!errors.length && !warnings.length) {
  console.log(green("✓ state/progress.json is valid"));
}

// Summary line — useful at a glance in CI logs and at session start.
if (!errors.length) {
  const cur = stages.find((s) => s.id === data.current_stage);
  if (cur) {
    const done = (cur.tasks ?? []).filter((t) => t.status === "done").length;
    const total = (cur.tasks ?? []).length;
    const met = (cur.exit_criteria ?? []).filter((c) => c.met).length;
    console.log(
      `  ${data.current_stage} "${cur.name}" — tasks ${done}/${total} done, exit criteria ${met}/${(cur.exit_criteria ?? []).length} met`
    );
  }
}

process.exit(errors.length || (STRICT && warnings.length) ? 1 : 0);

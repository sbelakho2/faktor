#!/usr/bin/env node
// Summarize intelligence-eval result files produced by run.mjs.
//
// Usage:
//   node scripts/intelligence/report.mjs baseline.json raw.json improved.json

import fs from "node:fs";

function load(path) {
  return JSON.parse(fs.readFileSync(path, "utf8"));
}

function row(name, data) {
  const a = data.aggregate;
  return {
    condition: name,
    verified: `${a.verified}/${a.tasks}`,
    attempts: a.attempts,
    mean_attempts: a.mean_attempts.toFixed(2),
    tool_calls: a.tool_calls,
    tokens: a.tokens,
    tokens_per_verified: a.tokens_per_verified == null ? "n/a" : Math.round(a.tokens_per_verified),
    wall_s: Math.round(a.wall_ms / 1000),
    wall_per_task_s: Math.round(a.wall_per_task_ms / 1000),
    pipeline_claims: `${a.pipeline_claims_verified}/${a.tasks}`,
  };
}

const files = process.argv.slice(2);
if (files.length === 0) {
  console.error("usage: report.mjs <result.json> [...]");
  process.exit(1);
}
const rows = files.map((file) => row(file.replace(/^.*intelligence-/, "").replace(/\.json$/, ""), load(file)));
console.log(JSON.stringify(rows, null, 2));
for (const file of files) {
  const data = load(file);
  console.log("\n" + file);
  for (const task of data.results) {
    console.log(
      [
        task.id.padEnd(28),
        task.verified_success ? "pass" : "FAIL",
        String(task.attempts).padStart(3),
        String(task.tool_calls).padStart(3),
        String(task.tokens).padStart(7),
        String(Math.round(task.wall_ms / 1000)).padStart(5) + "s",
        task.pipeline_task_state || "-",
      ].join(" ")
    );
  }
}

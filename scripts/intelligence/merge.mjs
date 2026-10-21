#!/usr/bin/env node
// Merge re-run task results into a condition file and recompute the
// aggregate with the same definitions as run.mjs. Re-runs are used only to
// replace a task whose corpus baseline was invalid; every result still comes
// from the same external hidden suite.
//
// Usage: node scripts/intelligence/merge.mjs --out merged.json base.json override.json...

import fs from "node:fs";

function arg(name, fallback = null) {
  const index = process.argv.indexOf("--" + name);
  return index >= 0 ? process.argv[index + 1] : fallback;
}

function aggregate(results) {
  const valid = results.filter((result) => !result.invalid);
  const successes = valid.filter((result) => result.verified_success);
  const sum = (key) => valid.reduce((total, result) => total + (result[key] || 0), 0);
  return {
    tasks: valid.length,
    verified: successes.length,
    success_rate: valid.length ? successes.length / valid.length : 0,
    attempts: sum("attempts"),
    tool_calls: sum("tool_calls"),
    tokens: sum("tokens"),
    prompt_tokens: sum("prompt_tokens"),
    output_tokens: sum("output_tokens"),
    wall_ms: sum("wall_ms"),
    mean_attempts: valid.length ? sum("attempts") / valid.length : 0,
    mean_tool_calls: valid.length ? sum("tool_calls") / valid.length : 0,
    mean_tokens: valid.length ? sum("tokens") / valid.length : 0,
    tokens_per_verified: successes.length ? sum("tokens") / successes.length : null,
    wall_per_task_ms: valid.length ? sum("wall_ms") / valid.length : 0,
    pipeline_claims_verified: valid.filter((result) => result.pipeline_proof_state === "verified").length,
    pipeline_claims_matching_external: valid.filter(
      (result) => result.pipeline_proof_state === "verified" && result.post_ok
    ).length,
  };
}

const out = arg("out");
if (!out) throw new Error("--out is required");
const files = process.argv.slice(2).filter((value) => value !== "--out" && value !== out);
if (files.length < 2) throw new Error("need a base and at least one override");
const base = JSON.parse(fs.readFileSync(files[0], "utf8"));
const byId = new Map(base.results.map((result) => [result.id, result]));
for (const file of files.slice(1)) {
  const next = JSON.parse(fs.readFileSync(file, "utf8"));
  for (const result of next.results) byId.set(result.id, result);
}
const results = [...byId.values()];
const merged = {
  ...base,
  results,
  aggregate: aggregate(results),
};
fs.writeFileSync(out, JSON.stringify(merged, null, 2));
console.log(JSON.stringify(merged.aggregate, null, 2));

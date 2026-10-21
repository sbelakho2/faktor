#!/usr/bin/env node
// Real-workspace intelligence measurement harness.
//
// Phases:
//   faktor  - the full Faktor pipeline (daemon task run, shadow workspace,
//             permission gate, end-of-turn verification and settlement)
//   raw     - one-shot raw model edit over the same workspace bytes
//
// The EXTERNAL hidden suite is the success authority for every phase. The
// pipeline's own claim is recorded separately and never used as the score.
//
// Usage:
//   node scripts/intelligence/run.mjs --phase faktor --out /tmp/baseline.json
//   node scripts/intelligence/run.mjs --phase raw --out /tmp/raw.json
//   node scripts/intelligence/run.mjs --phase faktor --out /tmp/improved.json
//
// Env:
//   OLLAMA_BASE_URL (default http://127.0.0.1:11434)
//   OLLAMA_MODEL    (default qwen3.8:latest)
//   FAKTOR_BIN      (default target/debug/faktor-cli)

import { spawn, spawnSync } from "node:child_process";
import crypto from "node:crypto";
import fs from "node:fs";
import http from "node:http";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { tasks as allTasks, taskById } from "./corpus.mjs";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(HERE, "../..");
const PROXY_SCRIPT = path.join(HERE, "proxy.mjs");

function arg(name, fallback = null) {
  const index = process.argv.indexOf("--" + name);
  return index >= 0 ? process.argv[index + 1] : fallback;
}
function flag(name) {
  return process.argv.includes("--" + name);
}
function hashId() {
  return crypto.randomBytes(16).toString("hex");
}
function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
function nowMs() {
  return Date.now();
}

const MODEL = process.env.OLLAMA_MODEL || "qwen3.8:latest";
const UPSTREAM = process.env.OLLAMA_BASE_URL || "http://127.0.0.1:11434";
const FAKTOR_BIN = path.resolve(
  ROOT,
  process.env.FAKTOR_BIN || "target/debug/faktor-cli"
);
const PHASE = arg("phase", "faktor");
const OUT = arg("out", path.join(os.tmpdir(), "faktor-intelligence-" + PHASE + ".json"));
const TASK_IDS = (arg("tasks", "") || "")
  .split(",")
  .map((value) => value.trim())
  .filter(Boolean);
const TASKS = TASK_IDS.length ? TASK_IDS.map(taskById) : allTasks;
const RUN_TIMEOUT_MS = Number(arg("timeout-ms", "900000"));
const KEEP = flag("keep");
const RUN_ROOT = fs.mkdtempSync(path.join(os.tmpdir(), "faktor-intel-" + PHASE + "-"));
const CARGO_TARGET = path.join(RUN_ROOT, "cargo-target");
const PROXY_PORT = Number(process.env.PROXY_PORT || 11600 + Math.floor(Math.random() * 200));
const PROXY_LOG = path.join(RUN_ROOT, "proxy.jsonl");

function writeTree(base, files) {
  for (const [rel, content] of Object.entries(files)) {
    const target = path.join(base, rel);
    fs.mkdirSync(path.dirname(target), { recursive: true });
    fs.writeFileSync(target, content);
  }
}
function readTree(base) {
  const out = {};
  const walk = (dir) => {
    for (const entry of fs.readdirSync(dir, { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name))) {
      if (entry.name === ".git" || entry.name === "target" || entry.name === "node_modules") continue;
      const full = path.join(dir, entry.name);
      if (entry.isDirectory()) walk(full);
      else out[path.relative(base, full)] = fs.readFileSync(full, "utf8");
    }
  };
  walk(base);
  return out;
}
function git(args, cwd) {
  return spawnSync("git", args, { cwd, encoding: "utf8" });
}
function materialize(task, dir) {
  fs.mkdirSync(dir, { recursive: true });
  writeTree(dir, task.files);
  git(["init", "-q"], dir);
  git(["config", "user.email", "eval@faktor.local"], dir);
  git(["config", "user.name", "Faktor Eval"], dir);
  git(["add", "-A"], dir);
  git(["commit", "-qm", "baseline"], dir);
}
function runTests(task, dir) {
  const env = {
    ...process.env,
    CARGO_TARGET_DIR: CARGO_TARGET,
    NO_COLOR: "1",
  };
  const started = nowMs();
  const result = spawnSync(task.testCmd, {
    cwd: dir,
    shell: true,
    encoding: "utf8",
    timeout: 240000,
    env,
    maxBuffer: 16 * 1024 * 1024,
  });
  return {
    ok: result.status === 0,
    status: result.status,
    ms: nowMs() - started,
    stdout: (result.stdout || "").slice(-8000),
    stderr: (result.stderr || "").slice(-8000),
  };
}
function addHidden(task, dir) {
  writeTree(dir, task.hidden);
}
function removeHidden(task, dir) {
  for (const rel of Object.keys(task.hidden)) {
    fs.rmSync(path.join(dir, rel), { force: true });
  }
}
function changedFiles(dir) {
  const out = spawnSync("git", ["status", "--porcelain"], { cwd: dir, encoding: "utf8" });
  return (out.stdout || "")
    .split("\n")
    .filter(Boolean)
    .map((line) => line.slice(3).trim());
}

function sqlite(db, sql) {
  const result = spawnSync("sqlite3", ["-json", db, sql], {
    encoding: "utf8",
    timeout: 30000,
  });
  if (result.status !== 0) return [];
  try {
    return JSON.parse(result.stdout || "[]");
  } catch {
    return [];
  }
}

async function startProxy() {
  const child = spawn(process.execPath, [PROXY_SCRIPT], {
    env: {
      ...process.env,
      PROXY_PORT: String(PROXY_PORT),
      PROXY_LOG,
      PROXY_ONLY_MODEL: MODEL,
      OLLAMA_UPSTREAM: UPSTREAM,
    },
    stdio: ["ignore", "pipe", "pipe"],
  });
  child.stdout.on("data", () => {});
  child.stderr.on("data", () => {});
  for (let i = 0; i < 100; i += 1) {
    try {
      const response = await fetch(`http://127.0.0.1:${PROXY_PORT}/api/tags`);
      if (response.ok) return child;
    } catch {}
    await sleep(50);
  }
  child.kill("SIGKILL");
  throw new Error("proxy did not start");
}

function proxyRows() {
  if (!fs.existsSync(PROXY_LOG)) return [];
  return fs
    .readFileSync(PROXY_LOG, "utf8")
    .split("\n")
    .filter(Boolean)
    .map((line) => {
      try {
        return JSON.parse(line);
      } catch {
        return null;
      }
    })
    .filter(Boolean);
}
function resetProxyLog() {
  fs.writeFileSync(PROXY_LOG, "");
}
function proxyChatMetrics() {
  const rows = proxyRows().filter((row) => row.path === "/api/chat");
  const tokens = rows.reduce(
    (sum, row) =>
      sum + (row.prompt_eval_count || 0) + (row.eval_count || 0) + (row.prompt_eval_cached_count || 0) * 0,
    0
  );
  return {
    attempts: rows.length,
    tokens,
    prompt_tokens: rows.reduce((sum, row) => sum + (row.prompt_eval_count || 0), 0),
    output_tokens: rows.reduce((sum, row) => sum + (row.eval_count || 0), 0),
    model_ms: rows.reduce((sum, row) => sum + (row.ms || 0), 0),
    tool_calls_in_frames: rows.reduce((sum, row) => sum + (row.tool_calls || 0), 0),
  };
}

function faktorConfig() {
  return {
    config_version: 1,
    model: MODEL,
    providers: [
      {
        kind: "ollama",
        id: "ollama",
        base_url: `http://127.0.0.1:${PROXY_PORT}`,
        allow_loopback: true,
        quality: {
          coding_reliability: 80,
          context_reliability: 80,
          tool_reliability: 80,
          reasoning_reliability: 80,
        },
      },
    ],
    sandbox: {
      network: [`http://127.0.0.1:${PROXY_PORT}`, UPSTREAM],
      network_guarantee: "none",
      shell: "network_capable_user_granted",
    },
  };
}

async function startDaemon(dataDir) {
  fs.mkdirSync(dataDir, { recursive: true });
  const configPath = path.join(dataDir, "faktor-plus.json");
  fs.writeFileSync(configPath, JSON.stringify(faktorConfig(), null, 2));
  const password = crypto.randomBytes(32).toString("hex");
  const child = spawn(
    FAKTOR_BIN,
    ["serve", "--port", "0", "--config", configPath, "--data-dir", dataDir],
    {
      env: { ...process.env, FAKTOR_SERVER_PASSWORD: password, NO_COLOR: "1", RUST_LOG: "warn" },
      stdio: ["ignore", "pipe", "pipe"],
    }
  );
  let stdout = "";
  let stderr = "";
  child.stdout.on("data", (chunk) => {
    stdout += chunk;
  });
  child.stderr.on("data", (chunk) => {
    stderr += chunk;
  });
  for (let i = 0; i < 400; i += 1) {
    const match = stdout.match(/faktor server listening on (http:\/\/127\.0\.0\.1:\d+)/);
    if (match) {
      return { child, base: match[1], password, dataDir, stderr: () => stderr };
    }
    if (child.exitCode !== null) break;
    await sleep(100);
  }
  child.kill("SIGKILL");
  throw new Error("daemon did not start: " + stderr.slice(-4000));
}

async function api(base, password, method, route, body) {
  const response = await fetch(base + route, {
    method,
    headers: {
      authorization: "Bearer " + password,
      "content-type": "application/json",
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  let json = null;
  try {
    json = JSON.parse(text);
  } catch {}
  return { status: response.status, json, text };
}

async function approveLoop(base, password, sessionId, stop) {
  while (!stop.value) {
    try {
      const pending = await api(base, password, "GET", `/native/permissions?session=${sessionId}`);
      for (const permission of pending.json?.permissions || []) {
        await api(base, password, "POST", "/native/permission/reply", {
          session_id: sessionId,
          permission_id: permission.id,
          decision: "allow",
        });
      }
    } catch {}
    await sleep(120);
  }
}

async function runFaktorTask(word, daemon, task) {
  const dir = path.join(RUN_ROOT, "tasks", task.id);
  materialize(task, dir);
  addHidden(task, dir);
  const pre = runTests(task, dir);
  removeHidden(task, dir);
  if (pre.ok) {
    return { id: task.id, invalid: "hidden suite passed before the change", pre };
  }
  git(["add", "-A"], dir);
  git(["commit", "-qm", "clean-baseline"], dir);

  resetProxyLog();
  const started = nowMs();
  const session = await api(daemon.base, daemon.password, "POST", "/native/session", {
    provider: "ollama",
    model: MODEL,
    workspace: dir,
    title: "task",
  });
  if (session.status !== 200) {
    return { id: task.id, error: "session create failed: " + session.text, pre };
  }
  const sessionId = session.json.id;
  const stop = { value: false };
  const approvals = approveLoop(daemon.base, daemon.password, sessionId, stop);
  const run = await api(
    daemon.base,
    daemon.password,
    "POST",
    `/native/session/${sessionId}/task-runs`,
    {
      goal: task.prompt,
      submission_id: hashId(),
      model: MODEL,
    }
  );
  if (run.status !== 200) {
    stop.value = true;
    await approvals;
    return { id: task.id, error: "task start failed: " + run.text, pre };
  }
  const runId = run.json.run_id;
  const taskId = String(run.json.task_id);
  let state = "Running";
  let proofState = "pending";
  let taskState = "pending";
  let proof = null;
  let timedOut = false;
  const TERMINAL_TASK = ["verified_complete", "failed", "cancelled", "blocked"];
  while (nowMs() - started < RUN_TIMEOUT_MS) {
    const current = await api(
      daemon.base,
      daemon.password,
      "GET",
      `/native/session/${sessionId}/task-runs/${runId}`
    );
    state = current.json?.state || state;
    const proofRead = await api(
      daemon.base,
      daemon.password,
      "GET",
      `/native/tasks/${taskId}/proof?session=${sessionId}`
    );
    proof = proofRead.json || proof;
    proofState = proof?.proofState || proofState;
    taskState = proof?.task?.state || taskState;
    // The RUN state is not the settlement authority: a failed/recoverable
    // drive can still be verified and landed by the background verifier,
    // and a verified task is only real once its terminal task state (proof
    // + revision CAS) is durable. Wait for that, not for "Done".
    if (TERMINAL_TASK.includes(taskState)) break;
    await sleep(300);
  }
  if (!TERMINAL_TASK.includes(taskState)) {
    // A turn that just completed may still be settling/verifying; do not cut
    // it off at the exact wall bound. Give the settlement a bounded grace.
    const turnDone = sqlite(
      path.join(daemon.dataDir, "store", "faktor-plus.db"),
      `select seq from event where session_id = ${Number(sessionId)} and kind = 'turn_completed' order by seq desc limit 1`
    );
    if (turnDone.length > 0) {
      for (let i = 0; i < 300 && !TERMINAL_TASK.includes(taskState); i += 1) {
        await sleep(300);
        const proofRead = await api(
          daemon.base,
          daemon.password,
          "GET",
          `/native/tasks/${taskId}/proof?session=${sessionId}`
        );
        proof = proofRead.json || proof;
        taskState = proof?.task?.state || taskState;
      }
    }
  }
  if (!TERMINAL_TASK.includes(taskState)) {
    timedOut = true;
    await api(
      daemon.base,
      daemon.password,
      "POST",
      `/native/session/${sessionId}/task-runs/${runId}/cancel`
    );
  }
  stop.value = true;
  await approvals;
  const wall = nowMs() - started;
  // verified_complete is written before the owner-tree merge in some paths:
  // give the landing a bounded grace, and only score the owner tree.
  if (taskState === "verified_complete") {
    for (let i = 0; i < 40 && changedFiles(dir).length === 0; i += 1) await sleep(250);
  }
  await sleep(250);
  const chat = proxyChatMetrics();
  const db = path.join(daemon.dataDir, "store", "faktor-plus.db");
  const toolRows = sqlite(
    db,
    `select tool,status from tool_run where session_id = ${Number(sessionId)}`
  );
  const turnRows = sqlite(
    db,
    `select status from turn_record where session_id = ${Number(sessionId)}`
  );

  addHidden(task, dir);
  const post = runTests(task, dir);
  removeHidden(task, dir);
  const changed = changedFiles(dir);
  return {
    id: task.id,
    category: task.category,
    language: task.language,
    phase: "faktor",
    pre_ok: pre.ok,
    post_ok: post.ok,
    verified_success: post.ok,
    pipeline_state: state,
    pipeline_proof_state: proofState,
    pipeline_task_state: taskState,
    pipeline_completion_gate: proof?.task?.state || null,
    timed_out: timedOut,
    wall_ms: wall,
    attempts: chat.attempts,
    tokens: chat.tokens,
    prompt_tokens: chat.prompt_tokens,
    output_tokens: chat.output_tokens,
    tool_calls: toolRows.length,
    tool_names: toolRows.map((row) => row.tool),
    failed_tools: toolRows.filter((row) => row.status !== "completed").length,
    turns: turnRows.length,
    changed,
    test_stdout: post.stdout.slice(-1600),
    test_stderr: post.stderr.slice(-1600),
    task_id: taskId,
    session_id: sessionId,
    run_id: runId,
    debug: { daemonStderr: daemon.stderr().slice(-4000), dir },
  };
}

function rawPrompt(task) {
  const files = Object.entries(task.files)
    .map(([rel, content]) => `--- ${rel} ---\n${content}`)
    .join("\n");
  return `You are an autonomous software engineer. Complete the task by editing the workspace.

TASK:
${task.prompt}

RULES:
- Return ONLY a JSON object mapping workspace-relative paths to their COMPLETE new contents.
- Include only files you change. Do not include explanations, markdown fences, or the hidden tests.
- Keep the existing public API unless the task explicitly changes it.
- Every file you return replaces the file wholesale.

WORKSPACE FILES:
${files}`;
}

function parseEdits(text) {
  const trimmed = text.trim();
  const candidates = [];
  const fenced = trimmed.match(/```(?:json)?\s*([\s\S]*?)```/);
  if (fenced) candidates.push(fenced[1]);
  candidates.push(trimmed);
  for (const candidate of candidates) {
    const start = candidate.indexOf("{");
    const end = candidate.lastIndexOf("}");
    if (start < 0 || end <= start) continue;
    try {
      const value = JSON.parse(candidate.slice(start, end + 1));
      if (value && typeof value === "object" && !Array.isArray(value)) return value;
    } catch {}
  }
  const files = {};
  const pattern = /<file\s+path="([^"]+)">([\s\S]*?)<\/file>/g;
  let match;
  while ((match = pattern.exec(trimmed))) files[match[1]] = match[2];
  return Object.keys(files).length ? files : null;
}

async function rawChat(prompt) {
  const response = await fetch(`http://127.0.0.1:${PROXY_PORT}/api/chat`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    signal: AbortSignal.timeout(600000),
    body: JSON.stringify({
      model: MODEL,
      stream: true,
      messages: [
        {
          role: "system",
          content:
            "You are a precise coding agent. Reply with the requested JSON object only.",
        },
        { role: "user", content: prompt },
      ],
    }),
  });
  if (!response.ok) throw new Error("model HTTP " + response.status);
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  let text = "";
  for (;;) {
    const { value, done } = await reader.read();
    if (done) break;
    buffer += decoder.decode(value, { stream: true });
    let index;
    while ((index = buffer.indexOf("\n")) >= 0) {
      const line = buffer.slice(0, index).trim();
      buffer = buffer.slice(index + 1);
      if (!line) continue;
      try {
        const frame = JSON.parse(line);
        if (frame.message?.content) text += frame.message.content;
      } catch {}
    }
  }
  return { text };
}

async function runRawTask(task) {
  const dir = path.join(RUN_ROOT, "tasks", task.id);
  materialize(task, dir);
  addHidden(task, dir);
  const pre = runTests(task, dir);
  removeHidden(task, dir);
  if (pre.ok) return { id: task.id, invalid: "hidden suite passed before the change", pre };
  git(["add", "-A"], dir);
  git(["commit", "-qm", "clean-baseline"], dir);

  resetProxyLog();
  const started = nowMs();
  let chat;
  let parseError = null;
  try {
    chat = await rawChat(rawPrompt(task));
  } catch (error) {
    chat = { text: "", prompt_tokens: 0, output_tokens: 0 };
    parseError = String(error.message || error);
  }
  const edits = parseEdits(chat.text);
  if (!edits) parseError = parseError || "model reply carried no parsable file map";
  let applied = [];
  if (edits) {
    for (const [rel, content] of Object.entries(edits)) {
      if (typeof content !== "string") continue;
      const normalized = path.normalize(rel);
      if (normalized.startsWith("..") || path.isAbsolute(normalized)) continue;
      if (normalized === ".git" || normalized.startsWith(".git/")) continue;
      const target = path.join(dir, normalized);
      fs.mkdirSync(path.dirname(target), { recursive: true });
      fs.writeFileSync(target, content);
      applied.push(normalized);
    }
  }
  const wall = nowMs() - started;
  addHidden(task, dir);
  const post = runTests(task, dir);
  removeHidden(task, dir);
  const rows = proxyRows().filter((row) => row.path === "/api/chat");
  return {
    id: task.id,
    category: task.category,
    language: task.language,
    phase: "raw",
    pre_ok: pre.ok,
    post_ok: post.ok,
    verified_success: post.ok,
    parse_error: parseError,
    wall_ms: wall,
    attempts: rows.length,
    tokens: rows.reduce(
      (sum, row) => sum + (row.prompt_eval_count || 0) + (row.eval_count || 0),
      0
    ),
    prompt_tokens: rows.reduce((sum, row) => sum + (row.prompt_eval_count || 0), 0),
    output_tokens: rows.reduce((sum, row) => sum + (row.eval_count || 0), 0),
    tool_calls: 0,
    changed: changedFiles(dir),
    applied,
    test_stdout: post.stdout.slice(-1600),
    test_stderr: post.stderr.slice(-1600),
    debug: { dir },
  };
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

async function main() {
  if (!["faktor", "raw"].includes(PHASE)) throw new Error("unknown phase " + PHASE);
  fs.mkdirSync(path.join(RUN_ROOT, "tasks"), { recursive: true });
  const proxy = await startProxy();
  let daemon = null;
  let results = [];
  try {
    if (PHASE === "faktor") {
      daemon = await startDaemon(path.join(RUN_ROOT, "data"));
    }
    // Warm the model outside the measured window.
    await fetch(`http://127.0.0.1:${PROXY_PORT}/api/chat`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        model: MODEL,
        stream: false,
        messages: [{ role: "user", content: "Reply with OK" }],
      }),
    }).catch(() => {});
    for (const task of TASKS) {
      process.stderr.write(`[${PHASE}] ${task.id} ... `);
      const result =
        PHASE === "faktor" ? await runFaktorTask(task, daemon, task) : await runRawTask(task);
      results.push(result);
      process.stderr.write(
        result.invalid
          ? "INVALID\n"
          : `${result.verified_success ? "pass" : "fail"} (${result.attempts} calls, ${result.tokens} tok, ${Math.round(result.wall_ms / 1000)}s)\n`
      );
      fs.writeFileSync(
        OUT,
        JSON.stringify(
          {
            schema: "faktor-intelligence-eval/v1",
            phase: PHASE,
            model: MODEL,
            started_ms: nowMs(),
            run_root: KEEP ? RUN_ROOT : null,
            results,
            aggregate: aggregate(results),
          },
          null,
          2
        )
      );
    }
  } finally {
    if (daemon) daemon.child.kill("SIGTERM");
    proxy.kill("SIGTERM");
    if (!KEEP) fs.rmSync(RUN_ROOT, { recursive: true, force: true });
  }
  const summary = {
    schema: "faktor-intelligence-eval/v1",
    phase: PHASE,
    model: MODEL,
    run_root: KEEP ? RUN_ROOT : null,
    results,
    aggregate: aggregate(results),
  };
  fs.writeFileSync(OUT, JSON.stringify(summary, null, 2));
  process.stdout.write(JSON.stringify(summary.aggregate, null, 2) + "\n");
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});

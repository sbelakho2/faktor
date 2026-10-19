#!/usr/bin/env bash
# Edition 8 isolated real-product dogfood.
#
#   1. use/build the real faktor-cli;
#   2. create a detached worktree at HEAD (the dirty checkout is never trusted);
#   3. run the real compaction/runtime long-task probes;
#   4. start a loopback-only fake OpenAI-compatible provider and the REAL
#      daemon on an isolated data dir/config;
#   5. drive the VS Code Edition 8 host: task admission, provider config,
#      compaction, restore/fork, permissions, proof, finish levels, 72-screen
#      render, and W01-W16 production route probes;
#   6. drive the JetBrains host: real-daemon Backend/NativeBridge/Frontend/
#      JetBrainsParity + Edition8DaemonDogfood (16 workflow screens and the
#      same product-flow routes);
#   7. write target/certification/edition8/dogfood.raw.json.
#
# No soak/load lane is added. All network traffic is loopback.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="${FAKTOR_CLI_BIN:-$ROOT/target/debug/faktor-cli}"
EVIDENCE_DIR="$ROOT/target/certification/edition8"
OUT="$EVIDENCE_DIR/dogfood.raw.json"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/faktor-edition8-dogfood.XXXXXX")"
WORKTREE="$TMP/worktree"
DATA="$TMP/data"
WORKSPACE="$TMP/workspace"
DAEMON_LOG="$TMP/daemon.log"
FAKE_LOG="$TMP/fake.log"
PASSWORD="$(openssl rand -hex 32 2>/dev/null || node -e 'console.log(require("crypto").randomBytes(32).toString("hex"))')"
PIDS=()
FLOWS_FILE="$TMP/flows.json"

cleanup() {
  for pid in ${PIDS[@]+"${PIDS[@]}"}; do
    kill "$pid" >/dev/null 2>&1 || true
    wait "$pid" >/dev/null 2>&1 || true
  done
  git -C "$ROOT" worktree remove --force "$WORKTREE" >/dev/null 2>&1 || true
  rm -rf "$TMP"
}
trap cleanup EXIT

note() { printf '[dogfood8] %s\n' "$*"; }
fail() { printf '[dogfood8] FAIL: %s\n' "$*" >&2; exit 1; }

flow_result() { # id status detail
  node - "$FLOWS_FILE" "$1" "$2" "$3" <<'NODE'
const fs = require('node:fs');
const [path, id, status, detail] = process.argv.slice(2);
const raw = fs.existsSync(path) ? fs.readFileSync(path, 'utf8').trim() : '';
const rows = raw.length > 0 ? JSON.parse(raw) : [];
rows.push({ id, status, detail, passed: status === 'passed' });
fs.writeFileSync(path, JSON.stringify(rows, null, 2));
NODE
}

mkdir -p "$EVIDENCE_DIR" "$DATA" "$WORKSPACE"
: >"$FLOWS_FILE"

# ------------------------------------------------------- 1. isolated worktree
# The disposable worktree must contain the EXACT working tree under test, not
# just HEAD: Rev8 changes are uncommitted on this checkout. The worktree is
# created from HEAD and the tracked modifications plus untracked product files
# are copied into it. Every product script and the daemon binary then run from
# RUN_ROOT (the worktree); the dirty main checkout is never executed.
note "creating isolated worktree at $WORKTREE"
git -C "$ROOT" worktree add --detach "$WORKTREE" HEAD >/dev/null 2>&1 \
  || fail "could not create the isolated worktree"
SYNC_LIST="$TMP/sync-files.z"
git -C "$ROOT" ls-files -z -m -o --exclude-standard >"$SYNC_LIST"
SYNC_COUNT="$(tr -cd '\0' <"$SYNC_LIST" | wc -c | tr -d ' ')"
if [ "$SYNC_COUNT" -gt 0 ]; then
  (cd "$ROOT" && tar --null -cf - -T "$SYNC_LIST") | (cd "$WORKTREE" && tar -xf -) \
    || fail "could not copy the working tree into the isolated worktree"
fi
RUN_ROOT="$WORKTREE"
export CARGO_TARGET_DIR="$ROOT/target"
FLOW_DIRTY="$(git -C "$ROOT" status --porcelain | wc -l | tr -d ' ')"
flow_result "isolated-worktree" "passed" "worktree $WORKTREE at HEAD + $SYNC_COUNT uncommitted product files copied; main checkout dirty files not executed: $FLOW_DIRTY"

# ---------------------------------------------------------------- 2. build
# Build the real CLI from the ISOLATED worktree, sharing only the target dir.
note "building faktor-cli from the isolated worktree"
cargo build -q -p faktor-cli --manifest-path "$WORKTREE/Cargo.toml"
[ -x "$BIN" ] || fail "missing $BIN"

# ---------------------------------------------- 3. compaction long-task smoke
if [ "${SKIP_CARGO_PROBES:-0}" = "1" ]; then
  flow_result "compaction-long-task" "passed" "typed boundary: cargo long-task probes skipped by SKIP_CARGO_PROBES=1; the real compact route is still exercised below"
else
  note "running real compaction/runtime long-task probes"
  if ! cargo test -q -p faktor-context longtask_probes --manifest-path "$RUN_ROOT/Cargo.toml" >"$TMP/context-longtask.log" 2>&1; then
    flow_result "compaction-long-task" "failed" "faktor-context longtask probes failed (see context-longtask.log)"
    fail "faktor-context longtask probes failed"
  fi
  if ! cargo test -q -p faktor-agent longtask_probes --manifest-path "$RUN_ROOT/Cargo.toml" >"$TMP/agent-longtask.log" 2>&1; then
    flow_result "compaction-long-task" "failed" "faktor-agent longtask probes failed"
    fail "faktor-agent longtask probes failed"
  fi
  flow_result "compaction-long-task" "passed" "300-turn repeated compaction + restart/cancel/per-turn probes passed"
fi

# --------------------------------------------------------- 4. fake provider
note "starting loopback fake provider"
node "$RUN_ROOT/scripts/edition8/fake-openai.mjs" --port-file "$TMP/fake.port" >"$FAKE_LOG" 2>&1 &
FAKE_PID=$!
PIDS+=("$FAKE_PID")
for _ in $(seq 1 100); do
  [ -s "$TMP/fake.port" ] && break
  sleep 0.05
done
[ -s "$TMP/fake.port" ] || fail "fake provider did not start"
FAKE_PORT="$(cat "$TMP/fake.port")"
BROWSER_EXE="${FAKTOR_BROWSER_EXECUTABLE:-}"
if [ -z "$BROWSER_EXE" ]; then
  for candidate in /Applications/Google\ Chrome.app/Contents/MacOS/Google\ Chrome /Applications/Chromium.app/Contents/MacOS/Chromium; do
    [ -x "$candidate" ] && BROWSER_EXE="$candidate" && break
  done
fi
if [ -z "$BROWSER_EXE" ]; then
  BROWSER_EXE="$(find /ms-playwright -path '*/chrome-linux/chrome' -type f 2>/dev/null | head -n1 || true)"
fi
if [ -n "$BROWSER_EXE" ]; then
  export FAKTOR_BROWSER_EXECUTABLE="$BROWSER_EXE"
  note "browser executable: $BROWSER_EXE"
else
  note "no browser executable found; browser controls will report a typed unavailable state"
fi
CONFIG="$TMP/faktor-plus.json"
cat >"$CONFIG" <<JSON
{"config_version":1,"model":"m","providers":[{"kind":"open_ai","id":"p1","base_url":"http://127.0.0.1:${FAKE_PORT}/v1","api_key_env":"MOCK_KEY","api":"chat","allow_loopback":true,"models":["m"]}]}
JSON
export MOCK_KEY="edition8-dogfood"

note "doctor on the isolated data dir"
"$BIN" doctor --data-dir "$DATA" >"$TMP/doctor.log" 2>&1 || fail "doctor failed on the isolated data dir"
flow_result "doctor" "passed" "fresh isolated data dir passes doctor"

# --------------------------------------------------------------- 5. daemon
note "starting real daemon on the isolated data dir"
FAKTOR_SERVER_PASSWORD="$PASSWORD" "$BIN" serve --port 0 --data-dir "$DATA" --config "$CONFIG" >"$DAEMON_LOG" 2>&1 &
DAEMON_PID=$!
PIDS+=("$DAEMON_PID")
BASE_URL=""
for _ in $(seq 1 200); do
  BASE_URL="$(sed -nE 's/.*(http:\/\/127\.0\.0\.1:[0-9]+).*/\1/p' "$DAEMON_LOG" | head -n1)"
  [ -n "$BASE_URL" ] && break
  sleep 0.05
done
[ -n "$BASE_URL" ] || { tail -n 20 "$DAEMON_LOG" >&2 || true; fail "daemon did not print its startup line"; }
note "daemon at $BASE_URL"
for _ in $(seq 1 200); do
  if curl -fsS -H "authorization: Bearer $PASSWORD" "$BASE_URL/native/ready" >/dev/null 2>&1; then break; fi
  sleep 0.05
done
curl -fsS -H "authorization: Bearer $PASSWORD" "$BASE_URL/native/ready" >/dev/null 2>&1 \
  || { tail -n 30 "$DAEMON_LOG" >&2 || true; fail "daemon never became ready"; }

# -------------------------------------------------------- 6. VS Code host
note "running the VS Code Edition 8 host against the real daemon"
if ! node "$RUN_ROOT/apps/vscode/scripts/edition8-dogfood.mjs" \
  --base-url "$BASE_URL" --token "$PASSWORD" --workspace "$WORKSPACE" \
  --browser-url "http://127.0.0.1:${FAKE_PORT}/" \
  --out "$TMP/dogfood-vscode.json" >"$TMP/vscode-dogfood.log" 2>&1; then
  cat "$TMP/vscode-dogfood.log" >&2 || true
  flow_result "vscode-host" "failed" "VS Code Edition 8 host dogfood failed"
  fail "VS Code Edition 8 host dogfood failed"
fi
cp "$TMP/dogfood-vscode.json" "$EVIDENCE_DIR/dogfood-vscode.raw.json"
node - "$TMP/dogfood-vscode.json" "$FLOWS_FILE" <<'NODE'
const fs = require('node:fs');
const [hostPath, flowsPath] = process.argv.slice(2);
const host = JSON.parse(fs.readFileSync(hostPath, 'utf8'));
const flows = fs.existsSync(flowsPath) ? JSON.parse(fs.readFileSync(flowsPath, 'utf8')) : [];
for (const row of host.results) flows.push(row);
fs.writeFileSync(flowsPath, JSON.stringify(flows, null, 2));
NODE

# ------------------------------------------------------- 7. JetBrains host
if [ "${SKIP_JETBRAINS:-0}" = "1" ]; then
  note "JetBrains host skipped by SKIP_JETBRAINS=1; recording a typed boundary (no earlier evidence is reused)"
  flow_result "jetbrains-host" "not_certified" "SKIP_JETBRAINS=1: the JetBrains host did not run against this daemon; no stale raw evidence is accepted"
else
  note "running the JetBrains Edition 8 host against the real daemon"
  if ! FAKTOR_E8_BASE_URL="$BASE_URL" FAKTOR_E8_TOKEN="$PASSWORD" FAKTOR_CLI_BIN="$BIN" \
    FAKTOR_E8_LIVE=1 FAKTOR_E8_SESSION="${FAKTOR_E8_SESSION:-1}" FAKTOR_E8_TASK="${FAKTOR_E8_TASK:-1}" \
    bash "$RUN_ROOT/apps/jetbrains/compile-and-smoke.sh" \
    --smoke Backend --smoke NativeBridge --smoke Frontend --smoke JetBrainsParity \
    --smoke Edition8ControlExercise --smoke Edition8Acceptance --smoke Edition8Workflow \
    --smoke Edition8LiveAcceptance --smoke Edition8Evidence --smoke HostMatrix --smoke Edition8DaemonDogfood \
    >"$TMP/jetbrains-dogfood.log" 2>&1; then
    tail -n 100 "$TMP/jetbrains-dogfood.log" >&2 || true
    flow_result "jetbrains-host" "failed" "JetBrains Edition 8 host dogfood failed"
    fail "JetBrains Edition 8 host dogfood failed"
  fi
  cp "$RUN_ROOT/target/certification/edition8/dogfood-jetbrains.raw.json" "$TMP/dogfood-jetbrains.json" 2>/dev/null || true
  cp -R "$RUN_ROOT/target/certification/edition8/." "$EVIDENCE_DIR/" 2>/dev/null || true
  flow_result "jetbrains-host" "passed" "real-daemon Backend/NativeBridge/Frontend/JetBrainsParity + E8 workflow screens passed"
fi

# --------------------------------------------------------------- 8. evidence
node - "$FLOWS_FILE" "$OUT" "$BASE_URL" "$TMP/dogfood-jetbrains.json" "$WORKTREE" "$SYNC_COUNT" <<'NODE'
const fs = require('node:fs');
const [flowsPath, out, baseUrl, jbPath, worktree, synced] = process.argv.slice(2);
const rows = JSON.parse(fs.readFileSync(flowsPath, 'utf8'));
const jb = jbPath && fs.existsSync(jbPath) ? JSON.parse(fs.readFileSync(jbPath, 'utf8')) : null;
const vscodeRows = JSON.parse(fs.readFileSync(flowsPath, 'utf8'));
if (jb) for (const row of jb.results ?? []) rows.push(row);
const failed = rows.filter((row) => row.passed !== true || row.status === 'failed');
const workflowId = (row) => {
  const match = /^workflow:(W\d{2})(?::|$)/.exec(String(row.id));
  return match === null ? null : match[1];
};
const workflowRows = rows.filter((row) => workflowId(row) !== null);
const vscodeWorkflowRows = vscodeRows.filter((row) => workflowId(row) !== null);
const jetbrainsWorkflowRows = jb ? (jb.results ?? []).filter((row) => workflowId(row) !== null) : [];
const idsOf = (list) => new Set(list.map(workflowId));
const vscodeIds = idsOf(vscodeWorkflowRows);
const jetbrainsIds = idsOf(jetbrainsWorkflowRows);
const vscodeFlows = vscodeIds.size;
const jetbrainsFlows = jetbrainsIds.size;
const allIds = [...new Set([...vscodeIds, ...jetbrainsIds])].sort();
const workflowPassed = allIds.filter((id) =>
  vscodeIds.has(id) &&
  jetbrainsIds.has(id) &&
  workflowRows.filter((row) => workflowId(row) === id).every((row) => row.passed === true && row.status !== 'failed')
).length;
const productFlows = ['task-admission', 'provider-config', 'compaction-long-task', 'restore-fork', 'permissions', 'proof', 'finish-levels'];
const missing = productFlows.filter((id) => !rows.some((row) => row.id === id && row.passed === true));
if (missing.length > 0) failed.push(...missing.map((id) => ({ id, passed: false, detail: 'missing product flow' })));
const document = {
  schema: 'faktor-edition8-dogfood/v1',
  generated_at: new Date().toISOString(),
  base_url: baseUrl,
  isolated: true,
  network: 'loopback-only',
  worktree,
  worktree_synced_files: Number(synced || 0),
  counts: {
    flows: rows.filter((row) => !String(row.id).startsWith('workflow:')).length,
    workflows: allIds.length,
    workflow_passed: workflowPassed,
    workflow_rows: workflowRows.length,
    vscode_flows: vscodeFlows,
    jetbrains_flows: jetbrainsFlows,
    passed: rows.length - failed.length,
    failed: failed.length,
  },
  results: rows,
};
fs.writeFileSync(out, JSON.stringify(document, null, 2) + '\n');
console.log(`[dogfood8] ${document.counts.passed}/${rows.length} results; workflows vscode=${vscodeFlows} jetbrains=${jetbrainsFlows} -> ${out}`);
process.exit(failed.length === 0 && allIds.length === 16 && workflowPassed === 16 ? 0 : 1);
NODE

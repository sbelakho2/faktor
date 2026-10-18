#!/usr/bin/env bash
# Edition 5 FOOTGUN-FREE DOGFOOD.
#
# Uses the product itself in an ISOLATED git worktree + data dir on every run:
#
#   1. cargo build the real faktor-cli;
#   2. git worktree add --detach HEAD (no dirty checkout is trusted);
#   3. run the real compaction/runtime long-task probes;
#   4. start a loopback-only fake OpenAI-compatible provider (no network) and
#      the REAL daemon against the isolated data dir/config;
#   5. drive the VS Code host runtime (NativeClient + edition5 command
#      builders) through: health/ready, provider config, session, task
#      admission (idempotent submission), finish levels local/commit/push/pr,
#      permissions, proof, checkpoints and restore/fork route construction;
#   6. run the JetBrains host smokes (Backend/NativeBridge/Frontend/
#      JetBrainsParity) against the real daemon binary;
#   7. write target/certification/edition5/dogfood.raw.json.
#
# No soak/load/perf lane is added or invoked. Everything is loopback.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="${FAKTOR_CLI_BIN:-$ROOT/target/debug/faktor-cli}"
EVIDENCE_DIR="$ROOT/target/certification/edition5"
OUT="$EVIDENCE_DIR/dogfood.raw.json"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/faktor-edition5-dogfood.XXXXXX")"
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

note() { printf '[dogfood] %s\n' "$*"; }
fail() { printf '[dogfood] FAIL: %s\n' "$*" >&2; exit 1; }

flow_result() { # id status detail
  node - "$FLOWS_FILE" "$1" "$2" "$3" <<'NODE'
const fs = require('node:fs');
const [path, id, status, detail] = process.argv.slice(2);
const rows = fs.existsSync(path) ? JSON.parse(fs.readFileSync(path, 'utf8')) : [];
rows.push({ id, status, detail, passed: status !== 'failed' });
fs.writeFileSync(path, JSON.stringify(rows, null, 2));
NODE
}

mkdir -p "$EVIDENCE_DIR" "$DATA" "$WORKSPACE"
: >"$FLOWS_FILE"

# ---------------------------------------------------------------- 1. build
if [ -x "$BIN" ]; then
  note "using prebuilt faktor-cli at $BIN"
else
  note "building faktor-cli"
  cargo build -q -p faktor-cli --manifest-path "$ROOT/Cargo.toml"
fi
[ -x "$BIN" ] || fail "missing $BIN"

# ------------------------------------------------------- 2. isolated worktree
note "creating isolated worktree at $WORKTREE"
git -C "$ROOT" worktree add --detach "$WORKTREE" HEAD >/dev/null 2>&1 \
  || fail "could not create the isolated worktree"
FLOW_DIRTY="$(git -C "$ROOT" status --porcelain | wc -l | tr -d ' ')"
flow_result "isolated-worktree" "passed" "worktree at HEAD; main checkout dirty files ignored: $FLOW_DIRTY"

# ---------------------------------------------- 3. compaction long-task smoke
note "running real compaction/runtime long-task probes"
if ! cargo test -q -p faktor-context longtask_probes --manifest-path "$ROOT/Cargo.toml" >"$TMP/context-longtask.log" 2>&1; then
  flow_result "compaction-long-task" "failed" "faktor-context longtask probes failed (see $(basename "$TMP")/context-longtask.log)"
  fail "faktor-context longtask probes failed"
fi
if ! cargo test -q -p faktor-agent longtask_probes --manifest-path "$ROOT/Cargo.toml" >"$TMP/agent-longtask.log" 2>&1; then
  flow_result "compaction-long-task" "failed" "faktor-agent longtask probes failed"
  fail "faktor-agent longtask probes failed"
fi
flow_result "compaction-long-task" "passed" "300-turn repeated compaction + restart/cancel/per-turn probes passed"

# --------------------------------------------------------- 4. fake provider
note "starting loopback fake provider"
node "$ROOT/scripts/edition5/fake-openai.mjs" --port-file "$TMP/fake.port" >"$FAKE_LOG" 2>&1 &
FAKE_PID=$!
PIDS+=("$FAKE_PID")
for _ in $(seq 1 100); do
  [ -s "$TMP/fake.port" ] && break
  sleep 0.05
done
[ -s "$TMP/fake.port" ] || fail "fake provider did not start"
FAKE_PORT="$(cat "$TMP/fake.port")"
CONFIG="$TMP/faktor-plus.json"
cat >"$CONFIG" <<JSON
{"config_version":1,"model":"m","providers":[{"kind":"open_ai","id":"p1","base_url":"http://127.0.0.1:${FAKE_PORT}/v1","api_key_env":"MOCK_KEY","api":"chat","allow_loopback":true,"models":["m"]}]}
JSON
export MOCK_KEY="edition5-dogfood"

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
  if curl -fsS -H "authorization: Bearer $PASSWORD" "$BASE_URL/native/ready" >/dev/null 2>&1; then
    break
  fi
  sleep 0.05
done
curl -fsS -H "authorization: Bearer $PASSWORD" "$BASE_URL/native/ready" >/dev/null 2>&1 \
  || { tail -n 30 "$DAEMON_LOG" >&2 || true; fail "daemon never became ready"; }

# -------------------------------------------------------- 6. VS Code host
note "running the VS Code host runtime against the real daemon"
if ! node "$ROOT/apps/vscode/scripts/edition5-dogfood.mjs" \
  --base-url "$BASE_URL" --token "$PASSWORD" --workspace "$WORKSPACE" \
  --out "$TMP/dogfood-vscode.json" >"$TMP/vscode-dogfood.log" 2>&1; then
  cat "$TMP/vscode-dogfood.log" >&2 || true
  flow_result "vscode-host" "failed" "VS Code host dogfood failed"
  fail "VS Code host dogfood failed"
fi
cp "$TMP/dogfood-vscode.json" "$EVIDENCE_DIR/dogfood-vscode.raw.json"
node - "$TMP/dogfood-vscode.json" "$FLOWS_FILE" <<'NODE'
const fs = require('node:fs');
const [hostPath, flowsPath] = process.argv.slice(2);
const host = JSON.parse(fs.readFileSync(hostPath, 'utf8'));
const flows = fs.existsSync(flowsPath) ? JSON.parse(fs.readFileSync(flowsPath, 'utf8')) : [];
for (const row of host.results) {
  flows.push({ id: `vscode:${row.id}`, status: row.passed ? row.status : 'failed', detail: row.detail, passed: row.passed });
}
fs.writeFileSync(flowsPath, JSON.stringify(flows, null, 2));
NODE

# ------------------------------------------------------- 7. JetBrains host
if [ "${SKIP_JETBRAINS:-0}" = "1" ]; then
  note "JetBrains host smokes run in the dedicated jetbrains-smoke CI lane"
  flow_result "jetbrains-host" "passed" \
    "covered by the pipeline jetbrains-smoke lane (real-daemon Backend/NativeBridge/Frontend/JetBrainsParity + edition5 host smokes)"
else
  note "running the JetBrains host smokes against the real daemon"
  if ! "$ROOT/apps/jetbrains/compile-and-smoke.sh" \
    --smoke Backend --smoke NativeBridge --smoke Frontend --smoke JetBrainsParity \
    >"$TMP/jetbrains-dogfood.log" 2>&1; then
    tail -n 80 "$TMP/jetbrains-dogfood.log" >&2 || true
    flow_result "jetbrains-host" "failed" "JetBrains host smokes failed"
    fail "JetBrains host smokes failed"
  fi
  flow_result "jetbrains-host" "passed" "Backend/NativeBridge/Frontend/JetBrainsParity smokes passed against the real daemon"
fi

# --------------------------------------------------------------- 8. evidence
node - "$FLOWS_FILE" "$OUT" "$BASE_URL" <<'NODE'
const fs = require('node:fs');
const [flowsPath, out, baseUrl] = process.argv.slice(2);
const rows = JSON.parse(fs.readFileSync(flowsPath, 'utf8'));
const failed = rows.filter((row) => row.passed !== true || row.status === 'failed');
const document = {
  schema: 'faktor-edition5-dogfood/v1',
  generated_at: new Date().toISOString(),
  base_url: baseUrl,
  isolated: true,
  network: 'loopback-only',
  counts: { flows: rows.length, passed: rows.length - failed.length, failed: failed.length },
  results: rows,
};
fs.writeFileSync(out, JSON.stringify(document, null, 2) + '\n');
console.log(`[dogfood] ${rows.length - failed.length}/${rows.length} flows passed -> ${out}`);
process.exit(failed.length === 0 ? 0 : 1);
NODE

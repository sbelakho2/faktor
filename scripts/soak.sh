#!/usr/bin/env bash
# Faktor accelerated release soak driver.
#
# Runs the repository's existing `#[ignore]`d `[soak]` / `[fault]` / `[perf]`
# campaigns with a CONFIGURABLE, BOUNDED duration/scale multiplier and records
# a machine-readable result at `target/certification/soak.json`.
#
# Semantics:
#   * The selected ignored-test groups are run once per campaign ROUNDS where
#     `rounds = ceil(min(FAKTOR_SOAK_SCALE, FAKTOR_SOAK_MAX_SCALE))`, bounded
#     further by `FAKTOR_SOAK_MAX_ROUNDS` and the whole-run wall budget
#     `FAKTOR_SOAK_MAX_WALL_SECONDS`. `smoke` mode pins rounds to 1 (the
#     always-fast CI gate); `long` mode applies the scale (the scheduled
#     release campaign).
#   * The default is accelerated: `FAKTOR_SOAK_SCALE=1`, `smoke` mode.
#   * A run that executes NO test is a FAILURE (no-op refusal). A zero,
#     negative or unparseable scale is refused before any test runs. A
#     non-positive wall/test budget is refused. The campaign never runs
#     unbounded: per-invocation `timeout` plus a checked global wall budget.
#   * `FAKTOR_SOAK_SCALE` is also exported to each test process so a future
#     scale-aware test can read the same multiplier.
#
# Modes of selection:
#   * Default groups (both modes): run the exact ignored sets of the `soak`,
#     `fault` and `perf` lanes from `scripts/certification/ignored-tests.json`.
#     In `smoke` mode each group runs ONE representative ignored test with
#     `--exact` (bounded CI smoke); in `long` mode the whole ignored set runs.
#   * Custom cargo args: `bash scripts/soak.sh -p <pkg> --release -- --ignored`
#     forwards everything from the first cargo option/`--` verbatim to
#     `cargo test` (this is how the nightly `soak` lane keeps the registry's
#     exact lane command while adding scale/rounds).
#
# Env:
#   FAKTOR_SOAK_SCALE               float > 0, default 1 (multiplier)
#   FAKTOR_SOAK_MODE                smoke|long, default smoke
#   FAKTOR_SOAK_GROUPS              csv subset of soak,fault,perf
#   FAKTOR_SOAK_MAX_SCALE           default 8 (scale clamp, recorded)
#   FAKTOR_SOAK_MAX_ROUNDS          default 8
#   FAKTOR_SOAK_MAX_WALL_SECONDS    default 1800 (smoke) / 21600 (long)
#   FAKTOR_SOAK_TEST_TIMEOUT_SECONDS default 1200 (smoke) / 3600 (long)
#   FAKTOR_SOAK_CARGO               cargo binary override
#   FAKTOR_SOAK_OUT_DIR             default <repo>/target/certification
#
# Usage:
#   bash scripts/soak.sh [--mode smoke|long] [--groups soak,fault,perf]
#                        [--scale N] [--max-rounds N] [--max-wall-seconds N]
#                        [--test-timeout-seconds N] [--cargo-bin PATH]
#                        [--out-dir DIR] [--print-plan] [--selftest]
#                        [CARGO TEST ARGS...]
#
# Exit codes: 0 passed; 1 a campaign/refusal failure; 2 usage error.
set -uo pipefail

SELF="${BASH_SOURCE[0]}"
ROOT="$(cd "$(dirname "$SELF")/.." && pwd)"

MODE="${FAKTOR_SOAK_MODE:-smoke}"
GROUPS_RAW="${FAKTOR_SOAK_GROUPS:-soak,fault,perf}"
SCALE_RAW="${FAKTOR_SOAK_SCALE:-1}"
MAX_SCALE_RAW="${FAKTOR_SOAK_MAX_SCALE:-8}"
MAX_ROUNDS_RAW="${FAKTOR_SOAK_MAX_ROUNDS:-8}"
OUT_DIR="${FAKTOR_SOAK_OUT_DIR:-$ROOT/target/certification}"
CARGO_BIN="${FAKTOR_SOAK_CARGO:-cargo}"
MAX_WALL_RAW="${FAKTOR_SOAK_MAX_WALL_SECONDS:-}"
TEST_TIMEOUT_RAW="${FAKTOR_SOAK_TEST_TIMEOUT_SECONDS:-}"
SELFTEST=0
PRINT_PLAN=0
CARGO_ARGS=()

usage() {
  sed -n '2,/^set -/p' "$SELF" | sed '$d' | sed 's/^# \{0,1\}//'
}

die_usage() {
  echo "soak: $1" >&2
  usage >&2
  exit 2
}

# Cargo-style options flip the parser into verbatim-forwarding mode so the
# registry lane command can pass through unchanged.
is_cargo_arg() {
  case "$1" in
  -p | --package | -- | --release | --debug | --test | --lib | --bins | \
    --bin | --example | --examples | --all-targets | --workspace | --exclude | \
    --features | --no-default-features | --all-features | --jobs | -j) return 0 ;;
  *) return 1 ;;
  esac
}

while [ "$#" -gt 0 ]; do
  if is_cargo_arg "$1"; then
    CARGO_ARGS=("$@")
    break
  fi
  case "$1" in
  --selftest) SELFTEST=1; shift ;;
  --print-plan) PRINT_PLAN=1; shift ;;
  --mode | --groups | --scale | --max-rounds | --max-wall-seconds | --test-timeout-seconds | --cargo-bin | --out-dir)
    [ "$#" -ge 2 ] || die_usage "$1 needs a value"
    case "$1" in
    --mode) MODE="$2" ;;
    --groups) GROUPS_RAW="$2" ;;
    --scale) SCALE_RAW="$2" ;;
    --max-rounds) MAX_ROUNDS_RAW="$2" ;;
    --max-wall-seconds) MAX_WALL_RAW="$2" ;;
    --test-timeout-seconds) TEST_TIMEOUT_RAW="$2" ;;
    --cargo-bin) CARGO_BIN="$2" ;;
    --out-dir) OUT_DIR="$2" ;;
    esac
    shift 2
    ;;
  -h | --help)
    usage
    exit 0
    ;;
  *)
    die_usage "unknown argument: $1"
    ;;
  esac
done

is_positive_number() {
  awk -v v="$1" 'BEGIN {
    if (v ~ /^[0-9]+(\.[0-9]+)?$/ && v + 0 > 0) exit 0;
    exit 1;
  }'
}

is_nonneg_integer() {
  case "$1" in
  '' | *[!0-9]*) return 1 ;;
  *) return 0 ;;
  esac
}

json_escape() {
  printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g' -e 's/\t/\\t/g' | tr -d '\r\n'
}

iso_now() { date -u +%Y-%m-%dT%H:%M:%SZ; }

# ------------------------------------------------------------- group catalog
# Each group carries the exact ignored-set command used by its nightly/trusted
# lane plus one representative `--exact` smoke test. The representative smoke
# keeps the default CI gate bounded while still executing a real `#[ignore]`d
# test from the group (a repo with no ignored tests fails as a no-op).
group_packages() {
  case "$1" in
  soak) printf '%s' 'faktor-tests-accounting-modelcheck' ;;
  fault) printf '%s' 'faktor-tests-fault -p faktor-updater' ;;
  perf) printf '%s' 'faktor-tests-performance' ;;
  *) return 1 ;;
  esac
}

group_smoke_exact() {
  case "$1" in
  soak) printf '%s' 'durable_reference_comparison_full' ;;
  fault) printf '%s' 'accounting_campaign_full' ;;
  perf) printf '%s' 'cold_start_under_150ms' ;;
  *) return 1 ;;
  esac
}

group_command() { # group mode
  local group="$1" mode="$2" packages
  packages="$(group_packages "$group")" || return 1
  if [ "$mode" = smoke ]; then
    printf 'cargo test -p %s --release -- --ignored --exact %s' \
      "$packages" "$(group_smoke_exact "$group")"
  else
    printf 'cargo test -p %s --release -- --ignored' "$packages"
  fi
}

normalize_groups() {
  local raw="$1" out="" group
  local old_ifs="$IFS"
  IFS=','
  for group in $raw; do
    IFS="$old_ifs"
    case "$group" in
    soak | fault | perf) ;;
    *) echo "soak: invalid group '$group' (expected soak,fault,perf)" >&2; return 1 ;;
    esac
    case ",$out," in
    *",$group,"*) ;;
    *) out="${out:+$out,}$group" ;;
    esac
    IFS=','
  done
  IFS="$old_ifs"
  [ -n "$out" ] || { echo "soak: no groups selected" >&2; return 1; }
  # canonical order
  local canon="" g
  for g in soak fault perf; do
    case ",$out," in
    *",$g,"*) canon="${canon:+$canon,}$g" ;;
    esac
  done
  printf '%s' "$canon"
}

# ------------------------------------------------------------------ planning
PLAN_GROUPS=""
PLAN_COMMANDS="" # newline-separated "group<TAB>command"

build_plan() {
  PLAN_GROUPS="$(normalize_groups "$GROUPS_RAW")" || return 1
  PLAN_COMMANDS=""
  local group cmd
  if [ "${#CARGO_ARGS[@]}" -gt 0 ]; then
    cmd="cargo test ${CARGO_ARGS[*]}"
    PLAN_GROUPS="custom"
    PLAN_COMMANDS="custom	$cmd"
    return 0
  fi
  local old_ifs="$IFS"
  IFS=','
  for group in $PLAN_GROUPS; do
    IFS="$old_ifs"
    cmd="$(group_command "$group" "$MODE")" || return 1
    PLAN_COMMANDS="${PLAN_COMMANDS}${PLAN_COMMANDS:+
}$group	$cmd"
    IFS=','
  done
  IFS="$old_ifs"
  return 0
}

# ------------------------------------------------------------------ JSON out
OUT_FILE=""
TMP_OUT=""
write_record() { # status reason started finished duration executed passed failed
  [ -n "$OUT_FILE" ] || return 0
  mkdir -p "$OUT_DIR"
  local status="$1" reason="$2" started="$3" finished="$4" duration="$5"
  local executed="$6" passed="$7" failed="$8"
  local groups_json="[]" logs_json="[]"
  if [ -n "${GROUP_JSON:-}" ]; then
    groups_json="[${GROUP_JSON%,}]"
  fi
  if [ -n "${LOG_JSON:-}" ]; then
    logs_json="[${LOG_JSON%,}]"
  fi
  local scale_clamped_json=false
  [ "${SCALE_CLAMPED:-0}" = "1" ] && scale_clamped_json=true
  local rounds_capped_json=false
  [ "${ROUNDS_CAPPED:-0}" = "1" ] && rounds_capped_json=true
  TMP_OUT="$OUT_FILE.tmp.$$"
  printf '{"schema":"faktor-soak/v1","lane":"soak","status":"%s","reason":"%s","mode":"%s","commit":"%s","tree":"%s","runner":{"os":"%s","arch":"%s"},"started_at":"%s","finished_at":"%s","duration_seconds":%s,"scale_requested":%s,"scale_effective":%s,"max_scale":%s,"scale_clamped":%s,"rounds":%s,"max_rounds":%s,"rounds_capped":%s,"max_wall_seconds":%s,"test_timeout_seconds":%s,"bounded":true,"executed_total":%s,"passed_total":%s,"failed_total":%s,"groups":%s,"logs":%s}\n' \
    "$(json_escape "$status")" "$(json_escape "$reason")" "$(json_escape "$MODE")" \
    "$(json_escape "$COMMIT")" "$(json_escape "$TREE")" \
    "$(uname -s | tr '[:upper:]' '[:lower:]')" "$(uname -m)" \
    "$started" "$finished" "$duration" \
    "${SCALE_REQUESTED:-0}" "${SCALE_EFFECTIVE:-0}" "${MAX_SCALE:-0}" "$scale_clamped_json" \
    "${ROUNDS:-0}" "${MAX_ROUNDS:-0}" "$rounds_capped_json" \
    "${MAX_WALL:-0}" "${TEST_TIMEOUT:-0}" \
    "$executed" "$passed" "$failed" "$groups_json" "$logs_json" >"$TMP_OUT"
  mv "$TMP_OUT" "$OUT_FILE"
  echo "soak: recorded $OUT_FILE (status=$status reason=${reason:-none})"
}

# ------------------------------------------------------------------ campaign
parse_counts() { # log -> "executed passed failed"
  awk '
    /test result:/ {
      for (i = 1; i <= NF; i++) {
        if ($i == "passed;") passed += $(i - 1);
        else if ($i == "failed;") failed += $(i - 1);
      }
    }
    END { printf "%d %d %d\n", passed + failed, passed, failed }
  ' "$1"
}

run_campaign() {
  mkdir -p "$OUT_DIR/soak-logs"
  OUT_FILE="$OUT_DIR/soak.json"
  LOG_DIR="$OUT_DIR/soak-logs"

  GROUP_JSON=""
  LOG_JSON=""
  local started finished start_epoch now_epoch duration
  started="$(iso_now)"
  start_epoch="$(date +%s)"
  COMMIT="unknown"
  TREE="unknown"
  if command -v git >/dev/null 2>&1 && git -C "$ROOT" rev-parse --verify HEAD >/dev/null 2>&1; then
    COMMIT="$(git -C "$ROOT" rev-parse HEAD 2>/dev/null || printf 'unknown')"
    TREE="$(git -C "$ROOT" rev-parse 'HEAD^{tree}' 2>/dev/null || printf 'unknown')"
  fi
  : "${COMMIT:=unknown}"
  : "${TREE:=unknown}"

  local executed_total=0 passed_total=0 failed_total=0
  local rounds=1 round=1 rc=0 reason=""

  if [ "${#CARGO_ARGS[@]}" -gt 0 ]; then
    # Custom cargo args mode: one logical command, repeated per round.
    local cmd="cargo test ${CARGO_ARGS[*]}"
    local exec_count=0 pass_count=0 fail_count=0
    while [ "$round" -le "$ROUNDS" ]; do
      now_epoch="$(date +%s)"
      if [ $((now_epoch - start_epoch)) -ge "$MAX_WALL" ]; then
        reason="wall-budget: campaign exceeded ${MAX_WALL}s before round $round"
        rc=1
        break
      fi
      local log="$LOG_DIR/round${round}-custom.log"
      run_command "$cmd" "$log"
      local group_rc=$?
      local counts
      counts="$(parse_counts "$log")"
      read -r exec_count pass_count fail_count <<<"$counts"
      executed_total=$((executed_total + exec_count))
      passed_total=$((passed_total + pass_count))
      failed_total=$((failed_total + fail_count))
      LOG_JSON="${LOG_JSON}\"$(json_escape "$log")\","
      if [ "$group_rc" -eq 124 ]; then
        reason="wall-budget: round $round exceeded the ${MAX_WALL}s budget ($cmd)"
        rc=1
        break
      fi
      if [ "$group_rc" -ne 0 ]; then
        reason="command-failed: round $round rc=$group_rc ($cmd)"
        rc=1
        break
      fi
      if [ "$exec_count" -eq 0 ]; then
        reason="no-op: round $round executed 0 tests ($cmd)"
        rc=1
        break
      fi
      round=$((round + 1))
    done
    GROUP_JSON="${GROUP_JSON}{\"name\":\"custom\",\"command\":\"$(json_escape "$cmd")\",\"rounds\":$((round - 1)),\"executed\":$executed_total,\"passed\":$passed_total,\"failed\":$failed_total},"
  else
    local group cmd log group_rc counts exec_count pass_count fail_count
    local old_ifs="$IFS"
    while [ "$round" -le "$ROUNDS" ]; do
      now_epoch="$(date +%s)"
      if [ $((now_epoch - start_epoch)) -ge "$MAX_WALL" ]; then
        reason="wall-budget: campaign exceeded ${MAX_WALL}s before round $round"
        rc=1
        break
      fi
      IFS=','
      for group in $PLAN_GROUPS; do
        IFS="$old_ifs"
        cmd="$(group_command "$group" "$MODE")" || { reason="invalid-group: $group"; rc=1; break; }
        log="$LOG_DIR/round${round}-${group}.log"
        run_command "$cmd" "$log"
        group_rc=$?
        counts="$(parse_counts "$log")"
        read -r exec_count pass_count fail_count <<<"$counts"
        executed_total=$((executed_total + exec_count))
        passed_total=$((passed_total + pass_count))
        failed_total=$((failed_total + fail_count))
        LOG_JSON="${LOG_JSON}\"$(json_escape "$log")\","
        GROUP_JSON="${GROUP_JSON}{\"name\":\"$(json_escape "$group")\",\"command\":\"$(json_escape "$cmd")\",\"log\":\"$(json_escape "$log")\",\"round\":$round,\"executed\":$exec_count,\"passed\":$pass_count,\"failed\":$fail_count},"
        if [ "$group_rc" -eq 124 ]; then
          reason="wall-budget: round $round group $group exceeded the ${MAX_WALL}s budget"
          rc=1
          break
        fi
        if [ "$group_rc" -ne 0 ]; then
          reason="command-failed: round $round group $group rc=$group_rc"
          rc=1
          break
        fi
        if [ "$exec_count" -eq 0 ]; then
          reason="no-op: round $round group $group executed 0 tests"
          rc=1
          break
        fi
        IFS=','
      done
      IFS="$old_ifs"
      [ "$rc" -eq 0 ] || break
      round=$((round + 1))
    done
    IFS="$old_ifs"
  fi

  finished="$(iso_now)"
  now_epoch="$(date +%s)"
  duration=$((now_epoch - start_epoch))
  if [ -z "$reason" ] && [ "$executed_total" -eq 0 ]; then
    reason="no-op: campaign executed 0 tests"
    rc=1
  fi
  if [ "$rc" -eq 0 ] && [ "$duration" -gt "$MAX_WALL" ]; then
    reason="wall-budget: campaign ran ${duration}s over the ${MAX_WALL}s budget"
    rc=1
  fi
  if [ "$rc" -eq 0 ]; then
    write_record passed "" "$started" "$finished" "$duration" "$executed_total" "$passed_total" "$failed_total"
  else
    write_record failed "$reason" "$started" "$finished" "$duration" "$executed_total" "$passed_total" "$failed_total"
  fi
  return "$rc"
}

# Runs one cargo test command with a per-invocation timeout bounded by the
# REMAINING wall budget, tees output to $2, and returns the cargo exit status
# (124 = timeout / budget exhausted).
run_command() {
  local cmd="$1" log="$2"
  local -a argv
  if [ "${#CARGO_ARGS[@]}" -gt 0 ] && [ "$cmd" = "cargo test ${CARGO_ARGS[*]}" ]; then
    argv=("$CARGO_BIN" test "${CARGO_ARGS[@]}")
  else
    # shellcheck disable=SC2206 # word splitting is intentional for catalog commands
    argv=("$CARGO_BIN" test ${cmd#cargo test })
  fi
  echo "soak: round=$round $cmd" >&2
  # Per-invocation bound: the smaller of the test timeout and what is left of
  # the wall budget. The budget is thus enforced WITHIN a round, not only
  # between rounds (smoke mode runs a single round). A missing `timeout`
  # binary is refused up front, so this branch always bounds the process.
  local remaining=$((MAX_WALL - ($(date +%s) - start_epoch)))
  if [ "$remaining" -le 0 ]; then
    echo "soak: wall budget exhausted before: $cmd" >&2
    : >"$log"
    return 124
  fi
  local bound="$TEST_TIMEOUT"
  if [ "$bound" -gt "$remaining" ]; then
    bound="$remaining"
  fi
  echo "soak: bound=${bound}s (test-timeout=${TEST_TIMEOUT}s remaining-wall=${remaining}s)" >&2
  # Scale-awareness hook: the effective multiplier and mode are visible to the
  # test processes as well as to the driver.
  FAKTOR_SOAK_SCALE="$SCALE_EFFECTIVE" FAKTOR_SOAK_MODE="$MODE" \
    "$TIMEOUT_BIN" "$bound" "${argv[@]}" 2>&1 | tee "$log"
  return "${PIPESTATUS[0]}"
}

# ------------------------------------------------------------------ selftest
selftest() {
  local tmp rc=0 failures=0
  tmp="$(mktemp -d "${TMPDIR:-/tmp}/faktor-soak-selftest.XXXXXX")"
  trap 'rm -rf "${tmp:-}"' EXIT
  cat >"$tmp/fake-cargo" <<'FAKE'
#!/bin/sh
printf '%s\n' "$*" >>"${FAKE_CARGO_LOG:-/dev/null}"
printf 'FAKTOR_SOAK_SCALE=%s FAKTOR_SOAK_MODE=%s\n' "${FAKTOR_SOAK_SCALE:-unset}" "${FAKTOR_SOAK_MODE:-unset}" >>"${FAKE_CARGO_LOG:-/dev/null}"
case "${FAKE_CARGO_MODE:-ok}" in
ok) printf 'running 2 tests\ntest result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n' ;;
zero) printf 'running 0 tests\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n' ;;
fail) printf 'test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n'; exit 101 ;;
sleep) printf 'running 2 tests\n'; sleep 30 ;;
esac
exit 0
FAKE
  chmod +x "$tmp/fake-cargo"
  # Hosts without coreutils `timeout` still exercise the selftest through a
  # passthrough shim; production refuses to run unbounded (checked below).
  if ! command -v timeout >/dev/null 2>&1; then
    cat >"$tmp/timeout" <<'SHIM'
#!/bin/sh
shift
exec "$@"
SHIM
    chmod +x "$tmp/timeout"
    PATH="$tmp:$PATH"
    export PATH
  fi

  field() { # file key
    grep -o "\"$2\":[^,}]*" "$1" 2>/dev/null | head -n 1 | sed 's/^[^:]*://' | tr -d '"'
  }
  contains() { # file text
    grep -q "$2" "$1" 2>/dev/null
  }
  check() { # label condition-result
    if [ "$2" = "0" ]; then
      echo "soak selftest ok: $1"
    else
      echo "soak selftest FAIL: $1" >&2
      failures=$((failures + 1))
    fi
  }

  # 1. happy accelerated smoke run
  out="$tmp/out-ok"
  FAKE_CARGO_LOG="$tmp/fake-ok.log" FAKE_CARGO_MODE=ok \
    bash "$SELF" --mode smoke --groups soak --cargo-bin "$tmp/fake-cargo" --out-dir "$out" \
    >"$tmp/ok.stdout" 2>"$tmp/ok.stderr"
  rc=$?
  check "accelerated smoke passes" "$([ "$rc" -eq 0 ] && echo 0 || echo 1)"
  check "status passed" "$([ "$(field "$out/soak.json" status)" = passed ] && echo 0 || echo 1)"
  check "executed_total=2" "$([ "$(field "$out/soak.json" executed_total)" = 2 ] && echo 0 || echo 1)"
  check "no-op record is not empty" "$([ -s "$out/soak.json" ] && echo 0 || echo 1)"
  check "scale exported to tests" "$(grep -q 'FAKTOR_SOAK_SCALE=1 FAKTOR_SOAK_MODE=smoke' "$tmp/fake-ok.log" && echo 0 || echo 1)"

  # 2. zero scale is refused before any test runs
  out="$tmp/out-zero-scale"
  FAKE_CARGO_LOG="$tmp/fake-zero-scale.log" FAKE_CARGO_MODE=ok \
    bash "$SELF" --mode smoke --groups soak --scale 0 --cargo-bin "$tmp/fake-cargo" --out-dir "$out" \
    >"$tmp/zs.stdout" 2>"$tmp/zs.stderr"
  rc=$?
  check "zero scale refused" "$([ "$rc" -eq 1 ] && echo 0 || echo 1)"
  check "zero scale reason" "$([ "$(field "$out/soak.json" status)" = failed ] && contains "$out/soak.json" 'zero' && echo 0 || echo 1)"
  check "zero scale ran no test" "$([ ! -s "$tmp/fake-zero-scale.log" ] && echo 0 || echo 1)"

  # 3. a run that executes no test fails
  out="$tmp/out-noop"
  FAKE_CARGO_LOG="$tmp/fake-noop.log" FAKE_CARGO_MODE=zero \
    bash "$SELF" --mode smoke --groups soak --cargo-bin "$tmp/fake-cargo" --out-dir "$out" \
    >"$tmp/noop.stdout" 2>"$tmp/noop.stderr"
  rc=$?
  check "no-op run fails" "$([ "$rc" -eq 1 ] && echo 0 || echo 1)"
  check "no-op reason" "$(contains "$out/soak.json" 'no-op' && echo 0 || echo 1)"

  # 4. an oversized scale is clamped to the recorded maximum
  out="$tmp/out-clamped"
  FAKE_CARGO_LOG="$tmp/fake-clamped.log" FAKE_CARGO_MODE=ok \
    bash "$SELF" --mode long --groups soak --scale 999 --cargo-bin "$tmp/fake-cargo" --out-dir "$out" \
    >"$tmp/clamp.stdout" 2>"$tmp/clamp.stderr"
  rc=$?
  check "oversized scale still passes" "$([ "$rc" -eq 0 ] && echo 0 || echo 1)"
  check "scale clamped to max" "$([ "$(field "$out/soak.json" scale_effective)" = 8 ] && echo 0 || echo 1)"
  check "clamped flag recorded" "$(contains "$out/soak.json" '"scale_clamped":true' && echo 0 || echo 1)"

  # 5. invalid scale is a refusal
  out="$tmp/out-bad-scale"
  FAKE_CARGO_LOG="$tmp/fake-bad.log" FAKE_CARGO_MODE=ok \
    bash "$SELF" --mode smoke --groups soak --scale abc --cargo-bin "$tmp/fake-cargo" --out-dir "$out" \
    >"$tmp/bad.stdout" 2>"$tmp/bad.stderr"
  rc=$?
  check "invalid scale refused" "$([ "$rc" -eq 1 ] && contains "$out/soak.json" 'invalid-scale' && echo 0 || echo 1)"

  # 6. a failing campaign is recorded and fails
  out="$tmp/out-fail"
  FAKE_CARGO_LOG="$tmp/fake-fail.log" FAKE_CARGO_MODE=fail \
    bash "$SELF" --mode smoke --groups soak --cargo-bin "$tmp/fake-cargo" --out-dir "$out" \
    >"$tmp/fail.stdout" 2>"$tmp/fail.stderr"
  rc=$?
  check "failing campaign fails" "$([ "$rc" -eq 1 ] && echo 0 || echo 1)"
  check "failure count recorded" "$([ "$(field "$out/soak.json" failed_total)" = 1 ] && echo 0 || echo 1)"

  # 7. print-plan does not run or write anything
  out="$tmp/out-plan"
  FAKE_CARGO_LOG="$tmp/fake-plan.log" FAKE_CARGO_MODE=ok \
    bash "$SELF" --print-plan --mode long --groups soak --scale 4 --cargo-bin "$tmp/fake-cargo" --out-dir "$out" \
    >"$tmp/plan.stdout" 2>"$tmp/plan.stderr"
  rc=$?
  check "print-plan exits 0" "$([ "$rc" -eq 0 ] && echo 0 || echo 1)"
  check "print-plan runs nothing" "$([ ! -e "$out/soak.json" ] && [ ! -s "$tmp/fake-plan.log" ] && echo 0 || echo 1)"
  check "print-plan reports rounds" "$(grep -q '"rounds":4' "$tmp/plan.stdout" && echo 0 || echo 1)"

  # 8. a hung group is killed by the per-invocation bound and fails as a
  # wall-budget breach (smoke has ROUNDS=1, so this proves the bound runs
  # WITHIN a round and that an over-budget run never records passed)
  out="$tmp/out-timeout"
  FAKE_CARGO_LOG="$tmp/fake-timeout.log" FAKE_CARGO_MODE=sleep \
    bash "$SELF" --mode smoke --groups soak --max-wall-seconds 300 --test-timeout-seconds 1 \
    --cargo-bin "$tmp/fake-cargo" --out-dir "$out" >"$tmp/to.stdout" 2>"$tmp/to.stderr"
  rc=$?
  check "invocation bound kills a hung group" "$([ "$rc" -eq 1 ] && echo 0 || echo 1)"
  check "wall-budget reason recorded" "$(contains "$out/soak.json" 'wall-budget' && echo 0 || echo 1)"
  check "over-budget run never records passed" "$([ "$(field "$out/soak.json" status)" = failed ] && echo 0 || echo 1)"

  if [ "$failures" -gt 0 ]; then
    echo "soak selftest: FAIL ($failures case(s))" >&2
    return 1
  fi
  echo "soak selftest: PASS (scale bounds, zero-duration, no-op and per-invocation wall-budget refusals exercised)"
  return 0
}

if [ "$SELFTEST" -eq 1 ]; then
  selftest
  exit $?
fi

if [ "$MODE" != smoke ] && [ "$MODE" != long ]; then
  echo "soak: FAKTOR_SOAK_MODE must be smoke|long (got '$MODE')" >&2
  exit 2
fi

[ -n "$MAX_WALL_RAW" ] || { [ "$MODE" = smoke ] && MAX_WALL_RAW=1800 || MAX_WALL_RAW=21600; }
[ -n "$TEST_TIMEOUT_RAW" ] || { [ "$MODE" = smoke ] && TEST_TIMEOUT_RAW=1200 || TEST_TIMEOUT_RAW=3600; }

if ! is_positive_number "$MAX_SCALE_RAW"; then
  echo "soak: FAKTOR_SOAK_MAX_SCALE must be a positive number (got '$MAX_SCALE_RAW')" >&2
  exit 2
fi
if ! is_nonneg_integer "$MAX_ROUNDS_RAW" || [ "$MAX_ROUNDS_RAW" -lt 1 ]; then
  echo "soak: FAKTOR_SOAK_MAX_ROUNDS must be a positive integer (got '$MAX_ROUNDS_RAW')" >&2
  exit 2
fi
if ! is_nonneg_integer "$MAX_WALL_RAW" || [ "$MAX_WALL_RAW" -lt 1 ]; then
  echo "soak: FAKTOR_SOAK_MAX_WALL_SECONDS must be a positive integer (got '$MAX_WALL_RAW')" >&2
  exit 2
fi
if ! is_nonneg_integer "$TEST_TIMEOUT_RAW" || [ "$TEST_TIMEOUT_RAW" -lt 1 ]; then
  echo "soak: FAKTOR_SOAK_TEST_TIMEOUT_SECONDS must be a positive integer (got '$TEST_TIMEOUT_RAW')" >&2
  exit 2
fi

TIMEOUT_BIN="$(command -v timeout 2>/dev/null || true)"
MAX_SCALE="$MAX_SCALE_RAW"
MAX_ROUNDS="$MAX_ROUNDS_RAW"
MAX_WALL="$MAX_WALL_RAW"
TEST_TIMEOUT="$TEST_TIMEOUT_RAW"
SCALE_REQUESTED="$SCALE_RAW"
SCALE_EFFECTIVE=0
SCALE_CLAMPED=0
ROUNDS=1
ROUNDS_CAPPED=0
COMMIT="${CI_COMMIT_SHA:-unknown}"
TREE="${CI_COMMIT_TREE:-unknown}"

# A refusal still records a typed failure (never a silent no-op pass).
refuse() {
  local reason="$1"
  mkdir -p "$OUT_DIR"
  OUT_FILE="$OUT_DIR/soak.json"
  local now
  now="$(iso_now)"
  SCALE_EFFECTIVE=0
  ROUNDS=0
  write_record failed "$reason" "$now" "$now" 0 0 0 0
  echo "soak: REFUSED: $reason" >&2
  exit 1
}

if ! is_positive_number "$SCALE_RAW"; then
  if printf '%s' "$SCALE_RAW" | grep -qE '^0+([.]0*)?$'; then
    refuse "zero-scale: FAKTOR_SOAK_SCALE must be > 0 (got '$SCALE_RAW'); a zero-duration soak is refused"
  fi
  refuse "invalid-scale: FAKTOR_SOAK_SCALE must be a positive number (got '$SCALE_RAW')"
fi
if [ "$MODE" = smoke ] && [ "${#CARGO_ARGS[@]}" -eq 0 ]; then
  SCALE_EFFECTIVE="$SCALE_RAW"
  SCALE_CLAMPED=0
  ROUNDS=1
else
  SCALE_EFFECTIVE="$(awk -v s="$SCALE_RAW" -v m="$MAX_SCALE" 'BEGIN { print (s < m ? s : m) }')"
  SCALE_CLAMPED="$(awk -v s="$SCALE_RAW" -v m="$MAX_SCALE" 'BEGIN { print (s > m ? 1 : 0) }')"
  ROUNDS="$(awk -v s="$SCALE_EFFECTIVE" 'BEGIN { r = int(s); if (r < s) r++; if (r < 1) r = 1; print r }')"
  if [ "$ROUNDS" -gt "$MAX_ROUNDS" ]; then
    ROUNDS="$MAX_ROUNDS"
    ROUNDS_CAPPED=1
  fi
fi
if [ "$ROUNDS" -lt 1 ]; then
  refuse "zero-duration: computed rounds=$ROUNDS (check FAKTOR_SOAK_SCALE)"
fi

build_plan || exit 2

if [ "$PRINT_PLAN" -eq 1 ]; then
  printf '{"schema":"faktor-soak-plan/v1","mode":"%s","groups":"%s","scale_requested":%s,"scale_effective":%s,"scale_clamped":%s,"rounds":%s,"max_rounds":%s,"max_wall_seconds":%s,"test_timeout_seconds":%s,"commands":[' \
    "$MODE" "$PLAN_GROUPS" "$SCALE_REQUESTED" "$SCALE_EFFECTIVE" "$SCALE_CLAMPED" "$ROUNDS" "$MAX_ROUNDS" "$MAX_WALL" "$TEST_TIMEOUT"
  first=1
  while IFS=$'\t' read -r g c; do
    [ -n "$g" ] || continue
    [ "$first" -eq 1 ] || printf ','
    printf '{"group":"%s","command":"%s"}' "$(json_escape "$g")" "$(json_escape "$c")"
    first=0
  done <<<"$PLAN_COMMANDS"
  printf ']}\n'
  exit 0
fi

# Boundedness is a hard contract: without `timeout` a hung gate could run
# forever, so a missing binary is refused typed instead of degrading to an
# unbounded run. (--print-plan above stays usable without it.)
if [ -z "$TIMEOUT_BIN" ]; then
  echo "soak: a 'timeout' binary is required to bound every invocation (install coreutils); refusing to run unbounded" >&2
  exit 2
fi

run_campaign

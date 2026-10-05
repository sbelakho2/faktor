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
#     always-fast gate used before the churn promotion); `long` mode applies
#     the scale (the legacy scheduled campaign).
#   * `churn` mode is the per-release accelerated churn (30-60 min): rounds
#     repeat until at least `FAKTOR_SOAK_CHURN_SECONDS` of real wall time has
#     elapsed, bounded by `FAKTOR_SOAK_MAX_WALL_SECONDS`.
#   * `realtime` mode is the nightly/RC real-time durability run (12-24 h):
#     rounds repeat until `FAKTOR_SOAK_REALTIME_SECONDS` of REAL wall clock
#     has elapsed (the workload cannot shorten it), bounded by the wall
#     budget.
#   * A run that executes NO test is a FAILURE (no-op refusal). A zero,
#     negative or unparseable scale is refused before any test runs. A
#     non-positive wall/test budget is refused. The campaign never runs
#     unbounded: per-invocation `timeout` plus a checked global wall budget.
#   * `FAKTOR_SOAK_SCALE`, `FAKTOR_SOAK_MODE`, `FAKTOR_SOAK_TARGET_SECONDS`,
#     `FAKTOR_SOAK_DATA_DIR` and `FAKTOR_SOAK_METRICS_FILE` are exported to
#     each test process so the convergence-aware workload can honor them.
#
# QUIESCENT CONVERGENCE (mandatory in churn/realtime): when the workload has
# quiesced the driver samples the subject process tree and the soak data dir
# with bounded samplers (`scripts/certification/soak-convergence.py sample`)
# and evaluates typed pass/fail metrics (`... check`):
#   rss_bounded, fds_bounded, child_processes_zero,
#   background_tasks_settled, writer_queue_zero, reader_queue_zero,
#   wal_converged, temp_files_removed, cas_unreachable_stable,
#   journal_latency_no_upward_trend, index_latency_no_upward_trend,
#   reconnect_correct, duration_target_met.
# ANY failed metric fails the lane; missing metric samples are a typed
# FAILURE (the samplers exist and must produce data), never a skip.
#
# Modes of selection:
#   * Default groups (smoke/long): run the exact ignored sets of the `soak`,
#     `fault` and `perf` lanes from `scripts/certification/ignored-tests.json`.
#     In `smoke` mode each group runs ONE representative ignored test with
#     `--exact` (bounded CI smoke); in `long`/`churn`/`realtime` mode the
#     whole ignored set runs.
#   * Custom cargo args: `bash scripts/soak.sh -p <pkg> --release -- --ignored`
#     forwards everything from the first cargo option/`--` verbatim to
#     `cargo test` (this is how the trusted/nightly lane keeps the registry's
#     exact lane command while adding churn/realtime and convergence).
#
# Env:
#   FAKTOR_SOAK_SCALE               float > 0, default 1 (multiplier)
#   FAKTOR_SOAK_MODE                smoke|long|churn|realtime, default smoke
#   FAKTOR_SOAK_CHURN_SECONDS       default 1800 (churn target wall time)
#   FAKTOR_SOAK_REALTIME_SECONDS    default 43200 (real-time target wall time)
#   FAKTOR_SOAK_CONVERGENCE         required|off (default required for
#                                   churn/realtime, off otherwise)
#   FAKTOR_SOAK_LANE                lane id recorded in soak.json (default soak)
#   FAKTOR_SOAK_DATA_DIR            workload data dir sampled after quiescence
#   FAKTOR_SOAK_METRICS_FILE        workload runtime-metrics JSONL stream
#   FAKTOR_SOAK_CONVERGENCE_SAMPLES explicit sampler stream override (selftest)
#   FAKTOR_SOAK_SAMPLE_INTERVAL     sampler tick seconds (default 1)
#   FAKTOR_SOAK_MAX_SAMPLES         sampler sample cap (default 100000)
#   FAKTOR_SOAK_PYTHON              python3 binary override
#   FAKTOR_SOAK_GROUPS              csv subset of soak,fault,perf
#   FAKTOR_SOAK_MAX_SCALE           default 8 (scale clamp, recorded)
#   FAKTOR_SOAK_MAX_ROUNDS          default 8
#   FAKTOR_SOAK_MAX_WALL_SECONDS    default 1800 (smoke) / 21600 (long)
#   FAKTOR_SOAK_TEST_TIMEOUT_SECONDS default 1200 (smoke) / 3600 (long)
#   FAKTOR_SOAK_CARGO               cargo binary override
#   FAKTOR_SOAK_OUT_DIR             default <repo>/target/certification
#
# Usage:
#   bash scripts/soak.sh [--mode smoke|long|churn|realtime]
#                        [--groups soak,fault,perf] [--churn-seconds N]
#                        [--realtime-seconds N] [--convergence required|off]
#                        [--lane L] [--sample-interval S] [--max-samples N]
#                        [--scale N] [--max-rounds N] [--max-wall-seconds N]
#                        [--test-timeout-seconds N] [--cargo-bin PATH]
#                        [--out-dir DIR] [--print-plan] [--selftest]
#                        [CARGO TEST ARGS...]
#
# Exit codes: 0 passed; 1 a campaign/refusal/convergence failure; 2 usage error.
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
CHURN_SECONDS_RAW="${FAKTOR_SOAK_CHURN_SECONDS:-1800}"
REALTIME_SECONDS_RAW="${FAKTOR_SOAK_REALTIME_SECONDS:-43200}"
CONVERGENCE_RAW="${FAKTOR_SOAK_CONVERGENCE:-}"
LANE="${FAKTOR_SOAK_LANE:-soak}"
SAMPLE_INTERVAL_RAW="${FAKTOR_SOAK_SAMPLE_INTERVAL:-1}"
MAX_SAMPLES_RAW="${FAKTOR_SOAK_MAX_SAMPLES:-100000}"
PYTHON_BIN="${FAKTOR_SOAK_PYTHON:-python3}"
CONVERGENCE_PY="$ROOT/scripts/certification/soak-convergence.py"
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
  --mode | --groups | --scale | --max-rounds | --max-wall-seconds | --test-timeout-seconds | --cargo-bin | --out-dir | --churn-seconds | --realtime-seconds | --convergence | --lane | --sample-interval | --max-samples | --python-bin)
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
    --churn-seconds) CHURN_SECONDS_RAW="$2" ;;
    --realtime-seconds) REALTIME_SECONDS_RAW="$2" ;;
    --convergence) CONVERGENCE_RAW="$2" ;;
    --lane) LANE="$2" ;;
    --sample-interval) SAMPLE_INTERVAL_RAW="$2" ;;
    --max-samples) MAX_SAMPLES_RAW="$2" ;;
    --python-bin) PYTHON_BIN="$2" ;;
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
  local convergence_json="${CONVERGENCE_JSON:-}"
  if [ -z "$convergence_json" ]; then
    convergence_json="{\"schema\":\"faktor-soak-convergence/v1\",\"status\":\"not-run\",\"required\":${CONVERGENCE_REQUIRED:-false}}"
  fi
  TMP_OUT="$OUT_FILE.tmp.$$"
  local target_clamped_json=false
  [ "${TARGET_CLAMPED:-0}" = "1" ] && target_clamped_json=true
  printf '{"schema":"faktor-soak/v1","lane":"%s","status":"%s","reason":"%s","mode":"%s","commit":"%s","tree":"%s","runner":{"os":"%s","arch":"%s"},"started_at":"%s","finished_at":"%s","duration_seconds":%s,"target_seconds":%s,"target_clamped":%s,"scale_requested":%s,"scale_effective":%s,"max_scale":%s,"scale_clamped":%s,"rounds":%s,"max_rounds":%s,"rounds_capped":%s,"max_wall_seconds":%s,"test_timeout_seconds":%s,"bounded":true,"executed_total":%s,"passed_total":%s,"failed_total":%s,"convergence_required":%s,"convergence":%s,"groups":%s,"logs":%s}\n' \
    "$(json_escape "$LANE")" "$(json_escape "$status")" "$(json_escape "$reason")" "$(json_escape "$MODE")" \
    "$(json_escape "$COMMIT")" "$(json_escape "$TREE")" \
    "$(uname -s | tr '[:upper:]' '[:lower:]')" "$(uname -m)" \
    "$started" "$finished" "$duration" \
    "${TARGET_SECONDS:-0}" "$target_clamped_json" \
    "${SCALE_REQUESTED:-0}" "${SCALE_EFFECTIVE:-0}" "${MAX_SCALE:-0}" "$scale_clamped_json" \
    "${ROUNDS:-0}" "${MAX_ROUNDS:-0}" "$rounds_capped_json" \
    "${MAX_WALL:-0}" "${TEST_TIMEOUT:-0}" \
    "$executed" "$passed" "$failed" "${CONVERGENCE_REQUIRED:-false}" "$convergence_json" "$groups_json" "$logs_json" >"$TMP_OUT"
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

# churn/realtime modes are REAL wall-clock targets: a successful round that
# finished before the target extends the campaign (bounded by MAX_ROUNDS)
# instead of ending early. `round`/`start_epoch`/`TARGET_SECONDS` are visible
# through bash dynamic scoping.
maybe_extend_rounds() {
  local mode="${1:-$MODE}"
  if [ "$mode" != churn ] && [ "$mode" != realtime ]; then
    return 0
  fi
  local elapsed=$(($(date +%s) - start_epoch))
  if [ "$elapsed" -lt "$TARGET_SECONDS" ] && [ "$round" -lt "$MAX_ROUNDS" ]; then
    ROUNDS=$((ROUNDS + 1))
  fi
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
  local sampler_pid="" convergence_samples=""
  local SUBJECT_PIDFILE="" SOAK_DATA_DIR=""

  # ------------------------------------------------------- convergence setup
  # The samplers are real and bounded; when convergence is required their
  # inputs MUST exist, so a missing sampler/dir/stream is a typed failure
  # (never a skip). The explicit FAKTOR_SOAK_CONVERGENCE_SAMPLES override is
  # the fake-metrics seam used by --selftest and nothing else.
  if [ "$CONVERGENCE" = required ]; then
    CONVERGENCE_REQUIRED=true
    SOAK_DATA_DIR="${FAKTOR_SOAK_DATA_DIR:-$OUT_DIR/soak-data}"
    if [ -z "${FAKTOR_SOAK_DATA_DIR:-}" ]; then
      rm -rf "$SOAK_DATA_DIR"
    fi
    mkdir -p "$SOAK_DATA_DIR"
    WORKLOAD_METRICS="${FAKTOR_SOAK_METRICS_FILE:-$LOG_DIR/workload-metrics.jsonl}"
    mkdir -p "$(dirname "$WORKLOAD_METRICS")"
    : >"$WORKLOAD_METRICS"
    export FAKTOR_SOAK_DATA_DIR="$SOAK_DATA_DIR"
    export FAKTOR_SOAK_METRICS_FILE="$WORKLOAD_METRICS"
    export FAKTOR_SOAK_TARGET_SECONDS="${TARGET_SECONDS:-0}"
    SUBJECT_PIDFILE="$LOG_DIR/subject.pid"
    ROOT_PIDFILE="$LOG_DIR/subject-root.pid"
    if [ -n "${FAKTOR_SOAK_CONVERGENCE_SAMPLES:-}" ]; then
      convergence_samples="$FAKTOR_SOAK_CONVERGENCE_SAMPLES"
      mkdir -p "$(dirname "$convergence_samples")"
      : >"$convergence_samples"
    else
      convergence_samples="$LOG_DIR/convergence.jsonl"
      : >"$convergence_samples"
      if ! command -v "$PYTHON_BIN" >/dev/null 2>&1; then
        echo "soak: convergence requires '$PYTHON_BIN' (bounded samplers are implemented, never skipped)" >&2
        CONVERGENCE_JSON='{"schema":"faktor-soak-convergence/v1","status":"failed","required":true,"failed_metrics":["sampler-unavailable"]}'
        reason="convergence-failed: sampler-unavailable"
        rc=1
      else
        # Bounded sampler: follows the per-round subject pidfile, samples the
        # process tree + data dir every tick, writes one final quiescent
        # sample + final scan on SIGTERM. Launched directly (not behind
        # `timeout`) so the driver's SIGTERM reaches the python process and
        # its own --duration/--max-samples caps keep it bounded regardless.
        "$PYTHON_BIN" "$CONVERGENCE_PY" sample \
          --out "$convergence_samples" \
          --pidfile "$SUBJECT_PIDFILE" \
          --root-pidfile "$ROOT_PIDFILE" \
          --data-dir "$SOAK_DATA_DIR" \
          --final-scan "$LOG_DIR/convergence-final.json" \
          --interval "$SAMPLE_INTERVAL" \
          --duration "$MAX_WALL" \
          --max-samples "$MAX_SAMPLES" \
          ${FAKTOR_SOAK_PROBE_URL:+--probe-url "$FAKTOR_SOAK_PROBE_URL"} \
          >"$LOG_DIR/convergence-sampler.log" 2>&1 &
        sampler_pid=$!
      fi
    fi
  else
    CONVERGENCE_REQUIRED=false
  fi

  if [ "$rc" -eq 0 ]; then
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
      maybe_extend_rounds
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
      maybe_extend_rounds
      round=$((round + 1))
    done
    IFS="$old_ifs"
  fi
  fi  # convergence preflight ok

  # ------------------------------------------------- convergence evaluation
  # The sampler is stopped FIRST so its final quiescent sample + final scan
  # land on disk before the checker reads them.
  if [ -n "$sampler_pid" ]; then
    kill -TERM "$sampler_pid" 2>/dev/null || true
    local waited=0
    while kill -0 "$sampler_pid" 2>/dev/null && [ "$waited" -lt 30 ]; do
      sleep 1
      waited=$((waited + 1))
    done
    kill -KILL "$sampler_pid" 2>/dev/null || true
    wait "$sampler_pid" 2>/dev/null || true
  fi
  [ -z "$SUBJECT_PIDFILE" ] || rm -f "$SUBJECT_PIDFILE" "$ROOT_PIDFILE"

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
  if [ "$CONVERGENCE" = required ]; then
    local conv_rc=0
    "$PYTHON_BIN" "$CONVERGENCE_PY" check \
      --samples "$convergence_samples" \
      --workload "$WORKLOAD_METRICS" \
      --final "$LOG_DIR/convergence-final.json" \
      --lane "$LANE" --mode "$MODE" \
      --commit "$COMMIT" --tree "$TREE" \
      --elapsed-seconds "$duration" \
      --target-seconds "$TARGET_SECONDS" \
      --max-seconds "$MAX_WALL" \
      --out "$OUT_DIR/soak-convergence.json" \
      >"$LOG_DIR/convergence-check.stdout" 2>&1 || conv_rc=$?
    if [ -f "$OUT_DIR/soak-convergence.json" ]; then
      CONVERGENCE_JSON="$(tr -d '\n' <"$OUT_DIR/soak-convergence.json")"
    else
      CONVERGENCE_JSON='{"schema":"faktor-soak-convergence/v1","status":"failed","required":true,"failed_metrics":["checker-produced-no-report"]}'
    fi
    if [ "$conv_rc" -ne 0 ]; then
      local failed_names
      failed_names="$(printf '%s' "$CONVERGENCE_JSON" | sed -n 's/.*"failed_metrics": *\[\([^]]*\)\].*/\1/p' | tr -d '"' | sed 's/^ *//;s/ *$//')"
      echo "soak: convergence FAILED (${failed_names:-unknown})" >&2
      if [ "$rc" -eq 0 ]; then
        reason="convergence-failed: ${failed_names:-unknown}"
        rc=1
      fi
    else
      echo "soak: convergence passed (all required metrics converged)" >&2
    fi
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
  # Scale-/convergence-awareness hook: the effective multiplier, mode, real
  # target and the convergence paths are visible to the test processes.
  local target_left=0
  if [ "$CONVERGENCE" = required ]; then
    target_left=$((TARGET_SECONDS - ($(date +%s) - start_epoch)))
    [ "$target_left" -ge 1 ] || target_left=1
  fi
  #
  # With convergence required the command runs as a background job so its
  # timeout/process-tree root can be published to the sampler pidfile; the
  # pipeline exit status is preserved (wait returns the job's status).
  if [ "$CONVERGENCE" = required ] && [ -n "$SUBJECT_PIDFILE" ]; then
    rm -f "$SUBJECT_PIDFILE" "$ROOT_PIDFILE"
    FAKTOR_SOAK_SCALE="$SCALE_EFFECTIVE" FAKTOR_SOAK_MODE="$MODE" \
      FAKTOR_SOAK_TARGET_SECONDS="$target_left" \
      FAKTOR_SOAK_DATA_DIR="$SOAK_DATA_DIR" \
      FAKTOR_SOAK_METRICS_FILE="$WORKLOAD_METRICS" \
      FAKTOR_SOAK_SUBJECT_PIDFILE="$SUBJECT_PIDFILE" \
      "$TIMEOUT_BIN" "$bound" "${argv[@]}" > >(tee "$log") 2>&1 &
    local subject_pid=$!
    printf '%s\n' "$subject_pid" >"$ROOT_PIDFILE"
    wait "$subject_pid"
    local subject_rc=$?
    rm -f "$SUBJECT_PIDFILE" "$ROOT_PIDFILE"
    return "$subject_rc"
  fi
  FAKTOR_SOAK_SCALE="$SCALE_EFFECTIVE" FAKTOR_SOAK_MODE="$MODE" \
    FAKTOR_SOAK_TARGET_SECONDS="$target_left" \
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
# Fake convergence metrics: `metrics-good` writes a converging world,
# `metrics-bad` writes a leaky one. The loops are bounded (12 samples/round).
emit_convergence() {
  world="$1"
  samples="${FAKTOR_SOAK_CONVERGENCE_SAMPLES:-}"
  work="${FAKTOR_SOAK_METRICS_FILE:-}"
  i=0
  while [ "$i" -lt 12 ]; do
    if [ "$world" = good ]; then
      rss=120000; fds=40; children=0; wal=1048576; temp=0; cas=0
      wq=0; rq=0; bg=0; lat=100; ok=true; final=false
      [ "$i" -eq 11 ] && final=true
    else
      rss=$((300000 + i * 400000)); fds=$((100 + i * 60)); children=2
      wal=$((1048576 + i * 8388608)); temp=3; cas=$((i * 40))
      wq=5; rq=5; bg=5; lat=$((100 + i * 20000)); ok=false; final=false
    fi
    [ -n "$samples" ] && printf '{"t":%s,"rss_kb":%s,"fds":%s,"children":%s,"orphans":0,"wal_bytes":%s,"temp_files":%s,"cas_blobs":50,"cas_unreachable":%s,"probe_ok":true,"final":%s}\n' \
      "$((i * 1000))" "$rss" "$fds" "$children" "$wal" "$temp" "$cas" "$final" >>"$samples"
    [ -n "$work" ] && printf '{"t":%s,"writer_queue":%s,"reader_queue":%s,"background_tasks":%s,"journal_us":%s,"index_us":%s}\n' \
      "$((i * 1000))" "$wq" "$rq" "$bg" "$lat" "$lat" >>"$work"
    i=$((i + 1))
  done
  [ -n "$work" ] && printf '{"t":12000,"event":"reconnect","ok":%s}\n' "$ok" >>"$work"
}
case "${FAKE_CARGO_MODE:-ok}" in
ok) printf 'running 2 tests\ntest result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n' ;;
zero) printf 'running 0 tests\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n' ;;
fail) printf 'test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n'; exit 101 ;;
sleep) printf 'running 2 tests\n'; sleep 30 ;;
metrics-good)
  printf 'running 2 tests\ntest result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n'
  emit_convergence good
  sleep 0.3
  ;;
metrics-bad)
  printf 'running 2 tests\ntest result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n'
  emit_convergence bad
  sleep 2.5
  ;;
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

  # 9. the convergence checker's own metric algebra (pass/fail/missing/
  # duration) with fake metrics
  if command -v "$PYTHON_BIN" >/dev/null 2>&1; then
    "$PYTHON_BIN" "$CONVERGENCE_PY" selftest >"$tmp/conv-tool.out" 2>&1
    rc=$?
    check "convergence checker selftest passes" "$([ "$rc" -eq 0 ] && echo 0 || echo 1)"
    check "convergence checker passed its full matrix" "$(contains "$tmp/conv-tool.out" 'soak-convergence selftest: PASS' && echo 0 || echo 1)"
  else
    check "python3 present for the convergence gate (samplers are never skipped)" 1
  fi

  # 10. accelerated churn + REQUIRED convergence with converging fake metrics:
  # the lane passes only with a typed all-metrics-passed convergence record
  out="$tmp/out-conv-ok"
  conv_samples="$tmp/conv-ok-samples.jsonl"
  FAKE_CARGO_LOG="$tmp/fake-conv-ok.log" FAKE_CARGO_MODE=metrics-good \
  FAKTOR_SOAK_CONVERGENCE_SAMPLES="$conv_samples" \
    bash "$SELF" --mode churn --churn-seconds 2 --max-wall-seconds 60 \
    --test-timeout-seconds 30 --sample-interval 0.2 --convergence required \
    --lane soak-smoke --cargo-bin "$tmp/fake-cargo" --out-dir "$out" \
    -p faktor-tests-soak --release -- --ignored >"$tmp/conv-ok.stdout" 2>"$tmp/conv-ok.stderr"
  rc=$?
  check "churn + required convergence passes" "$([ "$rc" -eq 0 ] && echo 0 || echo 1)"
  check "convergence report passed" "$(contains "$out/soak-convergence.json" '"status": "passed"' && echo 0 || echo 1)"
  check "convergence required recorded" "$(contains "$out/soak.json" '"convergence_required":true' && echo 0 || echo 1)"
  check "lane recorded" "$(contains "$out/soak.json" '"lane":"soak-smoke"' && echo 0 || echo 1)"
  check "duration target recorded" "$(contains "$out/soak.json" '"target_seconds":2' && echo 0 || echo 1)"
  metrics_ok=1
  for metric_name in rss_bounded fds_bounded child_processes_zero \
    background_tasks_settled writer_queue_zero reader_queue_zero wal_converged \
    temp_files_removed cas_unreachable_stable journal_latency_no_upward_trend \
    index_latency_no_upward_trend reconnect_correct duration_target_met; do
    contains "$out/soak.json" "$metric_name" || metrics_ok=0
  done
  check "every convergence metric is recorded in soak.json" "$([ "$metrics_ok" -eq 1 ] && echo 0 || echo 1)"

  # 11. any failed metric fails the lane: leaky fake metrics never record passed
  out="$tmp/out-conv-bad"
  conv_samples="$tmp/conv-bad-samples.jsonl"
  FAKE_CARGO_LOG="$tmp/fake-conv-bad.log" FAKE_CARGO_MODE=metrics-bad \
  FAKTOR_SOAK_CONVERGENCE_SAMPLES="$conv_samples" \
    bash "$SELF" --mode churn --churn-seconds 2 --max-wall-seconds 60 \
    --test-timeout-seconds 30 --sample-interval 0.2 --convergence required \
    --lane soak-smoke --cargo-bin "$tmp/fake-cargo" --out-dir "$out" \
    -p faktor-tests-soak --release -- --ignored >"$tmp/conv-bad.stdout" 2>"$tmp/conv-bad.stderr"
  rc=$?
  check "leaky convergence fails the lane" "$([ "$rc" -eq 1 ] && echo 0 || echo 1)"
  check "convergence-failed reason recorded" "$(contains "$out/soak.json" 'convergence-failed' && echo 0 || echo 1)"
  check "failed metrics named in soak.json" "$(contains "$out/soak.json" 'rss_bounded' && contains "$out/soak.json" 'reconnect_correct' && echo 0 || echo 1)"
  check "failed lane never records passed status" "$([ "$(field "$out/soak.json" status)" = failed ] && echo 0 || echo 1)"

  if [ "$failures" -gt 0 ]; then
    echo "soak selftest: FAIL ($failures case(s))" >&2
    return 1
  fi
  echo "soak selftest: PASS (scale bounds, zero-duration, no-op and per-invocation wall-budget refusals; convergence metric algebra pass/fail/missing/duration; churn integration passes only on all-metrics-converged evidence)"
  return 0
}

if [ "$SELFTEST" -eq 1 ]; then
  selftest
  exit $?
fi

case "$MODE" in
smoke | long | churn | realtime) ;;
*)
  echo "soak: FAKTOR_SOAK_MODE must be smoke|long|churn|realtime (got '$MODE')" >&2
  exit 2
  ;;
esac

# churn/realtime require the convergence checker; smoke/long keep it off
# unless explicitly requested. An explicit invalid value is always refused.
case "$CONVERGENCE_RAW" in
'')
  if [ "$MODE" = churn ] || [ "$MODE" = realtime ]; then
    CONVERGENCE=required
  else
    CONVERGENCE=off
  fi
  ;;
required | off) CONVERGENCE="$CONVERGENCE_RAW" ;;
*)
  echo "soak: FAKTOR_SOAK_CONVERGENCE must be required|off (got '$CONVERGENCE_RAW')" >&2
  exit 2
  ;;
esac
CONVERGENCE_REQUIRED=false
[ "$CONVERGENCE" = required ] && CONVERGENCE_REQUIRED=true
TARGET_SECONDS=0
TARGET_CLAMPED=0
if [ "$MODE" = churn ]; then
  if ! is_nonneg_integer "$CHURN_SECONDS_RAW" || [ "$CHURN_SECONDS_RAW" -lt 1 ]; then
    echo "soak: FAKTOR_SOAK_CHURN_SECONDS must be a positive integer (got '$CHURN_SECONDS_RAW')" >&2
    exit 2
  fi
  TARGET_SECONDS="$CHURN_SECONDS_RAW"
elif [ "$MODE" = realtime ]; then
  if ! is_nonneg_integer "$REALTIME_SECONDS_RAW" || [ "$REALTIME_SECONDS_RAW" -lt 1 ]; then
    echo "soak: FAKTOR_SOAK_REALTIME_SECONDS must be a positive integer (got '$REALTIME_SECONDS_RAW')" >&2
    exit 2
  fi
  TARGET_SECONDS="$REALTIME_SECONDS_RAW"
fi
if [ "$CONVERGENCE" = required ]; then
  if ! is_positive_number "$SAMPLE_INTERVAL_RAW"; then
    echo "soak: FAKTOR_SOAK_SAMPLE_INTERVAL must be a positive number (got '$SAMPLE_INTERVAL_RAW')" >&2
    exit 2
  fi
  if ! is_nonneg_integer "$MAX_SAMPLES_RAW" || [ "$MAX_SAMPLES_RAW" -lt 1 ]; then
    echo "soak: FAKTOR_SOAK_MAX_SAMPLES must be a positive integer (got '$MAX_SAMPLES_RAW')" >&2
    exit 2
  fi
fi
SAMPLE_INTERVAL="$SAMPLE_INTERVAL_RAW"
MAX_SAMPLES="$MAX_SAMPLES_RAW"

if [ -z "$MAX_WALL_RAW" ]; then
  case "$MODE" in
  smoke) MAX_WALL_RAW=1800 ;;
  long) MAX_WALL_RAW=21600 ;;
  churn) MAX_WALL_RAW=$((TARGET_SECONDS + 1800)) ;;
  realtime) MAX_WALL_RAW=$((TARGET_SECONDS + 3600)) ;;
  esac
fi
if [ -z "$TEST_TIMEOUT_RAW" ]; then
  case "$MODE" in
  smoke) TEST_TIMEOUT_RAW=1200 ;;
  long) TEST_TIMEOUT_RAW=3600 ;;
  churn) TEST_TIMEOUT_RAW=$((TARGET_SECONDS + 1200)) ;;
  realtime) TEST_TIMEOUT_RAW=$((TARGET_SECONDS + 3000)) ;;
  esac
fi

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

if [ "$TARGET_SECONDS" -gt "$MAX_WALL_RAW" ]; then
  echo "soak: $MODE target ${TARGET_SECONDS}s exceeds the wall budget ${MAX_WALL_RAW}s; clamped" >&2
  TARGET_SECONDS="$MAX_WALL_RAW"
  TARGET_CLAMPED=1
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
if [ "$MODE" = churn ] || [ "$MODE" = realtime ]; then
  # Target-driven modes: one workload invocation honors FAKTOR_SOAK_TARGET_SECONDS;
  # a round that ends early is repeated by maybe_extend_rounds (bounded by
  # MAX_ROUNDS) so the campaign cannot shrink below the real target.
  SCALE_EFFECTIVE="$(awk -v s="$SCALE_RAW" -v m="$MAX_SCALE" 'BEGIN { print (s < m ? s : m) }')"
  SCALE_CLAMPED="$(awk -v s="$SCALE_RAW" -v m="$MAX_SCALE" 'BEGIN { print (s > m ? 1 : 0) }')"
  ROUNDS=1
elif [ "$MODE" = smoke ] && [ "${#CARGO_ARGS[@]}" -eq 0 ]; then
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
  printf '{"schema":"faktor-soak-plan/v1","lane":"%s","mode":"%s","groups":"%s","target_seconds":%s,"convergence":"%s","scale_requested":%s,"scale_effective":%s,"scale_clamped":%s,"rounds":%s,"max_rounds":%s,"max_wall_seconds":%s,"test_timeout_seconds":%s,"commands":[' \
    "$(json_escape "$LANE")" "$MODE" "$PLAN_GROUPS" "$TARGET_SECONDS" "$CONVERGENCE" "$SCALE_REQUESTED" "$SCALE_EFFECTIVE" "$SCALE_CLAMPED" "$ROUNDS" "$MAX_ROUNDS" "$MAX_WALL" "$TEST_TIMEOUT"
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

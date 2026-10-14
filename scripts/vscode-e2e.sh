#!/usr/bin/env bash
# Faktor VS Code Extension Host / webview E2E matrix lane.
#
# Downloads a PINNED linux-x64 VS Code build, verifies the recorded SHA-256,
# and runs REAL Extension Host cells against a real `--extensionDevelopmentPath`
# tree (the dev tree, or the VSIX-extracted tree in `--vsix` mode) with
# isolated `--user-data-dir` / `--extensions-dir`. The host-side test module
# (`scripts/vscode-e2e/extension-host-smoke.cjs`) executes inside the pinned
# VS Code extension host and asserts:
#   * the `faktor.faktor` extension is present and activates,
#   * the contributed `faktor.*` commands are registered (incl. openChat),
#   * the compiled Faktor webview provider renders HTML carrying the strict
#     CSP plus the composer and attachment surface elements,
#   * a REAL WebviewPanel matrix driven over the Chrome DevTools Protocol:
#     live light/dark/high-contrast/high-contrast-light theme switches with
#     the acceptance-verdict chip contrast re-measured in the renderer,
#     real webview viewports at 240/320/480/800 CSS px, real keyboard-only
#     Tab traversal + Ctrl+Enter submission (trusted Input events), paste and
#     drop attachment bytes, dispose+reopen snapshot/attachment restoration,
#     and dedicated `--force-device-scale-factor=1.25|2` cells for the
#     100/125/200% zoom factors (devicePixelRatio asserted in the webview).
# The exact method per capability and the capabilities that cannot be
# emulated headlessly are recorded verbatim in the JSON record and evidence.
#
# COVERAGE / WHAT THIS DOES NOT PROVE (recorded verbatim in the JSON record):
#   * It does NOT take screenshots or compare rendered pixels; there is no
#     image/visual-baseline coverage.
#   * It does NOT talk to a live daemon; daemon behavior is covered by the
#     Faktor Node panel selftest and the protocol test suites.
#   * OS-level clipboard/drag payload injection and physical panel resizing
#     are unavailable headlessly; the record names the strongest substitute
#     that ran for each.
#
# Headless mechanics: the desktop VS Code workbench needs a display. When
# `Xvfb` is available (the pinned playwright image ships it) the harness
# starts and owns Xvfb itself (`-displayfd` picks the display; the socket is
# awaited) and runs every cell under that real X display; otherwise Electron
# falls back to `--ozone-platform=headless --headless --disable-gpu`
# (no X). Running as
# root (`id -u` = 0) adds `--no-sandbox` (Electron refuses to start as root
# otherwise). FAKTOR_VSCODE_E2E_XVFB=0 forces the headless-ozone fallback.
# Every launch runs with `--verbose` so the app's own stdout/stderr stays in
# the captured stdout.log (the CLI otherwise detaches the app's stdio).
# RUNTIME DIRS: VS Code's main-process IPC socket is <user-data-dir>/
# 1.14-main.sock; Linux sockaddr_un.sun_path is capped at 107 bytes, so a
# deep workspace (CI: /woodpecker/src/.../vscode-e2e/work/host-*) makes the
# app die in claimInstance with `listen EINVAL` and exit rc=0 before the test
# module loads. All host launches therefore use a short mktemp runtime dir
# under TMPDIR (validated up front; over-long TMPDIR is a typed refusal) and
# the harness reads the VS Code logs from that runtime user-data dir.
# A cell that exits without evidence NEVER passes: the harness captures the
# host stdout/stderr tail, the newest VS Code main/renderer/extension-host
# log lines and any crash dumps into $cell_dir/failure-diagnostic.txt and
# fails the lane with that diagnostic folded into the reason.
#
# LOAD-AWARE HOST TIMEOUT (emulated/VM runners). The documented product
# runner is genuine x86_64. Emulated x64 (Rosetta/VM) runners stretch
# Electron cold start and every in-host wait, so the per-cell `timeout` is
# derived from a same-run CONTROL instead of trusting the constant:
#   * CONTROL: a BOUNDED calibration launch of the SAME pinned Electron
#     host (`scripts/vscode-e2e/calibration-host.cjs`) through the SAME
#     launch path and Xvfb facility as the real cells, immediately before
#     them. It records `host_module_live_at` and `activation_at`; the
#     measured wall time to extension activation is the control.
#   * DERIVATION: `effective = min(max(requested, ceil(control_s) *
#     factor), hard cap)` (`FAKTOR_VSCODE_E2E_TIMEOUT_FACTOR` default 30 =
#     the documented full capability battery / cold-start ratio on the
#     product runner; `FAKTOR_VSCODE_E2E_TIMEOUT_CAP` default 3600 s).
#     The requested timeout (`--timeout`, default 600 s) is a floor, never
#     shrunk. The derived value scales the smoke module's own bounded
#     waits through `FAKTOR_VSCODE_E2E_WAIT_FACTOR` = control / documented
#     nominal (`FAKTOR_VSCODE_E2E_CALIBRATION_NOMINAL_MS` default 4000 ms),
#     clamped to 16x. Every figure is recorded in the JSON record.
#   * If the calibration host itself cannot reach activation inside the
#     bounded calibration window (`FAKTOR_VSCODE_E2E_CALIBRATION_TIMEOUT`,
#     default 900 s), the lane fails typed `extension-host-calibration-
#     failed` with the folded diagnostics; it NEVER guesses a timeout.
#   * STAGED MARKERS: download -> unpack -> Xvfb up -> electron spawn ->
#     host module live -> extension activate -> matrix -> verdict are
#     appended to `$WORK_DIR/progress.log` and each cell's
#     `$cell_dir/progress.log` (both under the evidence work dir), so a
#     kill/timeout is attributable to the exact stage that was running.
#   * TYPED REASONS: an evidence-less host exits as `extension-host-
#     timeout` (rc 124), `extension-host-killed` (rc 137/143) or
#     `extension-host-no-evidence` (any other rc), always with the
#     captured host stdout/VS Code logs/crashdumps and the last stage
#     folded in; none of these branches can pass.
#   * NO SILENT RETRIES: a failed or killed cell is NEVER retried or
#     downgraded. The only retry in the lane is curl's transport-level
#     `--retry 3` for the tarball download. All spawned children (the
#     host process group and its Xvfb) are killed and reaped
#     deterministically on every cell exit path and on harness exit.
#
# PIN PROVENANCE (recorded 2026-10-02):
#   curl https://update.code.visualstudio.com/api/update/linux-x64/stable/1.140.0
#   -> name 1.140.0, commit 07f806f999227108933c2e30515b26eecc1fda74,
#      sha256 d32031e9e213d59532af3cf32fcb8b357a1cdd10417967b4f5b5ba30436dc0dc
# The tarball URL embeds the immutable commit; the SHA-256 is verified before
# extraction, and the extracted `code --version` must print the pinned version
# AND commit.
#
# Usage:
#   bash scripts/vscode-e2e.sh [--cli-only] [--keep] [--print-pin]
#                              [--cache-dir DIR] [--work-dir DIR] [--out FILE]
#                              [--extension-dir DIR] [--vsix PATH]
#                              [--timeout SECONDS] [--timeout-factor N]
#                              [--timeout-cap SECONDS]
#                              [--calibration auto|off]
#                              [--calibration-timeout SECONDS] [--selftest]
#
# Self-test (no network, no real Electron needed):
#   bash scripts/vscode-e2e.sh --selftest
#
# Exit codes: 0 passed (or expected selftest failure matched); 1 failure; 2 usage.
set -uo pipefail

SELF="${BASH_SOURCE[0]}"
SCRIPT_DIR="$(cd "$(dirname "$SELF")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

VSCODE_VERSION="1.140.0"
VSCODE_COMMIT="07f806f999227108933c2e30515b26eecc1fda74"
VSCODE_BUILD="1790759436"
VSCODE_SHA256="d32031e9e213d59532af3cf32fcb8b357a1cdd10417967b4f5b5ba30436dc0dc"
VSCODE_URL="https://vscode.download.prss.microsoft.com/dbazure/download/stable/${VSCODE_COMMIT}/code-stable-x64-${VSCODE_BUILD}.tar.gz"

CACHE_DIR="${FAKTOR_VSCODE_E2E_CACHE:-$ROOT/target/certification/vscode-e2e/cache}"
WORK_DIR="${FAKTOR_VSCODE_E2E_WORK:-$ROOT/target/certification/vscode-e2e/work}"
OUT_FILE="${FAKTOR_VSCODE_E2E_OUT:-$ROOT/target/certification/vscode-e2e.json}"
EXTENSION_DIR="${FAKTOR_VSCODE_E2E_EXTENSION:-$ROOT/apps/vscode}"
VSIX_PATH="${FAKTOR_VSCODE_E2E_VSIX:-}"
HOST_TIMEOUT="${FAKTOR_VSCODE_E2E_TIMEOUT:-600}"
# Load-aware host timeout policy (see LOAD-AWARE HOST TIMEOUT above).
HOST_TIMEOUT_FACTOR="${FAKTOR_VSCODE_E2E_TIMEOUT_FACTOR:-30}"
HOST_TIMEOUT_CAP="${FAKTOR_VSCODE_E2E_TIMEOUT_CAP:-3600}"
CALIBRATION_RAW="${FAKTOR_VSCODE_E2E_CALIBRATION:-auto}"
CALIBRATION_TIMEOUT="${FAKTOR_VSCODE_E2E_CALIBRATION_TIMEOUT:-900}"
CALIBRATION_NOMINAL_MS="${FAKTOR_VSCODE_E2E_CALIBRATION_NOMINAL_MS:-4000}"
WAIT_FACTOR_MAX="${FAKTOR_VSCODE_E2E_WAIT_FACTOR_MAX:-16}"
EFFECTIVE_HOST_TIMEOUT="$HOST_TIMEOUT"
WAIT_FACTOR=1
CONTROL_MS=""
EXTENSION_HOST_SMOKE="$SCRIPT_DIR/vscode-e2e/extension-host-smoke.cjs"
CALIBRATION_HOST="$SCRIPT_DIR/vscode-e2e/calibration-host.cjs"
# Matrix cells: "<label>:<device-scale-factor>:<mode>". The full cell runs the
# capability battery (themes, widths, keyboard, paste/drop, disposal); the
# extra cells prove the exact 125%/200% real device scale factors.
MATRIX_CELLS=("full:1:full" "zoom125:1.25:zoom" "zoom200:2:zoom")
HOST_EXTENSION_DIR=""
HOST_PACKAGE_MODE="dev"
VSIX_EXPLICIT=0
CLI_ONLY=0
KEEP=0
SELFTEST=0
PRINT_PIN=0
NO_SANDBOX=0
# Real-X preference (see the headless mechanics note above): use Xvfb when it
# is present unless explicitly disabled. The display transport is the only
# difference; every matrix capability is asserted exactly as before.
if [ "${FAKTOR_VSCODE_E2E_XVFB:-1}" = "1" ] && command -v Xvfb >/dev/null 2>&1; then
  USE_XVFB=1
else
  USE_XVFB=0
fi

TARBALL_SOURCE="${FAKTOR_VSCODE_TARBALL:-}"
EXPECTED_SHA="${FAKTOR_VSCODE_SHA256:-$VSCODE_SHA256}"
DOWNLOAD_URL="${FAKTOR_VSCODE_URL:-$VSCODE_URL}"

usage() {
  sed -n '2,/^set -/p' "$SELF" | sed '$d' | sed 's/^# \{0,1\}//'
}

die_usage() {
  echo "vscode-e2e: $1" >&2
  usage >&2
  exit 2
}

while [ "$#" -gt 0 ]; do
  case "$1" in
  --cli-only) CLI_ONLY=1; shift ;;
  --keep) KEEP=1; shift ;;
  --print-pin) PRINT_PIN=1; shift ;;
  --no-sandbox) NO_SANDBOX=1; shift ;;
  --selftest) SELFTEST=1; shift ;;
  --cache-dir | --work-dir | --out | --extension-dir | --vsix | --timeout | --timeout-factor | --timeout-cap | --calibration | --calibration-timeout)
    [ "$#" -ge 2 ] || die_usage "$1 needs a value"
    case "$1" in
    --cache-dir) CACHE_DIR="$2" ;;
    --work-dir) WORK_DIR="$2" ;;
    --out) OUT_FILE="$2" ;;
    --extension-dir) EXTENSION_DIR="$2" ;;
    --vsix) VSIX_PATH="$2"; VSIX_EXPLICIT=1 ;;
    --timeout) HOST_TIMEOUT="$2" ;;
    --timeout-factor) HOST_TIMEOUT_FACTOR="$2" ;;
    --timeout-cap) HOST_TIMEOUT_CAP="$2" ;;
    --calibration) CALIBRATION_RAW="$2" ;;
    --calibration-timeout) CALIBRATION_TIMEOUT="$2" ;;
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

json_escape() {
  printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g' -e 's/\t/\\t/g' | tr -d '\r\n'
}

iso_now() { date -u +%Y-%m-%dT%H:%M:%SZ; }

# Millisecond monotonic-ish wall clock (node is a hard dependency of the lane
# already: port picking, aggregation, evidence parsing). Refuses empty output
# so a missing node cannot silently produce a bogus control.
now_ms() {
  local ms
  ms="$(node -e 'process.stdout.write(String(Date.now()))' 2>/dev/null)"
  case "$ms" in
  '' | *[!0-9]*) return 1 ;;
  esac
  printf '%s' "$ms"
}

# Staged progress markers (see LOAD-AWARE HOST TIMEOUT above): one line per
# stage, appended to the run log and every cell log under the evidence work
# dir, so a kill is attributable to the stage that was running.
PROGRESS_RUN_LOG=""
stage_line() { # logfile stage [detail]
  local log="$1" stage="$2" detail="${3:-}" line
  line="$(iso_now) stage=$stage${detail:+ $detail}"
  [ -n "$log" ] && printf '%s\n' "$line" >>"$log" 2>/dev/null || true
  printf 'vscode-e2e: %s\n' "$line" >&2
}

last_stage_of() { # progress logfile
  [ -n "$1" ] && [ -f "$1" ] && tail -n 1 "$1" 2>/dev/null | cut -c1-240
}

# ------------------------------------------------------------------ records
STATE_REASON=""
STATE_MODE=""
STATE_SHA=""
STATE_CODE_VERSION=""
STATE_HOST_EVIDENCE_SHA=""
STATE_CHECKS_JSON="[]"
STATE_HOST_CHECKS_JSON="[]"
STATE_MATRIX_JSON="[]"
STATE_TIMEOUT_JSON=""
STARTED=""

# The timeout policy must be parseable and internally consistent before any
# host launch: zero/negative/unparseable values are hard refusals (never a
# silent fallback), matching the lane's imported-budget discipline.
validate_timeout_policy() {
  case "$HOST_TIMEOUT" in '' | *[!0-9]*) fail "timeout-policy-invalid: timeout must be positive integer seconds ('$HOST_TIMEOUT')" ;; esac
  case "$HOST_TIMEOUT_FACTOR" in '' | *[!0-9]*) fail "timeout-policy-invalid: factor must be positive integer ('$HOST_TIMEOUT_FACTOR')" ;; esac
  case "$HOST_TIMEOUT_CAP" in '' | *[!0-9]*) fail "timeout-policy-invalid: cap must be positive integer seconds ('$HOST_TIMEOUT_CAP')" ;; esac
  case "$CALIBRATION_TIMEOUT" in '' | *[!0-9]*) fail "timeout-policy-invalid: calibration timeout must be positive integer seconds ('$CALIBRATION_TIMEOUT')" ;; esac
  case "$CALIBRATION_NOMINAL_MS" in '' | *[!0-9]*) fail "timeout-policy-invalid: calibration nominal ms must be positive integer ('$CALIBRATION_NOMINAL_MS')" ;; esac
  case "$WAIT_FACTOR_MAX" in '' | *[!0-9]*) fail "timeout-policy-invalid: wait factor max must be positive integer ('$WAIT_FACTOR_MAX')" ;; esac
  [ "$HOST_TIMEOUT" -gt 0 ] || fail "timeout-policy-invalid: timeout must be > 0 ('$HOST_TIMEOUT')"
  [ "$HOST_TIMEOUT_FACTOR" -ge 1 ] || fail "timeout-policy-invalid: factor must be >= 1 ('$HOST_TIMEOUT_FACTOR')"
  [ "$HOST_TIMEOUT_CAP" -ge "$HOST_TIMEOUT" ] || fail "timeout-policy-invalid: cap ($HOST_TIMEOUT_CAP) must be >= timeout ($HOST_TIMEOUT)"
  [ "$CALIBRATION_TIMEOUT" -gt 0 ] || fail "timeout-policy-invalid: calibration timeout must be > 0 ('$CALIBRATION_TIMEOUT')"
  [ "$CALIBRATION_NOMINAL_MS" -gt 0 ] || fail "timeout-policy-invalid: calibration nominal ms must be > 0 ('$CALIBRATION_NOMINAL_MS')"
  [ "$WAIT_FACTOR_MAX" -ge 1 ] || fail "timeout-policy-invalid: wait factor max must be >= 1 ('$WAIT_FACTOR_MAX')"
  case "$CALIBRATION_RAW" in
  auto | off) ;;
  *) fail "timeout-policy-invalid: calibration must be auto or off ('$CALIBRATION_RAW')" ;;
  esac
}

# Recorded numbers are sanitized so an early failure (sha mismatch, missing
# pin) can still write a parseable record even with a bogus policy env.
json_num() { case "$1" in '' | *[!0-9]*) printf 'null' ;; *) printf '%s' "$1" ;; esac; }

STATE_TIMEOUT_JSON="$(printf '{"status":"pending","requested_seconds":%s,"effective_seconds":%s,"factor":%s,"hard_cap_seconds":%s,"calibration":{"status":"not-run"}}' \
  "$(json_num "$HOST_TIMEOUT")" "$(json_num "$HOST_TIMEOUT")" "$(json_num "$HOST_TIMEOUT_FACTOR")" "$(json_num "$HOST_TIMEOUT_CAP")")"

write_record() { # status
  local status="$1"
  local finished duration
  finished="$(iso_now)"
  duration=0
  if [ -n "$STARTED" ]; then
    local s f
    s="$(date -u -d "$STARTED" +%s 2>/dev/null || echo 0)"
    f="$(date -u -d "$finished" +%s 2>/dev/null || echo 0)"
    [ "$f" -ge "$s" ] && duration=$((f - s))
  fi
  local commit tree
  commit="${CI_COMMIT_SHA:-}"
  tree="${CI_COMMIT_TREE:-}"
  if command -v git >/dev/null 2>&1 && git -C "$ROOT" rev-parse --verify HEAD >/dev/null 2>&1; then
    commit="$(git -C "$ROOT" rev-parse HEAD 2>/dev/null || printf 'unknown')"
    tree="$(git -C "$ROOT" rev-parse 'HEAD^{tree}' 2>/dev/null || printf 'unknown')"
  fi
  [ -n "$commit" ] || commit="unknown"
  [ -n "$tree" ] || tree="unknown"
  local coverage does_not_prove
  if [ "$STATE_MODE" = "extension-host" ] || [ "$STATE_MODE" = "extension-host-vsix" ]; then
    coverage="real pinned VS Code Extension Host matrix: extension present+activated, faktor.* commands registered, compiled webview provider HTML carries strict CSP + composer/attachment surface; a real WebviewPanel is driven over CDP through light/dark/high-contrast themes, 240/320/480/800 px viewports, trusted keyboard-only submission, paste/drop bytes, dispose+reopen restoration, and dedicated --force-device-scale-factor 1.25/2 cells (per-capability methods in the matrix field)"
    if [ "$STATE_MODE" = "extension-host-vsix" ]; then
      coverage="$coverage; the host extension tree is extracted from the supplied VSIX"
    fi
    does_not_prove='["rendered pixels or screenshots","trusted OS clipboard/drag file payload injection (synthetic DataTransfer events at the real handlers)","physical workbench panel resize (top-level CDP device metrics calibrated to the requested webview width)","live daemon connectivity (the provider HTML is exercised in-process)"]'
  else
    coverage="pinned VS Code CLI only: --version pin check and isolated --install-extension/--list-extensions of the built VSIX"
    does_not_prove='["real Extension Host launch","activation","webview HTML generation","rendered pixels"]'
  fi
  mkdir -p "$(dirname "$OUT_FILE")"
  printf '{"schema":"faktor-vscode-e2e/v1","status":"%s","reason":"%s","mode":"%s","coverage":"%s","does_not_prove":%s,"pin":{"version":"%s","commit":"%s","url":"%s","sha256":"%s","sha256_verified":%s},"code_version":"%s","host_evidence_sha256":"%s","cli_checks":%s,"host_checks":%s,"matrix":%s,"timeout":%s,"commit":"%s","tree":"%s","runner":{"os":"%s","arch":"%s"},"started_at":"%s","finished_at":"%s","duration_seconds":%s}\n' \
    "$(json_escape "$status")" "$(json_escape "$STATE_REASON")" "$(json_escape "${STATE_MODE:-cli-only}")" \
    "$(json_escape "$coverage")" "$does_not_prove" \
    "$VSCODE_VERSION" "$VSCODE_COMMIT" "$(json_escape "$DOWNLOAD_URL")" "$EXPECTED_SHA" "$([ "$STATE_SHA" = verified ] && echo true || echo false)" \
    "$(json_escape "$STATE_CODE_VERSION")" "$STATE_HOST_EVIDENCE_SHA" "$STATE_CHECKS_JSON" "$STATE_HOST_CHECKS_JSON" "$STATE_MATRIX_JSON" "$STATE_TIMEOUT_JSON" \
    "$(json_escape "$commit")" "$(json_escape "$tree")" \
    "$(uname -s | tr '[:upper:]' '[:lower:]')" "$(uname -m)" \
    "$STARTED" "$finished" "$duration" >"$OUT_FILE.tmp.$$"
  mv "$OUT_FILE.tmp.$$" "$OUT_FILE"
  echo "vscode-e2e: recorded $OUT_FILE (status=$status mode=${STATE_MODE:-cli-only} reason=${STATE_REASON:-none})"
}

fail() {
  STATE_REASON="$1"
  write_record failed
  echo "vscode-e2e: FAIL: $1" >&2
  exit 1
}

# ------------------------------------------------------------- download/extract
has_sha256() { [ -n "$1" ] && printf '%s' "$1" | grep -qE '^[0-9a-f]{64}$'; }

# Portable SHA-256 helper: GNU `sha256sum` first (Linux lanes unchanged),
# then the POSIX `shasum -a 256` that macOS ships (the selftest and the
# pin check must run on darwin hosts too). Absence of both is a loud typed
# failure, never an empty digest that would silently fail every pin.
sha_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    echo "vscode-e2e: no sha256 tool (sha256sum/shasum) on PATH" >&2
    return 1
  fi
}

download_file() { # url dest
  local url="$1" dest="$2"
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --retry 3 --max-time 1800 -o "$dest.part" "$url" || return 1
  elif command -v wget >/dev/null 2>&1; then
    wget -q -O "$dest.part" "$url" || return 1
  elif command -v node >/dev/null 2>&1; then
    node -e '
      const { Readable } = require("node:stream");
      const { createWriteStream } = require("node:fs");
      const { pipeline } = require("node:stream/promises");
      (async () => {
        const res = await fetch(process.argv[1]);
        if (!res.ok) throw new Error("HTTP " + res.status);
        await pipeline(Readable.fromWeb(res.body), createWriteStream(process.argv[2]));
      })().catch((e) => { console.error(e.message); process.exit(1); });
    ' "$url" "$dest.part" || return 1
  else
    echo "vscode-e2e: no curl/wget/node downloader available" >&2
    return 1
  fi
  mv "$dest.part" "$dest"
}

resolve_tarball() {
  mkdir -p "$CACHE_DIR"
  local cached="$CACHE_DIR/code-stable-x64-${VSCODE_VERSION}.tar.gz"
  if [ -n "$TARBALL_SOURCE" ]; then
    [ -f "$TARBALL_SOURCE" ] || fail "tarball-missing: FAKTOR_VSCODE_TARBALL=$TARBALL_SOURCE"
    TARBALL="$TARBALL_SOURCE"
    TARBALL_FROM_CACHE=0
    stage_line "$PROGRESS_RUN_LOG" download-source "tarball=$TARBALL_SOURCE"
    return 0
  fi
  if [ -f "$cached" ]; then
    TARBALL="$cached"
    TARBALL_FROM_CACHE=1
    stage_line "$PROGRESS_RUN_LOG" download-cached "tarball=$cached"
    return 0
  fi
  stage_line "$PROGRESS_RUN_LOG" download-start "url=$DOWNLOAD_URL"
  echo "vscode-e2e: downloading pinned VS Code $VSCODE_VERSION from $DOWNLOAD_URL" >&2
  download_file "$DOWNLOAD_URL" "$cached" || fail "download-failed: $DOWNLOAD_URL"
  TARBALL="$cached"
  TARBALL_FROM_CACHE=1
  stage_line "$PROGRESS_RUN_LOG" download-done "tarball=$cached"
}

verify_tarball() {
  has_sha256 "$EXPECTED_SHA" || fail "pin-invalid: expected SHA-256 is not 64 lowercase hex ('$EXPECTED_SHA')"
  case "$DOWNLOAD_URL" in
  *"$VSCODE_COMMIT"*) ;;
  *) fail "pin-invalid: download URL does not embed the pinned commit $VSCODE_COMMIT" ;;
  esac
  local actual
  if ! actual="$(sha_of "$TARBALL")"; then
    fail "sha256-tool-missing: neither sha256sum nor shasum is on PATH"
  fi
  if [ "$actual" != "$EXPECTED_SHA" ]; then
    if [ "${TARBALL_FROM_CACHE:-0}" = "1" ]; then
      rm -f "$TARBALL"
    fi
    fail "sha256-mismatch: pinned '$EXPECTED_SHA' != actual '$actual' for $TARBALL"
  fi
  STATE_SHA="verified"
  stage_line "$PROGRESS_RUN_LOG" sha256-verified "sha256=$actual"
  echo "vscode-e2e: sha256 verified ($actual)" >&2
}

export_vscode() {
  local extracted="${FAKTOR_VSCODE_EXTRACTED:-}"
  if [ -n "$extracted" ]; then
    CODE_BIN="$extracted/bin/code"
    stage_line "$PROGRESS_RUN_LOG" unpack-skipped "pre-extracted=$extracted"
  else
    stage_line "$PROGRESS_RUN_LOG" unpack-start "tarball=$TARBALL"
    rm -rf "$WORK_DIR/vscode"
    mkdir -p "$WORK_DIR/vscode"
    tar -xzf "$TARBALL" -C "$WORK_DIR/vscode" || fail "extract-failed: $TARBALL"
    CODE_BIN="$WORK_DIR/vscode/VSCode-linux-x64/bin/code"
    stage_line "$PROGRESS_RUN_LOG" unpack-done "code=$CODE_BIN"
  fi
  [ -x "$CODE_BIN" ] || fail "code-binary-missing: $CODE_BIN"
}

# ------------------------------------------------------------------ CLI checks
CLI_CHECKS="[]"
append_cli_check() {
  local name="$1" ok="$2" detail="$3" entry
  entry="{\"name\":\"$(json_escape "$name")\",\"ok\":$ok,\"detail\":\"$(json_escape "$detail")\"}"
  if [ "$CLI_CHECKS" = "[]" ]; then
    CLI_CHECKS="[$entry]"
  else
    CLI_CHECKS="$(printf '%s' "$CLI_CHECKS" | sed 's/]$//'),$entry]"
  fi
}

code_cli() { # args...
  local -a extra=()
  if [ "$NO_SANDBOX" -eq 1 ] || [ "$(id -u)" -eq 0 ]; then
    extra=(--no-sandbox)
  fi
  # bash 3.2 (macOS /bin/bash) treats `"${extra[@]}"` on an EMPTY array as
  # unbound under `set -u`; the `+` guard expands nothing in that case.
  "$CODE_BIN" ${extra[@]+"${extra[@]}"} --user-data-dir "$WORK_DIR/cli-user-data" --extensions-dir "$WORK_DIR/cli-extensions" "$@"
}

run_cli_checks() {
  mkdir -p "$WORK_DIR/cli-user-data" "$WORK_DIR/cli-extensions"
  local version_out
  version_out="$(code_cli --version 2>&1)" || fail "code-version-failed: $version_out"
  local code_version code_commit
  code_version="$(printf '%s\n' "$version_out" | head -n 1)"
  code_commit="$(printf '%s\n' "$version_out" | sed -n '2p')"
  STATE_CODE_VERSION="$code_version"
  append_cli_check "version-pin" "$([ "$code_version" = "$VSCODE_VERSION" ] && echo true || echo false)" "code --version printed '$code_version' (pin $VSCODE_VERSION)"
  append_cli_check "commit-pin" "$([ "$code_commit" = "$VSCODE_COMMIT" ] && echo true || echo false)" "code --version printed commit '$code_commit'"
  if [ "$code_version" != "$VSCODE_VERSION" ]; then
    fail "version-mismatch: pinned $VSCODE_VERSION, got '$code_version'"
  fi
  if [ "$code_commit" != "$VSCODE_COMMIT" ]; then
    fail "commit-mismatch: pinned commit $VSCODE_COMMIT, got '$code_commit'"
  fi

  local listed
  listed="$(code_cli --list-extensions --show-versions 2>&1)" || fail "list-extensions-failed: $listed"
  append_cli_check "isolated-extension-listing" true "isolated profile listed: $(printf '%s' "$listed" | tr '\n' ',' | sed 's/,$//')"

  if [ -n "$VSIX_PATH" ] && [ -f "$VSIX_PATH" ]; then
    code_cli --install-extension "$VSIX_PATH" --force >/dev/null 2>&1 || fail "vsix-install-failed: $VSIX_PATH"
    listed="$(code_cli --list-extensions --show-versions 2>&1)"
    if printf '%s' "$listed" | grep -q '^faktor\.faktor@'; then
      append_cli_check "vsix-loaded" true "isolated profile loaded $(printf '%s' "$listed" | grep '^faktor\.faktor@' | tr '\n' ',')"
    else
      append_cli_check "vsix-loaded" false "faktor.faktor not present after --install-extension"
      fail "vsix-load-missing: faktor.faktor is not listed after install"
    fi
  else
    append_cli_check "vsix-loaded" true "no VSIX supplied; skipped (the lane that builds it owns this check)"
  fi
}

# --------------------------------------------------------------- host launch
# SINGLE SOURCE OF TRUTH for the pinned-host flag set: the calibration control
# and every matrix cell launch through launch_host below, which always builds
# its command from these args (no divergent flag set anywhere). `--verbose`
# keeps the app's own stdout/stderr attached to the capture file; without it
# the VS Code CLI detaches the app's stdio and an early app exit leaves an
# empty diagnostic. The sandbox flag is explicit per uid: root (or an explicit
# --no-sandbox) gets --no-sandbox, non-root gets none.
host_launch_args() {
  local args
  if [ "$USE_XVFB" -eq 1 ]; then
    # Xvfb provides the display; do not force Chromium's headless platform.
    args="--wait --verbose --disable-gpu --disable-dev-shm-usage"
  else
    args="--wait --verbose --headless --disable-gpu --ozone-platform=headless --disable-dev-shm-usage"
  fi
  if [ "$NO_SANDBOX" -eq 1 ] || [ "$(id -u)" -eq 0 ]; then
    args="$args --no-sandbox"
  fi
  printf '%s' "$args"
}

pick_free_port() {
  node -e 'const net=require("node:net");const s=net.createServer();s.on("error",()=>process.exit(1));s.listen(0,"127.0.0.1",()=>{process.stdout.write(String(s.address().port));s.close();});' 2>/dev/null
}

# VS Code's main-process IPC socket lives at <user-data-dir>/1.14-main.sock and
# the Linux sockaddr_un.sun_path limit is 107 bytes. A deep workspace (the CI
# checkout is /woodpecker/src/github.com/sbelakho2/faktor/target/certification/
# vscode-e2e/work/host-*) makes user-data-dir too long, the app dies right
# after startup with `listen EINVAL` in claimInstance and exits rc=0 with no
# host module and no evidence -- the calibration rc=0 mystery. Every launch
# therefore uses a short-lived runtime dir under TMPDIR (only the path string
# that reaches the socket matters, not where evidence/logs live). The cell dir
# keeps evidence/progress/stdout logs; the VS Code logs live in the runtime
# user-data dir and are read from there by host_failure_diagnostic. The socket
# path is validated up front so an over-long TMPDIR is a typed refusal, never
# another silent rc=0. Sets RUNTIME_DIR; never call it in a subshell, the
# typed refusal must exit the whole harness.
RUNTIME_DIR=""
make_runtime_dir() { # cell_dir
  local rt sock
  RUNTIME_DIR=""
  rt="$(mktemp -d "${TMPDIR:-/tmp}/fzk-vscode-e2e.XXXXXX")" || return 1
  mkdir -p "$rt/user-data" "$rt/extensions" || {
    rm -rf "$rt"
    return 1
  }
  sock="$rt/user-data/1.14-main.sock"
  if [ "${#sock}" -gt 107 ]; then
    rm -rf "$rt"
    fail "runtime-dir-too-long: VS Code IPC socket path is ${#sock} bytes (>107): $sock (set TMPDIR to a shorter path)"
  fi
  RUNTIME_DIR="$rt"
  return 0
}

# Launches the pinned Electron host for one bounded run through the SAME flag
# set (host_launch_args) and the SAME short runtime-dir/stdio capture path for
# the calibration control and every matrix cell. Callers pass per-run env as
# NAME=VALUE pairs, then `--`, then extra code args (CDP port, device scale
# factor). The helper adds the shared env, the per-uid sandbox decision, the
# display flag and the runtime user-data/extensions dirs: one launch path.
launch_host() { # timeout_s logfile runtime_dir dev_path tests_path [NAME=VALUE...] -- [code args...]
  local timeout_s="$1" log="$2" rt_dir="$3" dev_path="$4" tests_path="$5"
  shift 5
  local -a envp=(
    FAKTOR_VSCODE_E2E_PACKAGE_MODE="$HOST_PACKAGE_MODE"
  )
  [ "$USE_XVFB" -eq 1 ] && envp+=(DISPLAY="$XVFB_DISPLAY")
  local -a code_args=()
  local arg seen_sep=0
  for arg in "$@"; do
    if [ "$seen_sep" -eq 0 ] && [ "$arg" = "--" ]; then
      seen_sep=1
      continue
    fi
    if [ "$seen_sep" -eq 0 ]; then
      envp+=("$arg")
    else
      code_args+=("$arg")
    fi
  done
  local -a args
  # shellcheck disable=SC2206,SC2207 # args are a fixed internal word list
  args=($(host_launch_args))
  args+=(${code_args[@]+"${code_args[@]}"})
  args+=(--user-data-dir "$rt_dir/user-data" --extensions-dir "$rt_dir/extensions" \
    --extensionDevelopmentPath="$dev_path" --extensionTestsPath="$tests_path")
  run_host_process "$timeout_s" "$log" env ${envp[@]+"${envp[@]}"} "$CODE_BIN" "${args[@]}"
}

# ------------------------------------------------- child lifecycle + Xvfb
# Every spawned child is owned by the harness: the host runs in its own
# process group (setsid when present), Xvfb is tracked by pid, and both are
# TERM/KILL-reaped with a bounded escalation on every exit path plus at
# harness exit. A cell is never retried.
CURRENT_PGID=""
XVFB_PID=""
XVFB_DISPLAY=""
LAUNCHED_PID=""

kill_tree() { # pgid [deciseconds]
  local pgid="$1" grace="${2:-100}" waited=0
  [ -n "$pgid" ] || return 0
  kill -s TERM -- "-$pgid" 2>/dev/null || true
  while [ "$waited" -lt "$grace" ]; do
    kill -0 -- "-$pgid" 2>/dev/null || return 0
    sleep 0.1
    waited=$((waited + 1))
  done
  kill -s KILL -- "-$pgid" 2>/dev/null || true
  return 0
}

kill_xvfb() {
  local waited=0
  [ -n "$XVFB_PID" ] || return 0
  kill -s TERM "$XVFB_PID" 2>/dev/null || true
  while [ "$waited" -lt 50 ]; do
    if ! kill -0 "$XVFB_PID" 2>/dev/null; then
      wait "$XVFB_PID" 2>/dev/null || true
      XVFB_PID=""
      XVFB_DISPLAY=""
      return 0
    fi
    sleep 0.1
    waited=$((waited + 1))
  done
  kill -s KILL "$XVFB_PID" 2>/dev/null || true
  wait "$XVFB_PID" 2>/dev/null || true
  XVFB_PID=""
  XVFB_DISPLAY=""
}

cleanup_run_children() {
  kill_tree "${CURRENT_PGID:-}"
  CURRENT_PGID=""
  kill_xvfb
}
trap 'cleanup_run_children' EXIT
trap 'cleanup_run_children; exit 130' INT
trap 'cleanup_run_children; exit 143' TERM

# Xvfb with `-displayfd` (Xvfb >= 1.19): Xvfb itself picks a free display,
# writes the number to fd 3 and only then serves. The harness waits for the
# socket so "Xvfb up" is a real, attributable stage before electron spawn.
# A failure to come up is a typed refusal, never a silent fallback.
start_xvfb() { # display_file xvfb_log
  local display_file="$1" xvfb_log="$2" display="" waited=0
  : >"$display_file"
  Xvfb -displayfd 3 -screen 0 1280x1024x24 -nolisten tcp 3>"$display_file" >"$xvfb_log" 2>&1 &
  XVFB_PID=$!
  while [ "$waited" -lt 300 ]; do
    display="$(tr -d '[:space:]' <"$display_file" 2>/dev/null || true)"
    if [ -n "$display" ]; then
      break
    fi
    if ! kill -0 "$XVFB_PID" 2>/dev/null; then
      XVFB_PID=""
      return 1
    fi
    sleep 0.1
    waited=$((waited + 1))
  done
  if [ -z "$display" ]; then
    kill_xvfb
    return 1
  fi
  waited=0
  while [ "$waited" -lt 300 ]; do
    if [ -e "/tmp/.X11-unix/X$display" ]; then
      XVFB_DISPLAY=":$display"
      return 0
    fi
    if ! kill -0 "$XVFB_PID" 2>/dev/null; then
      XVFB_PID=""
      return 1
    fi
    sleep 0.1
    waited=$((waited + 1))
  done
  kill_xvfb
  return 1
}

# Launches a command in the background, captures its process group when it
# differs from the harness's own (setsid path) and waits for it. The return
# value is the command's, so timeout's 124/137 survive to the caller.
run_host_process() { # timeout_s logfile cmd...
  local timeout_s="$1" log="$2"
  shift 2
  local -a cmd=(timeout -k 15 "$timeout_s" "$@")
  if command -v setsid >/dev/null 2>&1; then
    cmd=(setsid "${cmd[@]}")
  fi
  "${cmd[@]}" >"$log" 2>&1 &
  LAUNCHED_PID=$!
  local pgid self_pgid
  pgid="$(ps -o pgid= -p "$LAUNCHED_PID" 2>/dev/null | tr -d ' ')"
  self_pgid="$(ps -o pgid= -p $$ 2>/dev/null | tr -d ' ')"
  if [ -n "$pgid" ] && [ "$pgid" != "$self_pgid" ]; then
    CURRENT_PGID="$pgid"
  else
    CURRENT_PGID=""
  fi
  local rc=0
  wait "$LAUNCHED_PID" || rc=$?
  kill_tree "${CURRENT_PGID:-}"
  CURRENT_PGID=""
  LAUNCHED_PID=""
  return "$rc"
}

reap_cell() { # workdir
  local marker="$1" pid
  for pid in $(pgrep -f -- "$marker" 2>/dev/null || true); do
    [ "$pid" = "$$" ] && continue
    kill -9 "$pid" 2>/dev/null || true
  done
}

sanitize_one_line() { tr '\r\n' '  ' | tr -s ' ' | cut -c1-400; }

# A host that exits without evidence must name its own cause. Capture rc, the
# staged progress markers, the host stdout/stderr tail, the newest VS Code
# main/renderer/extension-host logs and any crash dumps into
# $cell_dir/failure-diagnostic.txt (kept on failure), and fold a bounded
# one-line summary into the lane reason. The no-silent-pass rule is
# unchanged: this branch always fails the lane.
host_failure_diagnostic() { # cell_dir rc [user_data_dir]
  local cell_dir="$1"
  local rc="$2"
  local user_data="${3:-$cell_dir/user-data}"
  local diag_file="$cell_dir/failure-diagnostic.txt"
  local log_dir tail_line last_stage
  {
    echo "host rc=$rc (124 = load-aware timeout fired; 137/143 = kill after the timeout; 0 without evidence = the host exited before the test module recorded any result)"
    if [ -s "$cell_dir/progress.log" ]; then
      echo "--- progress markers (last 30)"
      tail -n 30 "$cell_dir/progress.log"
    fi
    echo "--- runtime user-data dir: $user_data (ipc socket $(( ${#user_data} + 15 )) bytes)"
    echo "--- stdout.log tail (host stdout/stderr; --verbose keeps the app attached)"
    tail -n 40 "$cell_dir/stdout.log" 2>/dev/null || echo "(no stdout.log)"
    log_dir="$(find "$user_data/logs" -mindepth 1 -maxdepth 1 -type d 2>/dev/null | sort | tail -n 1)"
    if [ -n "$log_dir" ]; then
      for f in main.log "window1/renderer.log" "window1/exthost.log" exthost.log; do
        [ -f "$log_dir/$f" ] || continue
        echo "--- $f tail"
        tail -n 25 "$log_dir/$f"
      done
    else
      echo "--- no VS Code logs under $user_data/logs"
    fi
    for crashes in "$user_data/Crashpad" "$user_data/crashdumps"; do
      [ -d "$crashes" ] || continue
      echo "--- crash artifacts under $crashes"
      find "$crashes" -type f 2>/dev/null | tail -n 20
    done
  } >"$diag_file" 2>&1
  tail_line="$(tail -n 25 "$cell_dir/stdout.log" 2>/dev/null | grep -v '^[[:space:]]*$' | tail -n 3 | sanitize_one_line)"
  [ -n "$tail_line" ] || tail_line="(host stdout empty)"
  last_stage="$(last_stage_of "$cell_dir/progress.log")"
  [ -n "$last_stage" ] || last_stage="(no markers)"
  printf 'diagnostic=%s last-stage=%s host-tail=%s' "$diag_file" "$last_stage" "$tail_line"
}

# Resolve the extension tree that the host (and the calibration control)
# runs against and rebuild it when src/ is newer than out/, exactly as the
# real cells did: a stale out/ must never masquerade as a host failure.
prepare_host_extension() {
  local host_dir="$HOST_EXTENSION_DIR"
  if [ -z "$host_dir" ]; then
    host_dir="$EXTENSION_DIR"
  fi
  [ -f "$host_dir/package.json" ] || fail "extension-dir-invalid: $host_dir/package.json is missing"
  [ -f "$EXTENSION_HOST_SMOKE" ] || fail "host-test-missing: $EXTENSION_HOST_SMOKE"
  [ -f "$CALIBRATION_HOST" ] || fail "host-test-missing: $CALIBRATION_HOST"
  # Source mode used to run whatever stale out/ existed: a src change (a new
  # contributed command) then produced a misleading host failure. Rebuild
  # whenever any src file is newer than out/extension.js. A VSIX-extracted
  # tree has no src/ and is used exactly as packaged.
  if [ -f "$host_dir/package.json" ] && [ -d "$host_dir/src" ] && command -v npm >/dev/null 2>&1; then
    if [ ! -f "$host_dir/out/extension.js" ] || find "$host_dir/src" -type f -newer "$host_dir/out/extension.js" | grep -q .; then
      echo "vscode-e2e: rebuilding the extension (src is newer than out)" >&2
      (cd "$host_dir" && npm run build >/dev/null 2>&1) || fail "extension-build-failed: npm run build"
    fi
  fi
  [ -f "$host_dir/out/extension.js" ] || fail "extension-not-built: $host_dir/out/extension.js is missing (run npm run build first)"
  HOST_EXTENSION_DIR="$host_dir"
}

# ------------------------------------------------- load-aware calibration
# One bounded launch of the SAME pinned Electron host through the SAME
# launch path/Xvfb facility, immediately before the real cells. It records
# when the test module became live and when the extension activated; the
# measured wall time to activation drives the derived per-cell timeout and
# the smoke module's wait factor. A calibration that cannot reach activation
# inside its bounded window fails the lane typed; it is never retried and no
# timeout is ever guessed.
run_calibration() {
  if [ "$CALIBRATION_RAW" = "off" ]; then
    stage_line "$PROGRESS_RUN_LOG" calibration-skipped "reason=off requested_timeout_s=$HOST_TIMEOUT"
    EFFECTIVE_HOST_TIMEOUT="$HOST_TIMEOUT"
    WAIT_FACTOR=1
    STATE_TIMEOUT_JSON="$(printf '{"status":"off","requested_seconds":%s,"effective_seconds":%s,"factor":%s,"hard_cap_seconds":%s,"calibration":{"status":"off"}}' \
      "$HOST_TIMEOUT" "$HOST_TIMEOUT" "$HOST_TIMEOUT_FACTOR" "$HOST_TIMEOUT_CAP")"
    return 0
  fi

  local cal_dir="$WORK_DIR/host-calibration"
  mkdir -p "$cal_dir" || fail "evidence-dir-unwritable: dir=$cal_dir"
  [ -w "$cal_dir" ] || fail "evidence-dir-unwritable: dir=$cal_dir"
  local progress="$cal_dir/progress.log"
  : >"$progress"
  local evidence="$cal_dir/evidence.json"
  rm -f "$evidence"
  make_runtime_dir "$cal_dir" || fail "runtime-dir-unavailable: mktemp failed under ${TMPDIR:-/tmp}"
  local rt_dir="$RUNTIME_DIR"

  local display="ozone-headless"
  [ "$USE_XVFB" -eq 1 ] && display="xvfb"
  stage_line "$progress" calibration-start "display=$display bounded=${CALIBRATION_TIMEOUT}s"
  stage_line "$PROGRESS_RUN_LOG" calibration-start "display=$display bounded=${CALIBRATION_TIMEOUT}s"

  if [ "$USE_XVFB" -eq 1 ]; then
    start_xvfb "$cal_dir/xvfb-display" "$cal_dir/xvfb.log" || {
      kill_xvfb
      stage_line "$progress" xvfb-failed "log=$cal_dir/xvfb.log"
      fail "xvfb-start-failed: calibration Xvfb never accepted connections (see $cal_dir/xvfb.log)"
    }
    stage_line "$progress" xvfb-up "display=$XVFB_DISPLAY pid=$XVFB_PID"
    stage_line "$PROGRESS_RUN_LOG" xvfb-up "cell=calibration display=$XVFB_DISPLAY"
  fi

  local start_ms
  start_ms="$(now_ms)" || fail "clock-unavailable: node wall-clock unavailable"
  stage_line "$progress" electron-spawn "timeout=${CALIBRATION_TIMEOUT}s runtime=$rt_dir code=$CODE_BIN"

  local rc=0
  launch_host "$CALIBRATION_TIMEOUT" "$cal_dir/stdout.log" "$rt_dir" \
    "$HOST_EXTENSION_DIR" "$CALIBRATION_HOST" \
    "FAKTOR_VSCODE_E2E_EVIDENCE=$evidence" \
    "FAKTOR_VSCODE_E2E_MATRIX=calibration" \
    "FAKTOR_VSCODE_E2E_CELL=calibration" \
    "FAKTOR_VSCODE_E2E_PROGRESS=$progress" || rc=$?
  kill_xvfb
  local stage_at_exit
  stage_at_exit="$(last_stage_of "$progress")"
  stage_line "$progress" host-exit "rc=$rc"

  if [ ! -s "$evidence" ]; then
    reap_cell "$rt_dir"
    local cal_diag
    cal_diag="$(host_failure_diagnostic "$cal_dir" "$rc" "$rt_dir/user-data")"
    echo "vscode-e2e: calibration host failure diagnostic ($cal_dir/failure-diagnostic.txt):" >&2
    sed -n '1,80p' "$cal_dir/failure-diagnostic.txt" >&2 2>/dev/null || true
    if [ "$rc" -eq 124 ]; then
      fail "extension-host-calibration-failed: calibration host rc=124 and no calibration evidence at $evidence within the bounded ${CALIBRATION_TIMEOUT}s window (last-stage='$stage_at_exit') $cal_diag (no silent pass, no timeout guessed, never retried)"
    fi
    fail "extension-host-calibration-failed: calibration host rc=$rc and no calibration evidence at $evidence (last-stage='$stage_at_exit') $cal_diag (no silent pass, never retried)"
  fi

  local cal_fields
  cal_fields="$(node -e '
    const fs = require("node:fs");
    const e = JSON.parse(fs.readFileSync(process.argv[1], "utf8"));
    const start = Number(process.argv[2]);
    const live = Number(e.host_module_live_at || 0);
    const act = Number(e.activation_at || 0);
    const control = Math.max(act, live) - start;
    const clean = (v) => String(v || "").replace(/[\r\n]+/g, " ").slice(0, 240);
    console.log(clean(e.status));
    console.log(String(Number.isFinite(control) ? control : -1));
    console.log(clean(e.activation_error));
  ' "$evidence" "$start_ms" 2>&1)" || fail "extension-host-calibration-failed: unparseable calibration evidence at $evidence ($cal_fields)"
  local cal_status control_ms activation_error
  cal_status="$(printf '%s\n' "$cal_fields" | sed -n 1p)"
  control_ms="$(printf '%s\n' "$cal_fields" | sed -n 2p)"
  activation_error="$(printf '%s\n' "$cal_fields" | sed -n 3p)"

  [ "$cal_status" = "calibrated" ] || fail "extension-host-calibration-failed: calibration evidence status='$cal_status' (expected calibrated) at $evidence (no silent pass)"
  case "$control_ms" in
  '' | *[!0-9]*)
    fail "extension-host-calibration-failed: calibration control is not a non-negative integer ('$control_ms') at $evidence (host clock vs harness clock)"
    ;;
  esac
  [ "$control_ms" -ge 0 ] || fail "extension-host-calibration-failed: negative calibration control ($control_ms ms) at $evidence"

  local control_s wait_factor derived
  control_s=$(((control_ms + 999) / 1000))
  wait_factor=$(((control_ms + CALIBRATION_NOMINAL_MS - 1) / CALIBRATION_NOMINAL_MS))
  [ "$wait_factor" -lt 1 ] && wait_factor=1
  [ "$wait_factor" -gt "$WAIT_FACTOR_MAX" ] && wait_factor="$WAIT_FACTOR_MAX"
  derived=$((control_s * HOST_TIMEOUT_FACTOR))
  EFFECTIVE_HOST_TIMEOUT="$HOST_TIMEOUT"
  if [ "$derived" -gt "$EFFECTIVE_HOST_TIMEOUT" ]; then
    EFFECTIVE_HOST_TIMEOUT="$derived"
  fi
  if [ "$EFFECTIVE_HOST_TIMEOUT" -gt "$HOST_TIMEOUT_CAP" ]; then
    EFFECTIVE_HOST_TIMEOUT="$HOST_TIMEOUT_CAP"
  fi
  CONTROL_MS="$control_ms"
  WAIT_FACTOR="$wait_factor"
  STATE_TIMEOUT_JSON="$(printf '{"status":"measured","requested_seconds":%s,"effective_seconds":%s,"factor":%s,"hard_cap_seconds":%s,"calibration":{"status":"measured","control_ms":%s,"control_seconds_ceil":%s,"wait_factor":%s,"nominal_ms":%s,"calibration_timeout_seconds":%s,"activation_error":"%s","evidence":"%s"}}' \
    "$HOST_TIMEOUT" "$EFFECTIVE_HOST_TIMEOUT" "$HOST_TIMEOUT_FACTOR" "$HOST_TIMEOUT_CAP" \
    "$control_ms" "$control_s" "$WAIT_FACTOR" "$CALIBRATION_NOMINAL_MS" "$CALIBRATION_TIMEOUT" \
    "$(json_escape "$activation_error")" "$(json_escape "$evidence")")"
  stage_line "$progress" calibration-done "control_ms=$control_ms control_s=$control_s wait_factor=$WAIT_FACTOR effective_timeout_s=$EFFECTIVE_HOST_TIMEOUT"
  stage_line "$PROGRESS_RUN_LOG" calibration-done "control_ms=$control_ms wait_factor=$WAIT_FACTOR effective_timeout_s=$EFFECTIVE_HOST_TIMEOUT cap=$HOST_TIMEOUT_CAP"
  if [ "$KEEP" -eq 0 ]; then
    rm -rf "$rt_dir"
  fi
}

run_extension_host() {
  local host_dir="$HOST_EXTENSION_DIR"
  if [ -z "$host_dir" ]; then
    host_dir="$EXTENSION_DIR"
  fi
  [ -f "$host_dir/package.json" ] || fail "extension-dir-invalid: $host_dir/package.json is missing"
  [ -f "$EXTENSION_HOST_SMOKE" ] || fail "host-test-missing: $EXTENSION_HOST_SMOKE"
  [ -f "$host_dir/out/extension.js" ] || fail "extension-not-built: $host_dir/out/extension.js is missing (run npm run build first)"

  local aggregate="$WORK_DIR/host-aggregate.json"
  local -a evidence_files=()
  local spec cell dpr mode
  for spec in "${MATRIX_CELLS[@]}"; do
    IFS=: read -r cell dpr mode <<<"$spec"
    local cell_dir="$WORK_DIR/host-$cell"
    mkdir -p "$cell_dir" || fail "evidence-dir-unwritable: cell=$cell dir=$cell_dir"
    [ -w "$cell_dir" ] || fail "evidence-dir-unwritable: cell=$cell dir=$cell_dir"
    local progress="$cell_dir/progress.log"
    : >"$progress"
    local evidence="$cell_dir/evidence.json"
    rm -f "$evidence"
    make_runtime_dir "$cell_dir" || fail "runtime-dir-unavailable: cell=$cell mktemp failed under ${TMPDIR:-/tmp}"
    local rt_dir="$RUNTIME_DIR"
    local cdp_port
    cdp_port="$(pick_free_port)" || fail "cdp-port-unavailable: no free loopback port for cell $cell"
    [ -n "$cdp_port" ] || fail "cdp-port-unavailable: no free loopback port for cell $cell"

    local display="ozone-headless"
    [ "$USE_XVFB" -eq 1 ] && display="xvfb"
    echo "vscode-e2e: cell $cell (mode=$mode dpr=$dpr cdp=$cdp_port display=$display) launching pinned Extension Host ($VSCODE_VERSION)" >&2
    stage_line "$progress" cell-start "mode=$mode dpr=$dpr cdp=$cdp_port display=$display"
    stage_line "$PROGRESS_RUN_LOG" cell-start "cell=$cell mode=$mode dpr=$dpr"

    if [ "$USE_XVFB" -eq 1 ]; then
      start_xvfb "$cell_dir/xvfb-display" "$cell_dir/xvfb.log" || {
        kill_xvfb
        stage_line "$progress" xvfb-failed "log=$cell_dir/xvfb.log"
        fail "xvfb-start-failed: cell=$cell Xvfb never accepted connections (see $cell_dir/xvfb.log)"
      }
      stage_line "$progress" xvfb-up "display=$XVFB_DISPLAY pid=$XVFB_PID"
    fi

    stage_line "$progress" electron-spawn "timeout=${EFFECTIVE_HOST_TIMEOUT}s requested=${HOST_TIMEOUT}s control_ms=${CONTROL_MS:-unmeasured} wait_factor=$WAIT_FACTOR runtime=$rt_dir code=$CODE_BIN"
    local -a code_args=(--remote-debugging-port="$cdp_port" --remote-allow-origins='*')
    if [ "$dpr" != "1" ]; then
      code_args+=(--force-device-scale-factor="$dpr")
    fi

    local rc=0
    launch_host "$EFFECTIVE_HOST_TIMEOUT" "$cell_dir/stdout.log" "$rt_dir" \
      "$host_dir" "$EXTENSION_HOST_SMOKE" \
      "FAKTOR_VSCODE_E2E_EVIDENCE=$evidence" \
      "FAKTOR_VSCODE_E2E_PIN_VERSION=$VSCODE_VERSION" \
      "FAKTOR_VSCODE_E2E_PIN_COMMIT=$VSCODE_COMMIT" \
      "FAKTOR_VSCODE_E2E_CDP_PORT=$cdp_port" \
      "FAKTOR_VSCODE_E2E_MATRIX=$mode" \
      "FAKTOR_VSCODE_E2E_CELL=$cell" \
      "FAKTOR_VSCODE_E2E_EXPECT_DPR=$dpr" \
      "FAKTOR_VSCODE_E2E_PROGRESS=$progress" \
      "FAKTOR_VSCODE_E2E_TIMEOUT_SECONDS=$EFFECTIVE_HOST_TIMEOUT" \
      "FAKTOR_VSCODE_E2E_WAIT_FACTOR=$WAIT_FACTOR" \
      "FAKTOR_VSCODE_E2E_CALIBRATION_MS=${CONTROL_MS:-0}" \
      -- ${code_args[@]+"${code_args[@]}"} || rc=$?
    kill_xvfb
    local stage_at_exit
    stage_at_exit="$(last_stage_of "$progress")"
    stage_line "$progress" host-exit "rc=$rc"

    if [ ! -s "$evidence" ]; then
      reap_cell "$rt_dir"
      local host_diag
      host_diag="$(host_failure_diagnostic "$cell_dir" "$rc" "$rt_dir/user-data")"
      echo "vscode-e2e: cell $cell host failure diagnostic ($cell_dir/failure-diagnostic.txt):" >&2
      sed -n '1,80p' "$cell_dir/failure-diagnostic.txt" >&2 2>/dev/null || true
      case "$rc" in
      124)
        fail "extension-host-timeout: cell=$cell host rc=124 and no evidence at $evidence (load-aware timeout fired at ${EFFECTIVE_HOST_TIMEOUT}s; requested ${HOST_TIMEOUT}s, control_ms=${CONTROL_MS:-unmeasured}, wait_factor=$WAIT_FACTOR; last-stage='$stage_at_exit') $host_diag (no silent pass, never retried)"
        ;;
      137)
        fail "extension-host-killed: cell=$cell host rc=137 and no evidence at $evidence (SIGKILL after the ${EFFECTIVE_HOST_TIMEOUT}s timeout grace; last-stage='$stage_at_exit') $host_diag (no silent pass, never retried)"
        ;;
      143)
        fail "extension-host-killed: cell=$cell host rc=143 and no evidence at $evidence (SIGTERM; last-stage='$stage_at_exit') $host_diag (no silent pass, never retried)"
        ;;
      0)
        fail "extension-host-no-evidence: cell=$cell host rc=0 and no evidence at $evidence (last-stage='$stage_at_exit') $host_diag (no silent pass)"
        ;;
      *)
        fail "extension-host-exited: cell=$cell host rc=$rc and no evidence at $evidence (last-stage='$stage_at_exit') $host_diag (no silent pass)"
        ;;
      esac
    fi
    local cell_status
    cell_status="$(node -e '
      const fs = require("node:fs");
      const e = JSON.parse(fs.readFileSync(process.argv[1], "utf8"));
      const bad = (e.checks || []).filter((c) => !c.ok).map((c) => c.name);
      if (e.status !== "passed" || bad.length > 0) {
        const err = e.error ? " error=" + String(e.error).split("\n")[0].slice(0, 300) : "";
        console.error("cell " + process.argv[2] + ": status=" + e.status + " failed=" + bad.join(",") + err);
        process.exit(1);
      }
      process.stdout.write("ok");
    ' "$evidence" "$cell" 2>&1)" || {
      reap_cell "$rt_dir"
      fail "extension-host-checks-failed: $cell_status"
    }
    evidence_files+=("$evidence")
    stage_line "$progress" verdict-passed "cell=$cell"
    echo "vscode-e2e: cell $cell checks passed" >&2
    if [ "$KEEP" -eq 0 ]; then
      rm -rf "$rt_dir"
    fi
  done

  node -e '
    const fs = require("node:fs");
    const crypto = require("node:crypto");
    const files = process.argv.slice(1);
    const checks = [];
    const cells = [];
    const hashes = [];
    for (const file of files) {
      const cell = file.split("/").slice(-2)[0].replace(/^host-/, "");
      const e = JSON.parse(fs.readFileSync(file, "utf8"));
      for (const entry of e.checks || []) {
        checks.push({ name: cell + ":" + entry.name, ok: entry.ok === true, detail: entry.detail || "" });
      }
      cells.push({
        cell,
        mode: e.matrix && e.matrix.cell ? e.matrix.cell.mode : null,
        expected_dpr: e.matrix && e.matrix.cell ? e.matrix.cell.expected_dpr : null,
        package_mode: e.matrix && e.matrix.cell ? e.matrix.cell.package_mode : null,
        cdp: e.matrix ? e.matrix.cdp : null,
        capabilities: e.matrix ? e.matrix.capabilities : null,
        not_emulatable: e.matrix ? e.matrix.not_emulatable : [],
      });
      hashes.push(crypto.createHash("sha256").update(fs.readFileSync(file)).digest("hex"));
    }
    const evidenceSha = crypto.createHash("sha256").update(hashes.join("\n")).digest("hex");
    process.stdout.write(JSON.stringify({ checks, cells, evidence_sha256: evidenceSha }));
  ' "${evidence_files[@]}" >"$aggregate" 2>"$WORK_DIR/host-aggregate.err" || fail "host-aggregate-failed: $(cat "$WORK_DIR/host-aggregate.err")"
  rm -f "$WORK_DIR/host-aggregate.err"

  STATE_HOST_EVIDENCE_SHA="$(node -e 'process.stdout.write(JSON.parse(require("node:fs").readFileSync(process.argv[1], "utf8")).evidence_sha256)' "$aggregate")"
  STATE_HOST_CHECKS_JSON="$(node -e 'process.stdout.write(JSON.stringify(JSON.parse(require("node:fs").readFileSync(process.argv[1], "utf8")).checks))' "$aggregate")"
  STATE_MATRIX_JSON="$(node -e '
    const fs = require("node:fs");
    const aggregate = JSON.parse(fs.readFileSync(process.argv[1], "utf8"));
    const matrix = {
      cells: aggregate.cells.map((cell) => ({
        cell: cell.cell,
        mode: cell.mode,
        expected_dpr: cell.expected_dpr,
        package_mode: cell.package_mode,
        cdp: cell.cdp,
        capabilities: cell.capabilities,
        not_emulatable: cell.not_emulatable,
      })),
    };
    process.stdout.write(JSON.stringify(matrix));
  ' "$aggregate")"
  STATE_MODE="extension-host"
  if [ "$HOST_PACKAGE_MODE" = "vsix" ]; then
    STATE_MODE="extension-host-vsix"
  fi
  echo "vscode-e2e: ${#evidence_files[@]} host cell(s) passed (aggregate evidence sha256 $STATE_HOST_EVIDENCE_SHA)" >&2
}

# ------------------------------------------------- packaged (VSIX) host mode
prepare_packaged_extension() {
  [ "$VSIX_EXPLICIT" -eq 1 ] || return 0
  command -v unzip >/dev/null 2>&1 || fail "vsix-extract-tool-missing: unzip is required for --vsix packaged host mode"
  rm -rf "$WORK_DIR/vsix-extracted"
  mkdir -p "$WORK_DIR/vsix-extracted"
  unzip -q "$VSIX_PATH" -d "$WORK_DIR/vsix-extracted" || fail "vsix-extract-failed: $VSIX_PATH"
  HOST_EXTENSION_DIR="$WORK_DIR/vsix-extracted/extension"
  [ -f "$HOST_EXTENSION_DIR/package.json" ] || fail "vsix-extension-missing: $HOST_EXTENSION_DIR/package.json"
  HOST_PACKAGE_MODE="vsix"
  echo "vscode-e2e: packaged host mode uses the extracted VSIX tree $HOST_EXTENSION_DIR" >&2
}

# ------------------------------------------------------------------ selftest
make_fake_code() { # rootdir
  mkdir -p "$1/bin"
  cat >"$1/bin/code" <<'FAKE'
#!/bin/sh
args="$*"
case "$args" in
*--extensionTestsPath*)
  case "${FAKTOR_VSCODE_E2E_MATRIX:-}" in
  calibration)
    if [ "${FAKE_CODE_CAL_TIMEOUT:-0}" = 1 ]; then exit 124; fi
    if [ "${FAKE_CODE_CAL_NO_EVIDENCE:-0}" = 1 ]; then exit 0; fi
    ms="$(node -e 'process.stdout.write(String(Date.now()))')"
    at=$((ms + ${FAKE_CODE_CAL_CONTROL_MS:-0}))
    printf '{"schema":"faktor-vscode-e2e-calibration/v1","status":"%s","host_module_live_at":%s,"activation_at":%s,"activated":"faktor.faktor","activation_error":""}\n' \
      "${FAKE_CODE_CAL_STATUS:-calibrated}" "$at" "$at" >"${FAKTOR_VSCODE_E2E_EVIDENCE:?}"
    exit 0
    ;;
  esac
  if [ "${FAKE_CODE_TIMEOUT:-0}" = 1 ]; then exit 124; fi
  if [ "${FAKE_CODE_KILLED:-0}" = 1 ]; then exit 137; fi
  if [ "${FAKE_CODE_NO_EVIDENCE:-0}" = 1 ]; then exit 0; fi
  if [ "${FAKE_CODE_RUNNING:-0}" = 1 ]; then
    printf '{"status":"running","checks":[],"error":""}\n' >"${FAKTOR_VSCODE_E2E_EVIDENCE:?}"
    exit 0
  fi
  if [ -n "${FAKTOR_VSCODE_E2E_PROGRESS:-}" ]; then
    printf '%s stage=host-module-live cell=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "${FAKTOR_VSCODE_E2E_CELL:-fake}" >>"$FAKTOR_VSCODE_E2E_PROGRESS"
    printf '%s stage=extension-activated cell=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "${FAKTOR_VSCODE_E2E_CELL:-fake}" >>"$FAKTOR_VSCODE_E2E_PROGRESS"
  fi
  ext_dir=""
  prev=""
  for a in "$@"; do
    case "$a" in --extensionDevelopmentPath=*) ext_dir="${a#--extensionDevelopmentPath=}" ;; esac
    [ "$prev" = "--extensionDevelopmentPath" ] && ext_dir="$a"
    prev="$a"
  done
  if [ "${FAKE_CODE_FAIL_CHECK:-0}" = 1 ]; then
    printf '{"status":"failed","matrix":{"cell":{"label":"%s","mode":"%s","expected_dpr":%s,"package_mode":"%s"}},"checks":[{"name":"webview-html-has:id=\\"composer\\"","ok":false,"detail":"planted"}]}\n' \
      "${FAKTOR_VSCODE_E2E_CELL:-fake}" "${FAKTOR_VSCODE_E2E_MATRIX:-full}" "${FAKTOR_VSCODE_E2E_EXPECT_DPR:-1}" "${FAKTOR_VSCODE_E2E_PACKAGE_MODE:-dev}" >"${FAKTOR_VSCODE_E2E_EVIDENCE:?}"
    exit 0
  fi
  printf '{"status":"passed","matrix":{"cell":{"label":"%s","mode":"%s","expected_dpr":%s,"package_mode":"%s"},"cdp":{"connected":true},"capabilities":{"renderer":{"emulated":true,"method":"fake-host"}},"not_emulatable":[]},"extension_path":"%s","checks":[{"name":"extension-present","ok":true},{"name":"webview-html-has:id=\\"composer\\"","ok":true},{"name":"matrix:webview-booted","ok":true}]}\n' \
    "${FAKTOR_VSCODE_E2E_CELL:-fake}" "${FAKTOR_VSCODE_E2E_MATRIX:-full}" "${FAKTOR_VSCODE_E2E_EXPECT_DPR:-1}" "${FAKTOR_VSCODE_E2E_PACKAGE_MODE:-dev}" "$ext_dir" >"${FAKTOR_VSCODE_E2E_EVIDENCE:?}"
  [ -n "${FAKTOR_VSCODE_E2E_PROGRESS:-}" ] && printf '%s stage=verdict-passed cell=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "${FAKTOR_VSCODE_E2E_CELL:-fake}" >>"$FAKTOR_VSCODE_E2E_PROGRESS"
  exit 0
  ;;
esac
case "$args" in
*--version*) printf '%s\n%s\nx64\n' "${FAKE_CODE_VERSION:-1.140.0}" "${FAKE_CODE_COMMIT:-07f806f999227108933c2e30515b26eecc1fda74}"; exit 0 ;;
esac
case "$args" in
*--list-extensions*) printf 'faktor.faktor@0.1.0\n'; exit 0 ;;
esac
exit 0
FAKE
  chmod +x "$1/bin/code"
}

selftest() {
  local tmp failures=0
  tmp="$(mktemp -d "${TMPDIR:-/tmp}/faktor-vscode-e2e-selftest.XXXXXX")"
  trap 'rm -rf "${tmp:-}"' EXIT
  # Marker for the child invocations this suite drives: every case runs the
  # REAL script against a fake `code` fixture, so the linux-x64 HOST guard
  # (a production pin fact, not a fixture fact) must not refuse the selftest
  # on a darwin/arm64 host. Production runs never carry the marker.
  export FAKTOR_VSCODE_E2E_SELFTEST=1
  make_fake_code "$tmp/extracted"
  mkdir -p "$tmp/extension/out"
  printf '{"name":"faktor","publisher":"faktor"}\n' >"$tmp/extension/package.json"
  printf 'module.exports = {};\n' >"$tmp/extension/out/extension.js"
  export FAKTOR_VSCODE_E2E_EXTENSION="$tmp/extension"
  mkdir -p "$tmp/pack/VSCode-linux-x64/bin"
  cp "$tmp/extracted/bin/code" "$tmp/pack/VSCode-linux-x64/bin/code"
  tar -czf "$tmp/code.tar.gz" -C "$tmp/pack" VSCode-linux-x64
  local sha
  if ! sha="$(sha_of "$tmp/code.tar.gz")"; then
    echo "vscode-e2e selftest: no sha256 tool (sha256sum/shasum) on PATH" >&2
    exit 1
  fi

  check() { # label rc
    if [ "$2" -eq 0 ]; then
      echo "vscode-e2e selftest ok: $1"
    else
      echo "vscode-e2e selftest FAIL: $1" >&2
      failures=$((failures + 1))
    fi
  }
  status_of() {
    node -e 'process.stdout.write(JSON.parse(require("node:fs").readFileSync(process.argv[1],"utf8")).status)' "$1" 2>/dev/null
  }
  reason_of() {
    node -e 'process.stdout.write(JSON.parse(require("node:fs").readFileSync(process.argv[1],"utf8")).reason||"")' "$1" 2>/dev/null
  }

  # 1. happy path: pinned tarball -> CLI checks -> simulated extension host
  out="$tmp/happy.json"
  FAKTOR_VSCODE_TARBALL="$tmp/code.tar.gz" FAKTOR_VSCODE_SHA256="$sha" \
    bash "$SELF" --keep --out "$out" --cache-dir "$tmp/cache" --work-dir "$tmp/work-happy" \
    >"$tmp/happy.stdout" 2>"$tmp/happy.stderr"
  rc=$?
  check "happy pinned tarball passes" "$([ "$rc" -eq 0 ] && echo 0 || echo 1)"
  check "happy status passed" "$([ "$(status_of "$out")" = passed ] && echo 0 || echo 1)"
  check "happy mode extension-host" "$(node -e 'process.exit(JSON.parse(require("node:fs").readFileSync(process.argv[1],"utf8")).mode==="extension-host"?0:1)' "$out" 2>/dev/null && echo 0 || echo 1)"
  check "happy records does_not_prove" "$(grep -q 'does_not_prove' "$out" && echo 0 || echo 1)"
  check "happy aggregates all matrix cells" "$(node -e 'process.exit(JSON.parse(require("node:fs").readFileSync(process.argv[1],"utf8")).matrix.cells.length===3?0:1)' "$out" 2>/dev/null && echo 0 || echo 1)"
  check "happy records per-capability methods" "$(node -e 'const m=JSON.parse(require("node:fs").readFileSync(process.argv[1],"utf8")).matrix.cells;process.exit(m[0].capabilities&&m[0].capabilities.renderer?0:1)' "$out" 2>/dev/null && echo 0 || echo 1)"
  check "happy records measured calibration + floor timeout" "$(node -e 'const t=JSON.parse(require("node:fs").readFileSync(process.argv[1],"utf8")).timeout;process.exit(t.effective_seconds===600&&t.calibration.status==="measured"&&t.calibration.wait_factor>=1?0:1)' "$out" 2>/dev/null && echo 0 || echo 1)"
  check "happy staged progress markers" "$(grep -q 'stage=download-source' "$tmp/work-happy/progress.log" && grep -q 'stage=unpack-done' "$tmp/work-happy/progress.log" && grep -q 'stage=calibration-done' "$tmp/work-happy/progress.log" && grep -q 'stage=extension-activated' "$tmp/work-happy/host-full/progress.log" && grep -q 'stage=verdict-passed' "$tmp/work-happy/host-full/progress.log" && echo 0 || echo 1)"

  # 2. tampered tarball is refused before extraction
  out="$tmp/tampered.json"
  FAKTOR_VSCODE_TARBALL="$tmp/code.tar.gz" FAKTOR_VSCODE_SHA256="$(printf 'a%.0s' $(seq 1 64))" \
    bash "$SELF" --out "$out" --cache-dir "$tmp/cache" --work-dir "$tmp/work-tampered" \
    >"$tmp/tampered.stdout" 2>"$tmp/tampered.stderr"
  rc=$?
  check "tampered tarball fails" "$([ "$rc" -eq 1 ] && echo 0 || echo 1)"
  check "tampered reason sha256-mismatch" "$(printf '%s' "$(reason_of "$out")" | grep -q '^sha256-mismatch' && echo 0 || echo 1)"

  # 3. missing host evidence is a failure, never a silent pass
  out="$tmp/no-evidence.json"
  FAKTOR_VSCODE_TARBALL="$tmp/code.tar.gz" FAKTOR_VSCODE_SHA256="$sha" FAKE_CODE_NO_EVIDENCE=1 \
    bash "$SELF" --out "$out" --cache-dir "$tmp/cache" --work-dir "$tmp/work-noev" \
    >"$tmp/noev.stdout" 2>"$tmp/noev.stderr"
  rc=$?
  check "missing host evidence fails" "$([ "$rc" -eq 1 ] && echo 0 || echo 1)"
  check "missing evidence reason" "$(printf '%s' "$(reason_of "$out")" | grep -q '^extension-host-no-evidence' && echo 0 || echo 1)"
  check "missing evidence captures diagnostic" "$(printf '%s' "$(reason_of "$out")" | grep -q 'failure-diagnostic.txt' && echo 0 || echo 1)"
  check "missing evidence diagnostic file written" "$([ -s "$tmp/work-noev/host-full/failure-diagnostic.txt" ] && echo 0 || echo 1)"

  # 3b. a host that starts the test module and dies before a verdict
  #     ('running' evidence) fails the lane as well: the start marker is
  #     diagnostics, never a silent pass.
  out="$tmp/running.json"
  FAKTOR_VSCODE_TARBALL="$tmp/code.tar.gz" FAKTOR_VSCODE_SHA256="$sha" FAKE_CODE_RUNNING=1 \
    bash "$SELF" --out "$out" --cache-dir "$tmp/cache" --work-dir "$tmp/work-running" \
    >"$tmp/running.stdout" 2>"$tmp/running.stderr"
  rc=$?
  check "running evidence fails" "$([ "$rc" -eq 1 ] && echo 0 || echo 1)"
  check "running evidence reason" "$(printf '%s' "$(reason_of "$out")" | grep -q 'extension-host-checks-failed.*status=running' && echo 0 || echo 1)"

  # 3c. a calibration host that cannot reach activation inside its bounded
  #     window fails typed; the lane never guesses a timeout and never retries.
  out="$tmp/cal-timeout.json"
  FAKTOR_VSCODE_TARBALL="$tmp/code.tar.gz" FAKTOR_VSCODE_SHA256="$sha" FAKE_CODE_CAL_TIMEOUT=1 \
    FAKTOR_VSCODE_E2E_CALIBRATION_TIMEOUT=3 \
    bash "$SELF" --out "$out" --cache-dir "$tmp/cache" --work-dir "$tmp/work-cal-timeout" \
    >"$tmp/calt.stdout" 2>"$tmp/calt.stderr"
  rc=$?
  check "calibration timeout fails" "$([ "$rc" -eq 1 ] && echo 0 || echo 1)"
  check "calibration timeout typed reason" "$(printf '%s' "$(reason_of "$out")" | grep -q '^extension-host-calibration-failed' && echo 0 || echo 1)"
  check "calibration timeout diagnostic written" "$([ -s "$tmp/work-cal-timeout/host-calibration/failure-diagnostic.txt" ] && echo 0 || echo 1)"
  check "calibration timeout stage markers recorded" "$(grep -q 'stage=electron-spawn' "$tmp/work-cal-timeout/host-calibration/progress.log" && echo 0 || echo 1)"

  # 3d. load-aware derivation: a slow control raises the effective timeout
  #     above the requested floor (factor 4 x ~300 s control -> ~1204 s) and
  #     the raised value is recorded in the JSON record.
  out="$tmp/derived.json"
  FAKTOR_VSCODE_TARBALL="$tmp/code.tar.gz" FAKTOR_VSCODE_SHA256="$sha" FAKE_CODE_CAL_CONTROL_MS=300000 \
    FAKTOR_VSCODE_E2E_TIMEOUT_FACTOR=4 FAKTOR_VSCODE_E2E_TIMEOUT_CAP=2400 \
    bash "$SELF" --out "$out" --cache-dir "$tmp/cache" --work-dir "$tmp/work-derived" \
    >"$tmp/derived.stdout" 2>"$tmp/derived.stderr"
  rc=$?
  check "derived timeout passes" "$([ "$rc" -eq 0 ] && echo 0 || echo 1)"
  check "derived timeout above requested floor and within cap" "$(node -e 'const t=JSON.parse(require("node:fs").readFileSync(process.argv[1],"utf8")).timeout;process.exit(t.effective_seconds>600&&t.effective_seconds<=2400&&t.calibration.control_ms>=300000&&t.calibration.control_seconds_ceil>=300?0:1)' "$out" 2>/dev/null && echo 0 || echo 1)"

  # 3e. a cell that times out (rc 124) is a distinct typed reason with the
  #     captured diagnostic and last stage folded in; never a silent pass.
  out="$tmp/cell-timeout.json"
  FAKTOR_VSCODE_TARBALL="$tmp/code.tar.gz" FAKTOR_VSCODE_SHA256="$sha" FAKE_CODE_TIMEOUT=1 \
    bash "$SELF" --out "$out" --cache-dir "$tmp/cache" --work-dir "$tmp/work-cell-timeout" \
    >"$tmp/cellto.stdout" 2>"$tmp/cellto.stderr"
  rc=$?
  check "cell timeout fails" "$([ "$rc" -eq 1 ] && echo 0 || echo 1)"
  check "cell timeout typed reason" "$(printf '%s' "$(reason_of "$out")" | grep -q '^extension-host-timeout' && echo 0 || echo 1)"
  check "cell timeout folds diagnostic + last stage" "$(printf '%s' "$(reason_of "$out")" | grep -q 'failure-diagnostic.txt' && printf '%s' "$(reason_of "$out")" | grep -q 'last-stage=' && echo 0 || echo 1)"

  # 3f. a killed cell (rc 137) is its own typed reason.
  out="$tmp/cell-killed.json"
  FAKTOR_VSCODE_TARBALL="$tmp/code.tar.gz" FAKTOR_VSCODE_SHA256="$sha" FAKE_CODE_KILLED=1 \
    bash "$SELF" --out "$out" --cache-dir "$tmp/cache" --work-dir "$tmp/work-cell-killed" \
    >"$tmp/cellk.stdout" 2>"$tmp/cellk.stderr"
  rc=$?
  check "cell killed fails" "$([ "$rc" -eq 1 ] && echo 0 || echo 1)"
  check "cell killed typed reason" "$(printf '%s' "$(reason_of "$out")" | grep -q '^extension-host-killed' && echo 0 || echo 1)"

  # 3g. a calibration evidence with a non-calibrated status is refused typed.
  out="$tmp/cal-status.json"
  FAKTOR_VSCODE_TARBALL="$tmp/code.tar.gz" FAKTOR_VSCODE_SHA256="$sha" FAKE_CODE_CAL_STATUS=running \
    bash "$SELF" --out "$out" --cache-dir "$tmp/cache" --work-dir "$tmp/work-cal-status" \
    >"$tmp/calst.stdout" 2>"$tmp/calst.stderr"
  rc=$?
  check "calibration bad status fails" "$([ "$rc" -eq 1 ] && echo 0 || echo 1)"
  check "calibration bad status typed reason" "$(printf '%s' "$(reason_of "$out")" | grep -q '^extension-host-calibration-failed' && echo 0 || echo 1)"

  # 3h. a bogus timeout policy is refused before any host launch.
  out="$tmp/policy.json"
  FAKTOR_VSCODE_TARBALL="$tmp/code.tar.gz" FAKTOR_VSCODE_SHA256="$sha" FAKTOR_VSCODE_E2E_TIMEOUT_FACTOR=zero \
    bash "$SELF" --out "$out" --cache-dir "$tmp/cache" --work-dir "$tmp/work-policy" \
    >"$tmp/policy.stdout" 2>"$tmp/policy.stderr"
  rc=$?
  check "invalid timeout policy fails" "$([ "$rc" -eq 1 ] && echo 0 || echo 1)"
  check "invalid timeout policy typed reason" "$(printf '%s' "$(reason_of "$out")" | grep -q '^timeout-policy-invalid' && echo 0 || echo 1)"

  # 3i. a runtime dir whose VS Code IPC socket path exceeds unix(7)'s
  #     sockaddr_un limit is refused typed before any host launch. This is
  #     the deep-CI-workspace class where the app died with `listen EINVAL`
  #     and exited rc=0 with no evidence; it must never pass silently.
  out="$tmp/long-runtime.json"
  long_tmp="$tmp/$(printf 'p%.0s' $(seq 1 110))"
  mkdir -p "$long_tmp"
  TMPDIR="$long_tmp" FAKTOR_VSCODE_TARBALL="$tmp/code.tar.gz" FAKTOR_VSCODE_SHA256="$sha" \
    bash "$SELF" --out "$out" --cache-dir "$tmp/cache" --work-dir "$tmp/work-long-runtime" \
    >"$tmp/longrt.stdout" 2>"$tmp/longrt.stderr"
  rc=$?
  check "over-long runtime path fails" "$([ "$rc" -eq 1 ] && echo 0 || echo 1)"
  check "over-long runtime typed reason" "$(printf '%s' "$(reason_of "$out")" | grep -q '^runtime-dir-too-long' && echo 0 || echo 1)"

  # 4. a failing host check fails the lane
  out="$tmp/failed-check.json"
  FAKTOR_VSCODE_TARBALL="$tmp/code.tar.gz" FAKTOR_VSCODE_SHA256="$sha" FAKE_CODE_FAIL_CHECK=1 \
    bash "$SELF" --out "$out" --cache-dir "$tmp/cache" --work-dir "$tmp/work-fail" \
    >"$tmp/fail.stdout" 2>"$tmp/fail.stderr"
  rc=$?
  check "failing host check fails" "$([ "$rc" -eq 1 ] && echo 0 || echo 1)"
  check "failing host reason" "$(printf '%s' "$(reason_of "$out")" | grep -q '^extension-host-checks-failed' && echo 0 || echo 1)"

  # 5. wrong version is refused
  out="$tmp/wrong-version.json"
  FAKTOR_VSCODE_TARBALL="$tmp/code.tar.gz" FAKTOR_VSCODE_SHA256="$sha" FAKE_CODE_VERSION="1.139.0" \
    bash "$SELF" --out "$out" --cache-dir "$tmp/cache" --work-dir "$tmp/work-version" \
    >"$tmp/ver.stdout" 2>"$tmp/ver.stderr"
  rc=$?
  check "version mismatch fails" "$([ "$rc" -eq 1 ] && echo 0 || echo 1)"
  check "version mismatch reason" "$(printf '%s' "$(reason_of "$out")" | grep -q '^version-mismatch' && echo 0 || echo 1)"

  # 6. --cli-only records the reduced coverage without pretending E2E ran
  out="$tmp/cli-only.json"
  FAKTOR_VSCODE_TARBALL="$tmp/code.tar.gz" FAKTOR_VSCODE_SHA256="$sha" \
    bash "$SELF" --cli-only --out "$out" --cache-dir "$tmp/cache" --work-dir "$tmp/work-cli" \
    >"$tmp/cli.stdout" 2>"$tmp/cli.stderr"
  rc=$?
  check "cli-only passes" "$([ "$rc" -eq 0 ] && echo 0 || echo 1)"
  check "cli-only mode recorded" "$(node -e 'process.exit(JSON.parse(require("node:fs").readFileSync(process.argv[1],"utf8")).mode==="cli-only"?0:1)' "$out" 2>/dev/null && echo 0 || echo 1)"
  check "cli-only does_not_prove names host" "$(grep -q 'Extension Host launch' "$out" && echo 0 || echo 1)"

  # 7. the extension-host test module and the calibration control are valid
  node --check "$EXTENSION_HOST_SMOKE" >/dev/null 2>&1
  check "extension-host-smoke.cjs parses" "$?"
  node --check "$CALIBRATION_HOST" >/dev/null 2>&1
  check "calibration-host.cjs parses" "$?"

  # 8. --vsix packaged mode extracts the archive and runs the host matrix from
  #    that tree (never silently from the dev checkout). The pinned CI image
  #    ships no `unzip`, so when it is absent the case asserts the typed
  #    fail-closed refusal instead of a silent downgrade.
  if command -v zip >/dev/null 2>&1 || command -v python3 >/dev/null 2>&1; then
    mkdir -p "$tmp/vsix-src/extension/out" "$tmp/vsix-src/extension/media"
    printf '{"name":"faktor","publisher":"faktor"}\n' >"$tmp/vsix-src/extension/package.json"
    printf 'module.exports = {};\n' >"$tmp/vsix-src/extension/out/extension.js"
    printf '' >"$tmp/vsix-src/extension/media/chat.css"
    if command -v zip >/dev/null 2>&1; then
      (cd "$tmp/vsix-src" && zip -qr "$tmp/fake.vsix" extension)
    else
      (cd "$tmp/vsix-src" && python3 -c 'import sys,zipfile; z=zipfile.ZipFile(sys.argv[1],"w"); [z.write(p) for p in ["extension/package.json","extension/out/extension.js","extension/media/chat.css"]]; z.close()' "$tmp/fake.vsix")
    fi
    if command -v unzip >/dev/null 2>&1; then
      out="$tmp/vsix.json"
      FAKTOR_VSCODE_TARBALL="$tmp/code.tar.gz" FAKTOR_VSCODE_SHA256="$sha" \
        bash "$SELF" --vsix "$tmp/fake.vsix" --keep --out "$out" --cache-dir "$tmp/cache" --work-dir "$tmp/work-vsix" \
        >"$tmp/vsix.stdout" 2>"$tmp/vsix.stderr"
      rc=$?
      check "packaged vsix mode passes" "$([ "$rc" -eq 0 ] && echo 0 || echo 1)"
      check "packaged vsix mode recorded" "$(node -e 'process.exit(JSON.parse(require("node:fs").readFileSync(process.argv[1],"utf8")).mode==="extension-host-vsix"?0:1)' "$out" 2>/dev/null && echo 0 || echo 1)"
      check "packaged vsix host used extracted tree" "$(grep -q 'vsix-extracted' "$tmp/work-vsix/host-full/evidence.json" && echo 0 || echo 1)"
    else
      out="$tmp/vsix-no-unzip.json"
      FAKTOR_VSCODE_TARBALL="$tmp/code.tar.gz" FAKTOR_VSCODE_SHA256="$sha" \
        bash "$SELF" --vsix "$tmp/fake.vsix" --out "$out" --cache-dir "$tmp/cache" --work-dir "$tmp/work-vsix-no-unzip" \
        >"$tmp/vsix-no-unzip.stdout" 2>"$tmp/vsix-no-unzip.stderr"
      rc=$?
      check "vsix without unzip fails closed" "$([ "$rc" -eq 1 ] && echo 0 || echo 1)"
      check "vsix missing unzip reason" "$(printf '%s' "$(reason_of "$out")" | grep -q '^vsix-extract-tool-missing' && echo 0 || echo 1)"
    fi
  else
    echo "vscode-e2e selftest skip: packaged-vsix case needs zip or python3 to build a test archive"
  fi

  if [ "$failures" -gt 0 ]; then
    echo "vscode-e2e selftest: FAIL ($failures case(s))" >&2
    return 1
  fi
  echo "vscode-e2e selftest: PASS (pin tamper, no-evidence, running, calibration-timeout, derive/timeout-score, cell-timeout/killed, bad-status, policy, version, cli-only and packaged-VSIX cases exercised)"
  return 0
}

if [ "$SELFTEST" -eq 1 ]; then
  selftest
  exit $?
fi

if [ "$PRINT_PIN" -eq 1 ]; then
  printf '{"version":"%s","commit":"%s","build":"%s","url":"%s","sha256":"%s"}\n' \
    "$VSCODE_VERSION" "$VSCODE_COMMIT" "$VSCODE_BUILD" "$VSCODE_URL" "$VSCODE_SHA256"
  exit 0
fi

if [ "${FAKTOR_VSCODE_E2E_SELFTEST:-0}" != "1" ] &&
  { [ "$(uname -s)" != "Linux" ] || [ "$(uname -m)" != "x86_64" ]; }; then
  echo "vscode-e2e: linux-x64 only (got $(uname -s)/$(uname -m))" >&2
  exit 2
fi

STARTED="$(iso_now)"
mkdir -p "$WORK_DIR" "$CACHE_DIR" || fail "work-dir-unwritable: $WORK_DIR / $CACHE_DIR"
# Evidence/log paths must be absolute so a cell launched through
# `timeout`/`setsid` always points at the same directory the harness checks.
WORK_DIR="$(cd "$WORK_DIR" && pwd)" || fail "work-dir-unusable: $WORK_DIR"
CACHE_DIR="$(cd "$CACHE_DIR" && pwd)" || fail "cache-dir-unusable: $CACHE_DIR"
PROGRESS_RUN_LOG="$WORK_DIR/progress.log"
: >"$PROGRESS_RUN_LOG" || fail "progress-log-unwritable: $PROGRESS_RUN_LOG"
stage_line "$PROGRESS_RUN_LOG" run-start "runner=$(uname -s)/$(uname -m) mode=$([ "$CLI_ONLY" -eq 1 ] && echo cli-only || echo extension-host)"
validate_timeout_policy

if [ -n "$VSIX_PATH" ] && [ ! -f "$VSIX_PATH" ]; then
  fail "vsix-missing: $VSIX_PATH"
fi
if [ -z "$VSIX_PATH" ] && [ -f "$ROOT/apps/vscode/faktor-ci.vsix" ]; then
  VSIX_PATH="$ROOT/apps/vscode/faktor-ci.vsix"
fi

if [ -n "${FAKTOR_VSCODE_EXTRACTED:-}" ]; then
  # Test/operator override: a pre-extracted pinned tree (sha256_verified=false).
  CODE_BIN="$FAKTOR_VSCODE_EXTRACTED/bin/code"
  [ -x "$CODE_BIN" ] || fail "code-binary-missing: $CODE_BIN"
else
  resolve_tarball
  verify_tarball
  export_vscode
fi

run_cli_checks
STATE_CHECKS_JSON="$CLI_CHECKS"
stage_line "$PROGRESS_RUN_LOG" cli-checks-done
if [ "$CLI_ONLY" -eq 1 ]; then
  STATE_MODE="cli-only"
else
  prepare_packaged_extension
  # Name the mode before any host launch so a failed record never claims
  # cli-only coverage it did not run.
  STATE_MODE="extension-host"
  if [ "$HOST_PACKAGE_MODE" = "vsix" ]; then
    STATE_MODE="extension-host-vsix"
  fi
  prepare_host_extension
  run_calibration
  run_extension_host
fi
stage_line "$PROGRESS_RUN_LOG" run-complete "effective_timeout_s=$EFFECTIVE_HOST_TIMEOUT wait_factor=$WAIT_FACTOR"
write_record passed

if [ "$KEEP" -eq 0 ]; then
  rm -rf "$WORK_DIR"
fi
exit 0

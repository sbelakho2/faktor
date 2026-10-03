#!/usr/bin/env bash
# Faktor VS Code Extension Host / webview E2E smoke lane.
#
# Downloads a PINNED linux-x64 VS Code build, verifies the recorded SHA-256,
# and runs a REAL Extension Host against `--extensionDevelopmentPath=apps/vscode`
# with an isolated `--user-data-dir` / `--extensions-dir`. The host-side test
# module (`scripts/vscode-e2e/extension-host-smoke.cjs`) executes inside the
# pinned VS Code extension host and asserts:
#   * the `faktor.faktor` extension is present and activates,
#   * the contributed `faktor.*` commands are registered (incl. openChat),
#   * the compiled Faktor webview provider renders HTML carrying the strict
#     CSP plus the composer and attachment surface elements,
# and writes `target/certification/vscode-e2e.json` with the exact coverage.
#
# COVERAGE / WHAT THIS DOES NOT PROVE (recorded verbatim in the JSON record):
#   * It does NOT take screenshots or compare rendered pixels; there is no
#     image/visual-baseline coverage.
#   * It does NOT display or drive a visible workbench UI/WebviewView; the
#     webview HTML is produced by the extension's own compiled provider
#     running in the real extension host.
#   * It does NOT talk to a live daemon; daemon behavior is covered by the
#     Faktor Node panel selftest and the protocol test suites.
#
# Headless mechanics: the desktop VS Code workbench needs a display; on a
# headless runner the script launches Electron with `--ozone-platform=headless
# --disable-gpu` (plus `--headless`, which Chromium itself receives) so the
# extension host runs without X. Running as root (`id -u` = 0) adds
# `--no-sandbox` (Electron refuses to start as root otherwise).
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
#                              [--timeout SECONDS] [--selftest]
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
EXTENSION_HOST_SMOKE="$SCRIPT_DIR/vscode-e2e/extension-host-smoke.cjs"
CLI_ONLY=0
KEEP=0
SELFTEST=0
PRINT_PIN=0
NO_SANDBOX=0

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
  --cache-dir | --work-dir | --out | --extension-dir | --vsix | --timeout)
    [ "$#" -ge 2 ] || die_usage "$1 needs a value"
    case "$1" in
    --cache-dir) CACHE_DIR="$2" ;;
    --work-dir) WORK_DIR="$2" ;;
    --out) OUT_FILE="$2" ;;
    --extension-dir) EXTENSION_DIR="$2" ;;
    --vsix) VSIX_PATH="$2" ;;
    --timeout) HOST_TIMEOUT="$2" ;;
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

# ------------------------------------------------------------------ records
STATE_REASON=""
STATE_MODE=""
STATE_SHA=""
STATE_CODE_VERSION=""
STATE_HOST_EVIDENCE_SHA=""
STATE_CHECKS_JSON="[]"
STATE_HOST_CHECKS_JSON="[]"
STARTED=""

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
  if [ "$STATE_MODE" = "extension-host" ]; then
    coverage="real pinned VS Code Extension Host: extension present+activated, faktor.* commands registered, compiled webview provider HTML carries strict CSP + composer/attachment surface"
    does_not_prove='["rendered pixels or screenshots","visible workbench UI/WebviewView display","live daemon connectivity (the provider HTML is exercised in-process)"]'
  else
    coverage="pinned VS Code CLI only: --version pin check and isolated --install-extension/--list-extensions of the built VSIX"
    does_not_prove='["real Extension Host launch","activation","webview HTML generation","rendered pixels"]'
  fi
  mkdir -p "$(dirname "$OUT_FILE")"
  printf '{"schema":"faktor-vscode-e2e/v1","status":"%s","reason":"%s","mode":"%s","coverage":"%s","does_not_prove":%s,"pin":{"version":"%s","commit":"%s","url":"%s","sha256":"%s","sha256_verified":%s},"code_version":"%s","host_evidence_sha256":"%s","cli_checks":%s,"host_checks":%s,"commit":"%s","tree":"%s","runner":{"os":"%s","arch":"%s"},"started_at":"%s","finished_at":"%s","duration_seconds":%s}\n' \
    "$(json_escape "$status")" "$(json_escape "$STATE_REASON")" "$(json_escape "${STATE_MODE:-cli-only}")" \
    "$(json_escape "$coverage")" "$does_not_prove" \
    "$VSCODE_VERSION" "$VSCODE_COMMIT" "$(json_escape "$DOWNLOAD_URL")" "$EXPECTED_SHA" "$([ "$STATE_SHA" = verified ] && echo true || echo false)" \
    "$(json_escape "$STATE_CODE_VERSION")" "$STATE_HOST_EVIDENCE_SHA" "$STATE_CHECKS_JSON" "$STATE_HOST_CHECKS_JSON" \
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

sha_of() { sha256sum "$1" | awk '{print $1}'; }

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
    return 0
  fi
  if [ -f "$cached" ]; then
    TARBALL="$cached"
    TARBALL_FROM_CACHE=1
    return 0
  fi
  echo "vscode-e2e: downloading pinned VS Code $VSCODE_VERSION from $DOWNLOAD_URL" >&2
  download_file "$DOWNLOAD_URL" "$cached" || fail "download-failed: $DOWNLOAD_URL"
  TARBALL="$cached"
  TARBALL_FROM_CACHE=1
}

verify_tarball() {
  has_sha256 "$EXPECTED_SHA" || fail "pin-invalid: expected SHA-256 is not 64 lowercase hex ('$EXPECTED_SHA')"
  case "$DOWNLOAD_URL" in
  *"$VSCODE_COMMIT"*) ;;
  *) fail "pin-invalid: download URL does not embed the pinned commit $VSCODE_COMMIT" ;;
  esac
  local actual
  actual="$(sha_of "$TARBALL")"
  if [ "$actual" != "$EXPECTED_SHA" ]; then
    if [ "${TARBALL_FROM_CACHE:-0}" = "1" ]; then
      rm -f "$TARBALL"
    fi
    fail "sha256-mismatch: pinned '$EXPECTED_SHA' != actual '$actual' for $TARBALL"
  fi
  STATE_SHA="verified"
  echo "vscode-e2e: sha256 verified ($actual)" >&2
}

export_vscode() {
  local extracted="${FAKTOR_VSCODE_EXTRACTED:-}"
  if [ -n "$extracted" ]; then
    CODE_BIN="$extracted/bin/code"
  else
    rm -rf "$WORK_DIR/vscode"
    mkdir -p "$WORK_DIR/vscode"
    tar -xzf "$TARBALL" -C "$WORK_DIR/vscode" || fail "extract-failed: $TARBALL"
    CODE_BIN="$WORK_DIR/vscode/VSCode-linux-x64/bin/code"
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
  "$CODE_BIN" "${extra[@]}" --user-data-dir "$WORK_DIR/cli-user-data" --extensions-dir "$WORK_DIR/cli-extensions" "$@"
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
host_launch_args() {
  local args="--wait --headless --disable-gpu --ozone-platform=headless --disable-dev-shm-usage"
  if [ "$NO_SANDBOX" -eq 1 ] || [ "$(id -u)" -eq 0 ]; then
    args="$args --no-sandbox"
  fi
  printf '%s' "$args"
}

run_extension_host() {
  mkdir -p "$WORK_DIR/host-user-data" "$WORK_DIR/host-extensions"
  local evidence="$WORK_DIR/host-evidence.json"
  rm -f "$evidence"
  [ -f "$EXTENSION_HOST_SMOKE" ] || fail "host-test-missing: $EXTENSION_HOST_SMOKE"
  [ -f "$EXTENSION_DIR/out/extension.js" ] || fail "extension-not-built: $EXTENSION_DIR/out/extension.js is missing (run npm run build first)"

  local -a args
  # shellcheck disable=SC2206 # args are a fixed internal word list
  args=($(host_launch_args))
  echo "vscode-e2e: launching pinned Extension Host ($VSCODE_VERSION) ${args[*]}" >&2

  local rc
  FAKTOR_VSCODE_E2E_EVIDENCE="$evidence" \
    FAKTOR_VSCODE_E2E_PIN_VERSION="$VSCODE_VERSION" \
    FAKTOR_VSCODE_E2E_PIN_COMMIT="$VSCODE_COMMIT" \
    timeout "$HOST_TIMEOUT" "$CODE_BIN" "${args[@]}" \
    --user-data-dir "$WORK_DIR/host-user-data" \
    --extensions-dir "$WORK_DIR/host-extensions" \
    --extensionDevelopmentPath="$EXTENSION_DIR" \
    --extensionTestsPath="$EXTENSION_HOST_SMOKE" >/dev/null 2>&1
  rc=$?

  if [ ! -s "$evidence" ]; then
    fail "extension-host-no-evidence: host rc=$rc and no evidence at $evidence (no silent pass)"
  fi
  STATE_HOST_EVIDENCE_SHA="$(sha_of "$evidence")"
  local ok_checks
  ok_checks="$(node -e '
    const fs = require("node:fs");
    const e = JSON.parse(fs.readFileSync(process.argv[1], "utf8"));
    const bad = (e.checks || []).filter((c) => !c.ok).map((c) => c.name);
    if (e.status !== "passed" || bad.length > 0) {
      console.error("status=" + e.status + " failed=" + bad.join(","));
      process.exit(1);
    }
    process.stdout.write("ok");
  ' "$evidence" 2>&1)" || fail "extension-host-checks-failed: $ok_checks"
  STATE_HOST_CHECKS_JSON="$(node -e 'process.stdout.write(JSON.stringify(JSON.parse(require("node:fs").readFileSync(process.argv[1], "utf8")).checks || []))' "$evidence")"
  STATE_MODE="extension-host"
  echo "vscode-e2e: extension host checks passed (evidence sha256 $STATE_HOST_EVIDENCE_SHA)" >&2
}

# ------------------------------------------------------------------ selftest
make_fake_code() { # rootdir
  mkdir -p "$1/bin"
  cat >"$1/bin/code" <<'FAKE'
#!/bin/sh
args="$*"
case "$args" in
*--extensionTestsPath*)
  if [ "${FAKE_CODE_NO_EVIDENCE:-0}" = 1 ]; then exit 0; fi
  if [ "${FAKE_CODE_FAIL_CHECK:-0}" = 1 ]; then
    printf '{"status":"failed","checks":[{"name":"webview-html-has:id=\\"composer\\"","ok":false,"detail":"planted"}]}\n' >"${FAKTOR_VSCODE_E2E_EVIDENCE:?}"
    exit 0
  fi
  printf '{"status":"passed","checks":[{"name":"extension-present","ok":true},{"name":"webview-html-has:id=\\"composer\\"","ok":true}]}\n' >"${FAKTOR_VSCODE_E2E_EVIDENCE:?}"
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
  make_fake_code "$tmp/extracted"
  mkdir -p "$tmp/extension/out"
  printf 'module.exports = {};\n' >"$tmp/extension/out/extension.js"
  export FAKTOR_VSCODE_E2E_EXTENSION="$tmp/extension"
  mkdir -p "$tmp/pack/VSCode-linux-x64/bin"
  cp "$tmp/extracted/bin/code" "$tmp/pack/VSCode-linux-x64/bin/code"
  tar -czf "$tmp/code.tar.gz" -C "$tmp/pack" VSCode-linux-x64
  local sha
  sha="$(sha_of "$tmp/code.tar.gz")"

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
    bash "$SELF" --out "$out" --cache-dir "$tmp/cache" --work-dir "$tmp/work-happy" \
    >"$tmp/happy.stdout" 2>"$tmp/happy.stderr"
  rc=$?
  check "happy pinned tarball passes" "$([ "$rc" -eq 0 ] && echo 0 || echo 1)"
  check "happy status passed" "$([ "$(status_of "$out")" = passed ] && echo 0 || echo 1)"
  check "happy mode extension-host" "$(node -e 'process.exit(JSON.parse(require("node:fs").readFileSync(process.argv[1],"utf8")).mode==="extension-host"?0:1)' "$out" 2>/dev/null && echo 0 || echo 1)"
  check "happy records does_not_prove" "$(grep -q 'does_not_prove' "$out" && echo 0 || echo 1)"

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

  # 7. the extension-host test module is syntactically valid
  node --check "$EXTENSION_HOST_SMOKE" >/dev/null 2>&1
  check "extension-host-smoke.cjs parses" "$?"

  if [ "$failures" -gt 0 ]; then
    echo "vscode-e2e selftest: FAIL ($failures case(s))" >&2
    return 1
  fi
  echo "vscode-e2e selftest: PASS (pin tamper, no-evidence, failed-check, version and cli-only cases exercised)"
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

if [ "$(uname -s)" != "Linux" ] || [ "$(uname -m)" != "x86_64" ]; then
  echo "vscode-e2e: linux-x64 only (got $(uname -s)/$(uname -m))" >&2
  exit 2
fi

STARTED="$(iso_now)"
mkdir -p "$WORK_DIR" "$CACHE_DIR"

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
if [ "$CLI_ONLY" -eq 1 ]; then
  STATE_MODE="cli-only"
else
  run_extension_host
fi
write_record passed

if [ "$KEEP" -eq 0 ]; then
  rm -rf "$WORK_DIR"
fi
exit 0

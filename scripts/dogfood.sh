#!/usr/bin/env bash
# DOGFOOD executor: the full local self-verification suite, run in sections.
#
# This is the executable half of the DOGFOOD program. Every command here is a
# registered gate that scripts/dogfood.mjs --coverage requires to be present
# (the checker parses THIS file as the run plan), so the inventory and the
# executor cannot drift: adding a surface without registering its gate in both
# places turns the release/PR certificate lanes red.
#
# Sections:
#   1) canonical cargo gates (CI linux-lane commands; umask 022)
#   2) checkers + their selftests (including the certification selftest suite)
#   3) planted-mutation campaign (DOGFOOD_SKIP_MUTATIONS=1 skips with a reason)
#   4) faktor-cli doctor --deep on a fresh data dir
#   5) VS Code UI: build, selftest, render check, VSIX package/verify, real
#      Extension Host e2e when the pinned VS Code tarball is cached/reachable
#   6) JetBrains UI: gradle smokes + host-matrix ZIP proof (raw compile-and-
#      smoke path additionally when kotlinc is present)
#   7) dogfood.mjs --coverage LAST so the manifest reflects everything
#
# Exit status: non-zero when any section failed. Skipped steps are always
# printed with a reason; there are no silent skips.
set -uo pipefail
umask 022

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT" || exit 2

# The VS Code selftest imports TypeScript modules directly, which needs a Node
# that supports type stripping (24.x here). Prefer the host's nvm install; the
# tests' pinned node has shipped this support since 23.6. Override with
# DOGFOOD_NODE if the layout differs.
NODE_BIN="${DOGFOOD_NODE:-}"
if [ -z "$NODE_BIN" ] && [ -x "$HOME/.nvm/versions/node/v24.21.0/bin/node" ]; then
    NODE_BIN="$HOME/.nvm/versions/node/v24.21.0/bin/node"
fi
if [ -z "$NODE_BIN" ]; then
    NODE_BIN="$(command -v node || true)"
fi
if [ -z "$NODE_BIN" ]; then
    printf 'dogfood: FAIL — no node binary found (set DOGFOOD_NODE)\n' >&2
    exit 2
fi
PATH="$(dirname "$NODE_BIN"):$PATH"
export PATH
printf 'dogfood: node = %s (%s)\n' "$NODE_BIN" "$("$NODE_BIN" --version 2>/dev/null || printf unknown)"
printf 'dogfood: root = %s\n' "$ROOT"

# ---------------------------------------------------------------------------
# Section bookkeeping (bash 3.2 compatible: indexed arrays only).
# ---------------------------------------------------------------------------
SECTION_IDS=()
SECTION_LABELS=()
SECTION_STATUSES=()
SECTION_DETAILS=()
FAILED=0
SKIPPED=()

begin_section() { # id label
    SECTION_ID="$1"
    SECTION_LABEL="$2"
    SECTION_FAILED=0
}

end_section() {
    if [ "$SECTION_FAILED" -ne 0 ]; then
        SECTION_STATUSES+=("FAIL")
        SECTION_DETAILS+=("")
        FAILED=$((FAILED + 1))
        printf '\n[%s] %s: FAIL\n' "$SECTION_ID" "$SECTION_LABEL"
    else
        SECTION_STATUSES+=("PASS")
        SECTION_DETAILS+=("")
        printf '\n[%s] %s: PASS\n' "$SECTION_ID" "$SECTION_LABEL"
    fi
    SECTION_IDS+=("$SECTION_ID")
    SECTION_LABELS+=("$SECTION_LABEL")
}

skip_section() { # id label reason
    SECTION_IDS+=("$1")
    SECTION_LABELS+=("$2")
    SECTION_STATUSES+=("SKIP")
    SECTION_DETAILS+=("$3")
    SKIPPED+=("$1: $3")
    printf '\n[%s] %s: SKIP (%s)\n' "$1" "$2" "$3"
}

record_failure() {
    SECTION_FAILED=1
}

# run_cmd <argv...>: print and run a simple command.
run_cmd() {
    printf '\n$ %s\n' "$*"
    "$@"
    local rc=$?
    if [ "$rc" -ne 0 ]; then
        printf '  -> exit %s\n' "$rc"
        record_failure
    fi
    return "$rc"
}

# run_shell <command string>: print and run through bash (pipes, redirects).
run_shell() {
    printf '\n$ %s\n' "$*"
    bash -c "$*"
    local rc=$?
    if [ "$rc" -ne 0 ]; then
        printf '  -> exit %s\n' "$rc"
        record_failure
    fi
    return "$rc"
}

DOCTOR_DIR="$(mktemp -d "${TMPDIR:-/tmp}/faktor-dogfood-doctor.XXXXXX")"
VSIX_DIR="$(mktemp -d "${TMPDIR:-/tmp}/faktor-dogfood-vsix.XXXXXX")"
cleanup() {
    rm -rf "$DOCTOR_DIR" "$VSIX_DIR"
}
trap cleanup EXIT

# ===========================================================================
# §1 canonical cargo gates (trusted linux lane commands).
# ===========================================================================
# CI parity: the trusted linux lane caps libtest parallelism and rustc fan-out
# on shared runners (its documented resource setting — timing-sensitive
# settlement/queue tests livelock at full nproc when other lanes build on the
# same host). Override the caps in the environment if the host is idle.
export RUST_TEST_THREADS="${RUST_TEST_THREADS:-2}"
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}"
begin_section cargo "canonical cargo gates (fmt/check/clippy/tests)"
run_cmd cargo fmt --check
run_cmd cargo check --workspace --all-features
run_cmd cargo clippy --workspace --all-targets --all-features -- -D warnings
# The browser crate's sandboxed tests need an empty network namespace
# (CAP_SYS_ADMIN). Probe through the SAME code path (one light browser
# test):
#   * probe passes                  -> run the whole workspace;
#   * probe fails with the TYPED
#     sandbox-unavailable marker    -> exclude ONLY faktor-browser, with a
#     printed skip note (the CI trusted lane runs it);
#   * probe fails any other way     -> fall through to the full run so a
#     real regression stays loud and red.
BROWSER_PROBE_OUT="$(cargo test -p faktor-browser --test browser crash_detection_is_typed_and_leaves_no_zombie -- --exact 2>&1)"
BROWSER_PROBE_RC=$?
if [ "$BROWSER_PROBE_RC" -eq 0 ] || ! printf '%s' "$BROWSER_PROBE_OUT" | grep -q "sandbox unavailable"; then
    run_cmd cargo test --workspace --all-features
else
    printf '\n[skip note] browser sandbox tests: the configured sandbox cannot create a network namespace on this host (typed "sandbox unavailable"); running the workspace WITHOUT faktor-browser (the CI trusted lane runs it)\n'
    SKIPPED+=("browser: sandbox network namespaces unavailable on this host (typed isolation refusal); faktor-browser excluded locally, covered by the CI trusted lane")
    run_cmd cargo test --workspace --all-features --exclude faktor-browser
fi
run_shell "cargo test --workspace --all-features -- --list | python3 scripts/certification/check-capability-tests-compiled.py"
end_section

# ===========================================================================
# §2 checkers and their selftests (node-only where possible; CI parity).
# ===========================================================================
begin_section checkers "checkers and selftests"
run_cmd "$NODE_BIN" scripts/check-invariants.mjs
run_cmd "$NODE_BIN" scripts/check-invariant-coverage.mjs selftest
run_cmd "$NODE_BIN" scripts/check-invariant-coverage.mjs --check
run_cmd "$NODE_BIN" scripts/check-invariant-coverage.mjs --release
run_cmd "$NODE_BIN" scripts/check-network-authority-singular.mjs
run_cmd "$NODE_BIN" scripts/check-authority-collapse.mjs --selftest-then-coverage
run_cmd "$NODE_BIN" scripts/check-ignored-tests.mjs
run_cmd "$NODE_BIN" scripts/check-ignored-tests.mjs --selftest
run_shell "bash scripts/check-docs-sync.sh"
run_cmd "$NODE_BIN" scripts/capabilities-manifest.mjs --selftest
run_cmd "$NODE_BIN" scripts/capabilities-manifest.mjs --check-claims
run_shell "bash scripts/check-licenses.sh"
run_shell "bash scripts/check-gradle-integrity.sh --selftest"
run_shell "bash scripts/branding-scan.sh"
run_cmd cargo test -p faktor-tests-static-authority
run_cmd "$NODE_BIN" scripts/discover-capabilities.mjs selftest
run_cmd "$NODE_BIN" scripts/protocol-codegen.mjs --check-clients
run_cmd "$NODE_BIN" scripts/check-visual-platforms.mjs --selftest
run_cmd python3 scripts/certification/check-visual-baseline.py selftest
run_cmd "$NODE_BIN" scripts/contracts/emit.mjs selftest
run_cmd "$NODE_BIN" scripts/certification/evidence.mjs selftest
run_cmd "$NODE_BIN" scripts/certification/check-workflow-graph.mjs selftest
run_cmd "$NODE_BIN" scripts/certification/publish-status.mjs selftest
run_cmd "$NODE_BIN" scripts/certification/accept-visual-baseline.mjs selftest
run_shell "bash scripts/sign-macos.sh selftest"
run_shell "bash scripts/sign-windows.sh selftest"
run_cmd "$NODE_BIN" scripts/update-manifest.mjs selftest
run_shell "bash scripts/certification/tests/selftests.sh"
end_section

# ===========================================================================
# §3 planted-mutation campaign (long; the coordinator runs it in full).
# ===========================================================================
if [ "${DOGFOOD_SKIP_MUTATIONS:-0}" = "1" ]; then
    skip_section mutations "planted-mutation campaign" \
        "DOGFOOD_SKIP_MUTATIONS=1 — operator requested the dry run; the full campaign runs by default and in the CI jetbrains-smoke lane"
else
    begin_section mutations "planted-mutation campaign"
    run_cmd "$NODE_BIN" scripts/check-invariants.mjs --mutations
    end_section
fi

# ===========================================================================
# §4 doctor --deep on a fresh data dir (mirrors cargo run -p faktor-cli -- doctor).
# ===========================================================================
begin_section doctor "faktor-cli doctor --deep (fresh data dir)"
run_cmd cargo run -p faktor-cli -- doctor --data-dir "$DOCTOR_DIR" --deep
end_section

# ===========================================================================
# §5 VS Code UI: build, selftest, render check, VSIX package/verify, e2e.
# ===========================================================================
begin_section vscode "VS Code UI (build/selftest/render/VSIX/e2e)"
if "$NODE_BIN" -e 'process.exit(Number(process.versions.node.split(".")[0]) >= 23 ? 0 : 1)'; then
    :
else
    printf '\ndogfood: NODE TOO OLD for apps/vscode/scripts/selftest.mjs (direct .ts imports need Node >= 23.6; set DOGFOOD_NODE to a Node 24+ binary)\n'
    record_failure
fi
if [ -d apps/vscode/node_modules ] && [ ! -w apps/vscode/node_modules/.bin ]; then
    # Container-created caches leave root-owned entries the invoking user
    # cannot rewrite; `npm ci` would fail with EACCES even though the existing
    # tree is complete. Record an explicit SKIP instead of a spurious FAIL;
    # the rest of the VS Code lane (build, selftest, render, VSIX, e2e) still
    # proves the surface.
    NPM_CI_REASON="apps/vscode/node_modules/.bin is not writable (owner $(stat -c '%U' apps/vscode/node_modules/.bin 2>/dev/null || printf unknown); container-created install) — npm ci cannot rewrite it without root; the existing installed tree is used"
    SKIPPED+=("npm-ci: $NPM_CI_REASON")
    printf '\nSKIP npm ci (%s)\n' "$NPM_CI_REASON"
else
    run_shell "cd apps/vscode && npm ci"
fi
run_shell "cd apps/vscode && npm run build"
run_shell "cd apps/vscode && $NODE_BIN scripts/selftest.mjs"
run_shell "cd apps/vscode && $NODE_BIN scripts/render-webview.mjs --check"
run_shell "cd apps/vscode && npx --no-install vsce package --out faktor-ci.vsix"
run_shell "cd apps/vscode && $NODE_BIN scripts/verify-vsix.mjs faktor-ci.vsix --min-media-files 4"
run_shell "rm -rf '$VSIX_DIR/extract' && mkdir -p '$VSIX_DIR/extract' && unzip -q apps/vscode/faktor-ci.vsix -d '$VSIX_DIR/extract'"
run_shell "cd apps/vscode && $NODE_BIN scripts/verify-vsix.mjs faktor-ci.vsix --extract-dir '$VSIX_DIR/extract/extension'"
run_shell "cd apps/vscode && $NODE_BIN scripts/selftest.mjs --packaged '$VSIX_DIR/extract/extension'"
# A VS Code CLI may live at a non-PATH snap location; export it so the
# packaged-extension load is REAL evidence instead of a recorded skip.
if [ -z "${VSCODE_CLI:-}" ] && [ -x /snap/bin/code ]; then
    export VSCODE_CLI=/snap/bin/code
fi
run_shell "cd apps/vscode && $NODE_BIN scripts/verify-vsix.mjs faktor-ci.vsix --ide-load"
# Real Extension Host e2e: run only when the pinned tarball is cached or the
# pinned URL is reachable; otherwise record an explicit SKIP (never silent).
VSCODE_VERSION="$(sed -n 's/^VSCODE_VERSION="\(.*\)"$/\1/p' scripts/vscode-e2e.sh | head -n 1)"
VSCODE_URL="$(sed -n 's/^VSCODE_URL="\(.*\)"$/\1/p' scripts/vscode-e2e.sh | head -n 1)"
E2E_CACHE="${FAKTOR_VSCODE_E2E_CACHE:-$ROOT/target/certification/vscode-e2e/cache}/code-stable-x64-${VSCODE_VERSION}.tar.gz"
E2E_REASON=""
if [ -n "${FAKTOR_VSCODE_TARBALL:-}" ] && [ -f "${FAKTOR_VSCODE_TARBALL}" ]; then
    :
elif [ -f "$E2E_CACHE" ]; then
    :
elif [ -n "$VSCODE_URL" ] && command -v curl >/dev/null 2>&1 && curl -fsSI --max-time 20 "$VSCODE_URL" >/dev/null 2>&1; then
    :
else
    E2E_REASON="offline: pinned VS Code ${VSCODE_VERSION} tarball not cached at ${E2E_CACHE} and ${VSCODE_URL:-the pinned URL} is unreachable (the lane verifies the pinned sha256 before use)"
fi
if [ -n "$E2E_REASON" ]; then
    SKIPPED+=("vscode-e2e: $E2E_REASON")
    printf '\nSKIP vscode-e2e (%s)\n' "$E2E_REASON"
else
    run_shell "bash scripts/vscode-e2e.sh"
fi
end_section

# ===========================================================================
# §6 JetBrains UI: gradle smokes + host matrix ZIP (kotlinc raw path optional).
# ===========================================================================
begin_section jetbrains "JetBrains UI (gradle smokes + host matrix zip)"
run_shell "cd apps/jetbrains && bash ../../scripts/check-gradle-integrity.sh"
if [ -x "$ROOT/target/debug/faktor-cli" ]; then
    :
else
    printf '\ndogfood: target/debug/faktor-cli missing (built by §4 doctor); building explicitly\n'
    run_cmd cargo build -p faktor-cli
fi
run_shell "cd apps/jetbrains && ./gradlew --console=plain --no-daemon -PfaktorCliBin='$ROOT/target/debug/faktor-cli' :backend:smoke :frontend:smoke :frontend:smokeHostMatrixZip"
if command -v kotlinc >/dev/null 2>&1; then
    run_shell "FAKTOR_JETBRAINS_REQUIRE_PLUGIN_ZIP=1 bash apps/jetbrains/compile-and-smoke.sh"
else
    printf '\nSKIP compile-and-smoke.sh (kotlinc absent; the gradle smoke tasks above are the CI-documented fallback)\n'
    SKIPPED+=("compile-and-smoke.sh: kotlinc absent; gradle smoke tasks used instead")
fi
end_section

# ===========================================================================
# §7 dogfood coverage LAST so the manifest reflects every section above.
# ===========================================================================
begin_section coverage "dogfood surface coverage"
run_cmd "$NODE_BIN" scripts/dogfood.mjs --coverage
end_section

# ===========================================================================
# Summary.
# ===========================================================================
printf '\n===================== DOGFOOD SUMMARY =====================\n'
k=0
while [ "$k" -lt "${#SECTION_IDS[@]}" ]; do
    printf '%-10s %-52s %s\n' "${SECTION_IDS[$k]}" "${SECTION_LABELS[$k]}" "${SECTION_STATUSES[$k]}"
    if [ "${SECTION_STATUSES[$k]}" = "SKIP" ]; then
        printf '           reason: %s\n' "${SECTION_DETAILS[$k]}"
    fi
    k=$((k + 1))
done
printf '===========================================================\n'
if [ "$FAILED" -ne 0 ]; then
    printf 'DOGFOOD: FAIL (%s section(s) failed; %s skip note(s))\n' "$FAILED" "${#SKIPPED[@]}"
    exit 1
fi
printf 'DOGFOOD: PASS (%s section(s); %s skip note(s))\n' "${#SECTION_IDS[@]}" "${#SKIPPED[@]}"
exit 0

#!/usr/bin/env bash
# Local certification harness (Phase 7 item 91/120 + capability manifest).
#
# One command runs the offline gate a change must pass on this machine and
# emits a machine-readable certificate for the EXACT commit it ran on.
# No network, no provider keys, no LLM calls: every section is local.
#
# CI migration note: the platform lanes moved from GitHub Actions to
# Woodpecker (event-scoped workflows in `.woodpecker/`), but this harness
# REMAINS the local/offline certificate: it is the per-host evidence source
# and the only producer of the `local_offline` and `release` manifest levels.
# CI and this script are complementary, not substitutes
# (docs/certification.md §1/§5).
#
# Profiles:
#   fast (default)  fmt --check; check --workspace; cross-target compile
#                   (windows-msvc, per-crate skips recorded); derived
#                   capability manifest + docs drift; evidence-verifier
#                   selftest (marker rejection matrix + repo drift); clippy
#                   -D warnings; workspace tests (caffeinate -i wrapped on
#                   darwin); static-authority scans; fault campaign smoke;
#                   doctor --deep on a fresh data dir; branding scan; release
#                   CLI build + doctor --deep on an empty data dir.
#   full            fast plus the long lanes: release [perf] distribution
#                   gates; [fault] campaigns at scale; coding-benchmark
#                   harness smoke; efficiency harness; ACP interop;
#                   installable artifact packaging (daemon tar.gz + VSIX +
#                   JetBrains zip) and the installation matrix. The
#                   packaging section is the only section that may touch
#                   the npm registry; an unreachable registry is recorded
#                   as an explicit skip, never silently claimed.
#                   Provider-key (real-model) runs are ALWAYS recorded as
#                   skipped: the local certificate is offline by contract.
#
# Env:
#   FAST_TESTS_SKIP=1  dry-run aid: records workspace tests as a SKIP with
#                      its reason instead of running them. Default (unset/0)
#                      runs the tests; a manifest with the tests skipped is
#                      never locally certified.
#   CERTIFY_OUT_DIR    output directory (default target/certification).
#   CERTIFY_EVIDENCE_KEYS  ed25519 identity allowlist for release-grade
#                      evidence verification; falls back to
#                      scripts/certification/evidence.mjs defaults (which are
#                      empty, so unsigned evidence never certifies).
#
# Release gates are EVIDENCE, not flags. CERTIFY_CROSS_PLATFORM_LANES and
# CERTIFY_REAL_PROVIDER are INERT booleans kept only as self-test inputs:
# setting one to 1 can never certify anything. A release gate is true only
# when target/certification/evidence/<kind>.json exists, verifies against the
# exact HEAD commit and HEAD tree hash (`git rev-parse 'HEAD^{tree}'`), has
# matching command/artifact digests, and carries an ed25519 signature from an
# allowlisted CI identity. A truthy boolean with no verifying evidence file
# FAILS the run loudly.
#
# Output:
#   target/certification/manifest.json       certificate for this exact commit
#   target/certification/capabilities.json   derived capability manifest
#                                            (surfaces probed from files/scripts)
#   target/certification/logs/<id>.log       full output per section
#
# The manifest schema (documented in docs/certification.md):
#   {schema, profile, status, certification_level, local_offline_certified,
#    release_certified, release_gates{cross_platform_lanes,real_provider,
#    evidence_required,boolean_inputs_ignored},
#    evidence{cross_platform_lanes{file,verified,signed}, ...},
#    commit, dirty_count, rustc, cargo, os, arch, timestamp,
#    duration_ms, fast_tests_skipped, sections[{name,label,status,duration_ms,
#    detail}], skipped[{name,reason}], capabilities{...}}.
#
# Capability truth: the `capabilities` block derives its UI-parity labels
# from target/certification/capabilities.json (generated from repository
# files/scripts by scripts/capabilities-manifest.mjs, never from prose).
# The same script's drift check fails when docs/certification.md disagrees
# with the derived manifest; when node is absent or the manifest is stale
# for another commit, labels fall back to "unknown" (never a fabricated
# status).
#
# Certification levels:
#   none             the run failed, or a fast-profile run cannot locally
#                    certify an offline release candidate.
#   local_offline    a clean full-profile run passed: the change is certified
#                    on THIS host, offline, with no provider keys or network.
#                    It is NOT a release certificate.
#   release          local_offline PLUS the external evidence gates
#                    (cross-platform lanes, real-provider run) verified from
#                    signed evidence objects at the same commit and tree.
#                    Without every gate, release_certified is false.
# Exit code is non-zero if any required section fails (fail-fast: the
# remaining sections are then recorded as skipped), or if a release-gate
# boolean flag is set without a verifying signed evidence file.
#
# A release is certified only for its exact commit with dirty=false AND every
# evidence gate verified; see docs/certification.md for what 100% means in
# this repository. The 12-24h real-time soak is out of scope by owner
# decision: it is not a release criterion, no CI workflow or release gate
# consumes it, and the [soak]-ignored longrun suites stay runnable manually.
#
# Self-test:
#   CERTIFY_SELFTEST=force_fail bash scripts/certify-local.sh fast
#     runs only synthetic sections (the first fails) to prove the failure
#     path exits non-zero, fail-fast records the remainder, and the manifest
#     carries the certification-level schema with all flags false.
#   CERTIFY_SELFTEST=release_gates bash scripts/certify-local.sh fast
#     proves the release rule end to end: (a) CERTIFY_* boolean flags are
#     inert — with both set to 1 and no evidence files the release
#     gates stay false; (b) an unsigned evidence file never certifies; (c) a
#     signed evidence file from an allowlisted ed25519 identity certifies
#     (keypair generated in a temp dir; nothing touches the real
#     certificate). Use CERTIFY_OUT_DIR to keep either run from overwriting
#     a real certificate.
set -u
set -o pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT" || exit 2

PROFILE="${1:-fast}"
case "$PROFILE" in
    fast | full) ;;
    *)
        printf 'usage: %s [fast|full]\n' "$0" >&2
        exit 2
        ;;
esac

SELFTEST="${CERTIFY_SELFTEST:-}"
OUT_DIR="${CERTIFY_OUT_DIR:-target/certification}"
LOG_DIR="$OUT_DIR/logs"
MANIFEST="$OUT_DIR/manifest.json"
EVIDENCE_DIR="$OUT_DIR/evidence"
EVIDENCE_SCRIPT="$ROOT/scripts/certification/evidence.mjs"
FAST_TESTS_SKIP="${FAST_TESTS_SKIP:-0}"
FAST_TESTS_SKIPPED=0
case "$FAST_TESTS_SKIP" in
    "" | 0 | false | FALSE | no | NO) FAST_TESTS_SKIP=0 ;;
    *) FAST_TESTS_SKIP=1 ;;
esac

mkdir -p "$LOG_DIR" || exit 2

# ---------------------------------------------------------------------------
# Result bookkeeping (bash 3.2 compatible: indexed arrays only).
# ---------------------------------------------------------------------------
SECTION_FN=()
SECTION_ID=()
SECTION_LABEL=()
SECTION_STATUS=()
SECTION_MS=()
SECTION_DETAIL=()
SKIPPED_NAMES=()
SKIPPED_REASONS=()
FAILED=0
FAILED_SECTION=""
TOTAL_MS=0
ATTEMPTED=0

add_section() {
    SECTION_FN+=("$1")
    SECTION_ID+=("$2")
    SECTION_LABEL+=("$3")
}

add_skip() {
    SKIPPED_NAMES+=("$1")
    SKIPPED_REASONS+=("$2")
}

now_ms() {
    if command -v perl >/dev/null 2>&1; then
        perl -MTime::HiRes=time -e 'printf "%.0f\n", time * 1000'
    else
        printf '%s000\n' "$(date +%s)"
    fi
}

json_escape() {
    printf '%s' "$1" | tr -d '\r' | tr '\n' ' ' |
        sed -e 's/\\/\\\\/g' -e 's/"/\\"/g' -e 's/\t/ /g'
}

first_error_line() {
    local log="$1" line
    line="$(grep -a -m1 -E '(^error(\[E[0-9]+\])?:|^error:|panicked at|FAILED|assertion .* failed|fatal:)' "$log" 2>/dev/null | head -n1)"
    if [ -z "$line" ]; then
        line="$(grep -a -m1 -iE 'error|fail' "$log" 2>/dev/null | head -n1)"
    fi
    if [ -z "$line" ]; then
        line="$(grep -a -m1 -v '^[[:space:]]*$' "$log" 2>/dev/null | head -n1)"
    fi
    printf '%s' "$line" | cut -c1-200
}

section_passed() {
    local want="$1" k
    for ((k = 0; k < ATTEMPTED; k++)); do
        if [ "${SECTION_ID[$k]}" = "$want" ] && [ "${SECTION_STATUS[$k]}" = "pass" ]; then
            printf 'true'
            return 0
        fi
    done
    printf 'false'
}

# One derived capability status from target/certification/capabilities.json.
# `unknown` whenever node is absent, the manifest is missing, or it was
# generated for a different commit — a stale label is never reused.
capability_status() {
    local key="$1" manifest="$OUT_DIR/capabilities.json" commit
    commit="$(git rev-parse HEAD 2>/dev/null || printf unknown)"
    if command -v node >/dev/null 2>&1 && [ -f "$manifest" ]; then
        node -e '
const fs = require("fs");
const [manifest, key, commit] = process.argv.slice(1);
const parsed = JSON.parse(fs.readFileSync(manifest, "utf8"));
if (parsed.commit !== commit) {
    process.stdout.write("unknown");
} else {
    const surface = parsed.surfaces && parsed.surfaces[key];
    process.stdout.write((surface && surface.status) || "unknown");
}' "$manifest" "$key" "$commit"
    else
        printf 'unknown'
    fi
}

# ---------------------------------------------------------------------------
# Certification rules (the ONLY place the manifest flags are decided; the
# selftest proves them as pure functions).
# ---------------------------------------------------------------------------

# A truthy evidence flag from the environment. Anything else is false.
flag() {
    case "${1:-}" in
        1 | true | TRUE | yes | YES) printf 'true' ;;
        *) printf 'false' ;;
    esac
}

# `evidence_gate <kind> [evidence-dir]`: true ONLY when
# target/certification/evidence/<kind>.json verifies via
# scripts/certification/evidence.mjs — schema + exact HEAD commit + HEAD tree
# hash + digests, and an ed25519 signature from an allowlisted identity
# (--require-signed). Environment booleans are deliberately NOT consulted;
# they are inert self-test inputs.
evidence_gate() {
    local kind="$1" dir="${2:-$EVIDENCE_DIR}"
    if [ ! -f "$EVIDENCE_SCRIPT" ]; then
        printf 'false'
        return 0
    fi
    if ! command -v node >/dev/null 2>&1; then
        printf 'false'
        return 0
    fi
    if [ ! -f "$dir/$kind.json" ]; then
        printf 'false'
        return 0
    fi
    mkdir -p "$LOG_DIR"
    if node "$EVIDENCE_SCRIPT" verify --kind "$kind" --evidence-dir "$dir" --require-signed \
        >"$LOG_DIR/evidence-$kind.log" 2>&1; then
        printf 'true'
    else
        printf 'false'
    fi
}

# The release-gate kinds, paired with their INERT boolean env names.
RELEASE_GATE_KINDS=(cross_platform_lanes real_provider)
RELEASE_GATE_ENVS=(CERTIFY_CROSS_PLATFORM_LANES CERTIFY_REAL_PROVIDER)

# Fails closed when an operator asserts a gate with the inert boolean but no
# verifying signed evidence exists: a boolean can never certify.
assert_boolean_flags_inert() {
    local idx env_name truthy gate kind
    for ((idx = 0; idx < ${#RELEASE_GATE_KINDS[@]}; idx++)); do
        kind="${RELEASE_GATE_KINDS[$idx]}"
        env_name="${RELEASE_GATE_ENVS[$idx]}"
        truthy="$(flag "${!env_name:-}")"
        gate="$(evidence_gate "$kind")"
        if [ "$truthy" = "true" ] && [ "$gate" != "true" ]; then
            printf 'release-gate: %s=%s is INERT and no verifying signed evidence/%s.json exists — a boolean never certifies; failing closed\n' \
                "$env_name" "${!env_name}" "$kind" >&2
            return 1
        fi
        if [ "$truthy" = "true" ] && [ "$gate" = "true" ]; then
            printf 'release-gate: %s=%s ignored; certification comes from the verified evidence/%s.json\n' \
                "$env_name" "${!env_name}" "$kind" >&2
        fi
    done
    return 0
}

# `local_offline_certified`: a CLEAN full-profile pass on this host with the
# workspace tests actually run. It certifies the offline local lane only —
# never a release.
local_offline_certified_rule() {
    # status profile dirty fast_tests_skipped
    if [ "$1" = "pass" ] && [ "$2" = "full" ] && [ "$3" = "0" ] && [ "$4" = "0" ]; then
        printf 'true'
    else
        printf 'false'
    fi
}

# `release_certified`: the local offline certificate PLUS the external
# evidence gates at the same SHA. The local harness can never fabricate a
# cross-platform lane or a real-provider run. The 12-24h real-time soak is
# out of scope by owner decision and is deliberately NOT a gate.
release_certified_rule() {
    # local_offline cross_platform real_provider
    if [ "$1" = "true" ] && [ "$2" = "true" ] && [ "$3" = "true" ]; then
        printf 'true'
    else
        printf 'false'
    fi
}

# ---------------------------------------------------------------------------
# Sections.
# ---------------------------------------------------------------------------
section_fmt() {
    cargo fmt --check
}

section_check() {
    cargo check --workspace
}

# Cross-target compile-only lane: crates that compile for windows-msvc
# without a C cross-toolchain are checked; a crate whose C dependency needs
# a cross-toolchain is a RECORDED SKIP inside target/certification/
# cross-target.json (the script exits 0 for skips, non-zero only on a real
# compile error).
section_cross_target() {
    bash scripts/cross-target-check.sh
}

# Regenerate the JetBrains parity artifact the capability derivation reads
# (target/ is disposable; a `cargo clean` must not silently downgrade the
# labels). kotlinc runs offline; when the toolchain is genuinely absent the
# step records a skip and the capabilities section then fails loudly rather
# than certifying a stale claim.
section_jetbrains_parity() {
    if ! command -v kotlinc >/dev/null 2>&1; then
        echo "SKIP: kotlinc unavailable; parity artifact not regenerated" >&2
        add_skip jetbrains-parity "kotlinc unavailable"
        return 0
    fi
    bash apps/jetbrains/compile-and-smoke.sh
}

# Derive target/certification/capabilities.json from repository files/scripts
# and fail when docs/certification.md's capability table drifts from it.
section_capabilities() {
    CAPABILITIES_OUT_DIR="$OUT_DIR" node scripts/capabilities-manifest.mjs
}

# Prove the certification-evidence verifier: the full marker rejection
# matrix, evidence commit/tree binding, the ed25519 signature allowlist and
# the .woodpecker marker/command drift check (scripts/certification/evidence.mjs).
section_evidence_selftest() {
    node scripts/certification/evidence.mjs selftest
}

section_clippy() {
    cargo clippy --workspace --all-targets -- -D warnings
}

section_tests() {
    if [ "$(uname -s)" = "Darwin" ] && command -v caffeinate >/dev/null 2>&1; then
        caffeinate -i cargo test --workspace
    else
        cargo test --workspace
    fi
}

doctor_deep_check() {
    local out rc
    out="$("$@" 2>&1)"
    rc=$?
    printf '%s\n' "$out"
    if [ "$rc" -ne 0 ]; then
        printf 'doctor exited non-zero (%s)\n' "$rc" >&2
        return 1
    fi
    if ! printf '%s\n' "$out" | grep -q 'doctor: all checks passed'; then
        printf 'doctor output lacks the all-checks-passed line\n' >&2
        return 1
    fi
    return 0
}

section_doctor_deep() {
    local dir rc
    dir="$(mktemp -d "${TMPDIR:-/tmp}/faktor-cert-doctor.XXXXXX")" || return 1
    doctor_deep_check cargo run -q -p faktor-cli -- doctor --deep --data-dir "$dir"
    rc=$?
    rm -rf "$dir"
    return "$rc"
}

section_fault_smoke() {
    cargo test -p faktor-tests-fault
}

section_static_authority() {
    cargo test -p faktor-tests-static-authority
}

section_branding() {
    bash scripts/branding-scan.sh || return 1
    local d found
    for d in apps/vscode apps/jetbrains; do
        [ -d "$d" ] || continue
        found="$(find "$d" -type f \( -name '*.vsix' -o -name '*.jar' \) -print -quit 2>/dev/null)"
        [ -n "$found" ] || continue
        bash scripts/branding-scan.sh --artifacts "$d" || return 1
    done
    return 0
}

section_release_cli() {
    cargo build --release -p faktor-cli || return 1
    local dir rc
    dir="$(mktemp -d "${TMPDIR:-/tmp}/faktor-cert-release.XXXXXX")" || return 1
    doctor_deep_check "$ROOT/target/release/faktor-cli" doctor --deep --data-dir "$dir"
    rc=$?
    rm -rf "$dir"
    return "$rc"
}

section_release_perf() {
    cargo test -p faktor-tests-performance --release -- --ignored
}

section_fault_scale() {
    cargo test -p faktor-tests-fault --release -- --ignored
}

section_coding_benchmark() {
    cargo test -p faktor-tests-coding-benchmark --test smoke
}

section_efficiency() {
    cargo test -p faktor-tests-efficiency
}

section_acp_interop() {
    cargo test -p faktor-acp --test interop
}

section_package_artifacts() {
    PACKAGE_OUT_DIR="$OUT_DIR" bash scripts/package-artifacts.sh
}

section_install_matrix() {
    MATRIX_OUT_DIR="$OUT_DIR" MATRIX_ARTIFACTS="$OUT_DIR/artifacts.json" \
        node scripts/install-matrix.mjs
}

section_selftest_fail() {
    printf 'CERTIFY_SELFTEST=force_fail: synthetic section failure\n'
    return 1
}

section_selftest_never() {
    printf 'CERTIFY_SELFTEST=force_fail: this section must never run\n'
    return 1
}

# ---------------------------------------------------------------------------
# Pure release-gate selftest: proves the certification rules before any real
# section runs. `release_gates` exits 0 only when every assertion holds.
# ---------------------------------------------------------------------------
if [ "$SELFTEST" = "release_gates" ]; then
    failures=0
    expect() {
        if [ "$2" != "$3" ]; then
            printf 'release-gate selftest: %s => %s (expected %s)\n' "$1" "$2" "$3" >&2
            failures=$((failures + 1))
        fi
    }
    expect "clean full pass is locally certified" \
        "$(local_offline_certified_rule pass full 0 0)" true
    expect "dirty full pass is not certified" \
        "$(local_offline_certified_rule pass full 1 0)" false
    expect "fast profile is not locally certified" \
        "$(local_offline_certified_rule pass fast 0 0)" false
    expect "skipped tests are not certified" \
        "$(local_offline_certified_rule pass full 0 1)" false
    expect "failed run is not certified" \
        "$(local_offline_certified_rule fail full 0 0)" false
    expect "all release gates certify" \
        "$(release_certified_rule true true true)" true
    expect "missing real provider blocks release" \
        "$(release_certified_rule true true false)" false
    expect "missing cross-platform lanes block release" \
        "$(release_certified_rule true false true)" false
    expect "no external evidence blocks release" \
        "$(release_certified_rule true false false)" false
    expect "no local certificate blocks release" \
        "$(release_certified_rule false true true)" false

    # ---- evidence is the only certifier: booleans are inert --------------
    # With every boolean asserted and no evidence files, every gate is false
    # and a locally-certified run still cannot release.
    export CERTIFY_CROSS_PLATFORM_LANES=1 CERTIFY_REAL_PROVIDER=1
    selftest_tmp="$(mktemp -d "${TMPDIR:-/tmp}/faktor-cert-gates.XXXXXX")" || exit 1
    trap 'rm -rf "$selftest_tmp"' EXIT
    mkdir -p "$selftest_tmp/empty" "$selftest_tmp/signed" "$selftest_tmp/unsigned"
    expect "boolean cross-platform flag is inert without evidence" \
        "$(evidence_gate cross_platform_lanes "$selftest_tmp/empty")" false
    expect "boolean real-provider flag is inert without evidence" \
        "$(evidence_gate real_provider "$selftest_tmp/empty")" false
    expect "asserted booleans cannot release-certify a local pass" \
        "$(release_certified_rule true false false)" false
    saved_evidence_dir="$EVIDENCE_DIR"
    EVIDENCE_DIR="$selftest_tmp/empty"
    if assert_boolean_flags_inert; then
        printf 'release-gate selftest: asserted booleans did not fail closed without evidence\n' >&2
        failures=$((failures + 1))
    fi
    EVIDENCE_DIR="$saved_evidence_dir"
    if command -v node >/dev/null 2>&1; then
        if node -e '
const { generateKeyPairSync } = require("crypto");
const fs = require("fs");
const { privateKey, publicKey } = generateKeyPairSync("ed25519");
fs.writeFileSync(process.argv[1], privateKey.export({ type: "pkcs8", format: "pem" }));
fs.writeFileSync(process.argv[2], JSON.stringify({
  identities: { selftest: { ed25519_public_key: publicKey.export({ format: "der", type: "spki" }).subarray(12).toString("base64") } },
}));
' "$selftest_tmp/key.pem" "$selftest_tmp/keys.json" &&
            CERTIFY_EVIDENCE_KEYS="$selftest_tmp/keys.json" node "$EVIDENCE_SCRIPT" write \
                --kind cross_platform_lanes --status passed --out-dir "$selftest_tmp/signed" \
                --sign-key "$selftest_tmp/key.pem" --key-id selftest --commands "release-gate selftest" \
                --runner-os darwin --runner-arch arm64 --runner-ci selftest --runner-run-id 1 >/dev/null &&
            CERTIFY_EVIDENCE_KEYS="$selftest_tmp/keys.json" node "$EVIDENCE_SCRIPT" write \
                --kind real_provider --status passed --out-dir "$selftest_tmp/unsigned" \
                --commands "release-gate selftest unsigned" \
                --runner-os darwin --runner-arch arm64 --runner-ci selftest --runner-run-id 1 >/dev/null; then
            expect "signed allowlisted evidence certifies a gate" \
                "$(CERTIFY_EVIDENCE_KEYS="$selftest_tmp/keys.json" evidence_gate cross_platform_lanes "$selftest_tmp/signed")" true
            expect "unsigned evidence never certifies a gate" \
                "$(CERTIFY_EVIDENCE_KEYS="$selftest_tmp/keys.json" evidence_gate real_provider "$selftest_tmp/unsigned")" false
            node -e '
const fs = require("fs");
const file = process.argv[1];
const record = JSON.parse(fs.readFileSync(file, "utf8"));
record.commit_sha = "0".repeat(40);
fs.writeFileSync(file, JSON.stringify(record, null, 2));
' "$selftest_tmp/signed/cross_platform_lanes.json"
            expect "evidence for another commit never certifies" \
                "$(CERTIFY_EVIDENCE_KEYS="$selftest_tmp/keys.json" evidence_gate cross_platform_lanes "$selftest_tmp/signed")" false
        else
            printf 'release-gate selftest: evidence.mjs self-check could not run\n' >&2
            failures=$((failures + 1))
        fi
    else
        printf 'release-gate selftest: node unavailable; evidence signature assertions skipped (booleans still proven inert)\n' >&2
    fi
    if [ "$failures" -eq 0 ]; then
        printf 'release-gate selftest: PASS (booleans inert; evidence must verify, bind to HEAD commit+tree, and be signed)\n'
        exit 0
    fi
    printf 'release-gate selftest: FAIL (%s assertion(s))\n' "$failures" >&2
    exit 1
fi

# ---------------------------------------------------------------------------
# Section plan for the selected profile.
# ---------------------------------------------------------------------------
if [ "$SELFTEST" = "force_fail" ]; then
    add_section section_selftest_fail selftest-fail "selftest: synthetic failure"
    add_section section_selftest_never selftest-never "selftest: must be skipped by fail-fast"
    add_skip all-gates "CERTIFY_SELFTEST=force_fail: synthetic failure replaces the real gate"
else
    add_section section_fmt fmt "cargo fmt --check"
    add_section section_check check "cargo check --workspace"
    add_section section_cross_target cross-target "cross-target compile (windows-msvc, skips recorded)"
    if command -v node >/dev/null 2>&1; then
        add_section section_jetbrains_parity jetbrains-parity "JetBrains parity artifact regeneration"
        add_section section_capabilities capabilities "capability manifest + docs drift"
        add_section section_evidence_selftest evidence-selftest "certification evidence verifier selftest"
    else
        add_skip capabilities-manifest "node is unavailable on this host; capability labels stay unknown"
        add_skip evidence-selftest "node is unavailable on this host; certified evidence cannot be verified"
    fi
    add_section section_clippy clippy "cargo clippy -D warnings"
    if [ "$FAST_TESTS_SKIP" -eq 1 ]; then
        FAST_TESTS_SKIPPED=1
        add_skip workspace-tests "FAST_TESTS_SKIP=1: dry run deferred the workspace test suite"
    else
        add_section section_tests workspace-tests "cargo test --workspace"
    fi
    if [ -f tests/static-authority/Cargo.toml ]; then
        add_section section_static_authority static-authority "static-authority scans"
    else
        add_skip static-authority "static-authority crate absent from this workspace"
    fi
    if [ -f tests/fault/Cargo.toml ]; then
        add_section section_fault_smoke fault-smoke "fault campaign smoke"
    else
        add_skip fault-smoke "fault suite crate absent from this workspace"
    fi
    add_section section_doctor_deep doctor-deep "doctor --deep (fresh data dir)"
    add_section section_branding branding "branding scan"
    add_section section_release_cli release-cli "release CLI + doctor (empty data dir)"

    if [ "$PROFILE" = "full" ]; then
        add_section section_release_perf release-perf "[perf] release gates"
        add_section section_fault_scale fault-scale "[fault] campaigns at scale"
        add_section section_coding_benchmark coding-benchmark "coding-benchmark smoke"
        add_section section_efficiency efficiency "efficiency harness"
        add_section section_acp_interop acp-interop "ACP interop"
        add_section section_package_artifacts package-artifacts "release artifact packaging (daemon + VSIX + JetBrains)"
        add_section section_install_matrix install-matrix "install matrix (clean-prefix extract + doctor + archive structure)"
    else
        add_skip release-perf "fast profile: run the full profile for [perf] release gates"
        add_skip fault-scale "fast profile: run the full profile for [fault] campaigns at scale"
        add_skip coding-benchmark "fast profile: run the full profile for the benchmark harness smoke"
        add_skip efficiency "fast profile: run the full profile for the efficiency harness"
        add_skip acp-interop "fast profile: run the full profile for ACP interop"
        add_skip package-artifacts "fast profile: run the full profile to package release artifacts"
        add_skip install-matrix "fast profile: run the full profile for the installation matrix"
    fi
    add_skip coding-benchmark-real-model "provider-key run excluded: local certification is offline by contract (no keys, no network)"
    add_skip windows-lane "no Windows host here; the CI windows lane owns it"
fi

TOTAL_SECTIONS=${#SECTION_FN[@]}

# ---------------------------------------------------------------------------
# Run sections in order, fail-fast.
# ---------------------------------------------------------------------------
printf 'local certification harness: profile=%s commit=%s\n' \
    "$PROFILE" "$(git rev-parse --short HEAD 2>/dev/null || printf unknown)"

i=0
for idx in "${!SECTION_FN[@]}"; do
    i=$((i + 1))
    fn="${SECTION_FN[$idx]}"
    sid="${SECTION_ID[$idx]}"
    label="${SECTION_LABEL[$idx]}"
    log="$LOG_DIR/$sid.log"
    printf '\n== [%s/%s] %s ==\n' "$i" "$TOTAL_SECTIONS" "$label"
    start="$(now_ms)"
    if "$fn" >"$log" 2>&1; then
        rc=0
    else
        rc=$?
    fi
    end="$(now_ms)"
    ms=$((end - start))
    TOTAL_MS=$((TOTAL_MS + ms))
    SECTION_MS+=("$ms")
    if [ "$rc" -eq 0 ]; then
        SECTION_STATUS+=("pass")
        SECTION_DETAIL+=("ok")
        printf '   ok (%sms) log: %s\n' "$ms" "$log"
    else
        detail="$(first_error_line "$log")"
        [ -n "$detail" ] || detail="section exited $rc"
        SECTION_STATUS+=("fail")
        SECTION_DETAIL+=("$detail (exit $rc)")
        FAILED=1
        FAILED_SECTION="$sid"
        printf '   FAIL (exit %s, %sms): %s\n' "$rc" "$ms" "$detail"
        printf '   log tail:\n'
        tail -n 25 "$log" 2>/dev/null | sed 's/^/   | /'
        printf '   full log: %s\n' "$log"
        break
    fi
done

if [ "$FAILED" -ne 0 ]; then
    j=0
    for idx in "${!SECTION_FN[@]}"; do
        j=$((j + 1))
        if [ "$j" -gt "$i" ]; then
            add_skip "${SECTION_ID[$idx]}" "fail-fast: not run after section '$FAILED_SECTION' failed"
        fi
    done
fi
ATTEMPTED="$i"

# ---------------------------------------------------------------------------
# Emit the certificate manifest (always, pass or fail).
# ---------------------------------------------------------------------------
emit_manifest() {
    local commit dirty rustc_v cargo_v os arch now status local_offline release_certified level
    local cross_platform real_provider platform_lane k
    local cross_signed real_signed
    commit="$(git rev-parse HEAD 2>/dev/null || printf unknown)"
    dirty="$(git status --porcelain 2>/dev/null | wc -l | tr -d ' ')"
    rustc_v="$(rustc --version 2>/dev/null || printf unknown)"
    cargo_v="$(cargo --version 2>/dev/null || printf unknown)"
    os="$(uname -s | tr '[:upper:]' '[:lower:]')"
    arch="$(uname -m)"
    now="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    # Release gates: EVIDENCE ONLY. The CERTIFY_* booleans are inert; a
    # truthy boolean without verifying signed evidence fails the run.
    cross_platform="$(evidence_gate cross_platform_lanes)"
    real_provider="$(evidence_gate real_provider)"
    if ! assert_boolean_flags_inert; then
        FAILED=1
        FAILED_SECTION="${FAILED_SECTION:-release-gates}"
    fi
    if [ "$FAILED" -eq 0 ]; then
        status="pass"
    else
        status="fail"
    fi
    # Certification levels: local_offline is this host's clean full pass;
    # release additionally requires EVERY verified external evidence gate
    # for this exact commit and tree. The harness never fabricates a gate
    # from a flag, and the 12-24h real-time soak is out of scope by owner
    # decision (never a gate).
    local_offline="$(local_offline_certified_rule "$status" "$PROFILE" "$dirty" "$FAST_TESTS_SKIPPED")"
    # Write this host's own evidence object when it certifies (unsigned unless
    # CERTIFY_EVIDENCE_SIGN_KEY is configured; never a release gate).
    if [ "$local_offline" = "true" ] && command -v node >/dev/null 2>&1 && [ -f "$EVIDENCE_SCRIPT" ]; then
        node "$EVIDENCE_SCRIPT" write --kind local_offline --status passed \
            --out-dir "$EVIDENCE_DIR" \
            --commands "local_offline full-profile certification of $commit" \
            --runner-os "$os" --runner-arch "$arch" --runner-ci local \
            --runner-run-id "$(date -u +%Y%m%dT%H%M%SZ)" \
            >>"$LOG_DIR/evidence-local_offline.log" 2>&1 || true
    fi
    release_certified="$(release_certified_rule "$local_offline" "$cross_platform" "$real_provider")"
    level="none"
    if [ "$release_certified" = "true" ]; then
        level="release"
    elif [ "$local_offline" = "true" ]; then
        level="local_offline"
    fi
    cross_signed="$(if [ "$cross_platform" = "true" ]; then printf true; else printf false; fi)"
    real_signed="$(if [ "$real_provider" = "true" ]; then printf true; else printf false; fi)"
    case "$os" in
        darwin) platform_lane="macos" ;;
        linux) platform_lane="linux" ;;
        *) platform_lane="unknown" ;;
    esac

    mkdir -p "$OUT_DIR"
    {
        printf '{\n'
        printf '  "schema": "faktor-certification-manifest/v1",\n'
        printf '  "profile": "%s",\n' "$PROFILE"
        printf '  "status": "%s",\n' "$status"
        printf '  "certification_level": "%s",\n' "$level"
        printf '  "local_offline_certified": %s,\n' "$local_offline"
        printf '  "release_certified": %s,\n' "$release_certified"
        printf '  "release_gates": {\n'
        printf '    "cross_platform_lanes": %s,\n' "$cross_platform"
        printf '    "real_provider": %s,\n' "$real_provider"
        printf '    "evidence_required": true,\n'
        printf '    "boolean_inputs_ignored": true\n'
        printf '  },\n'
        printf '  "evidence": {\n'
        printf '    "cross_platform_lanes": {"file": "%s", "verified": %s, "release_grade": %s},\n' \
            "$(json_escape "$EVIDENCE_DIR/cross_platform_lanes.json")" "$cross_platform" "$cross_signed"
        printf '    "real_provider": {"file": "%s", "verified": %s, "release_grade": %s}\n' \
            "$(json_escape "$EVIDENCE_DIR/real_provider.json")" "$real_provider" "$real_signed"
        printf '  },\n'
        printf '  "commit": "%s",\n' "$(json_escape "$commit")"
        printf '  "dirty_count": %s,\n' "$dirty"
        printf '  "rustc": "%s",\n' "$(json_escape "$rustc_v")"
        printf '  "cargo": "%s",\n' "$(json_escape "$cargo_v")"
        printf '  "os": "%s",\n' "$(json_escape "$os")"
        printf '  "arch": "%s",\n' "$(json_escape "$arch")"
        printf '  "timestamp": "%s",\n' "$now"
        printf '  "duration_ms": %s,\n' "$TOTAL_MS"
        printf '  "fast_tests_skipped": %s,\n' \
            "$(if [ "$FAST_TESTS_SKIPPED" -eq 1 ]; then printf true; else printf false; fi)"

        printf '  "sections": ['
        for ((k = 0; k < ATTEMPTED; k++)); do
            if [ "$k" -gt 0 ]; then
                printf ','
            fi
            printf '\n    {"name": "%s", "label": "%s", "status": "%s", "duration_ms": %s, "detail": "%s"}' \
                "$(json_escape "${SECTION_ID[$k]}")" \
                "$(json_escape "${SECTION_LABEL[$k]}")" \
                "${SECTION_STATUS[$k]}" \
                "${SECTION_MS[$k]}" \
                "$(json_escape "${SECTION_DETAIL[$k]}")"
        done
        if [ "$ATTEMPTED" -gt 0 ]; then printf '\n  '; fi
        printf '],\n'

        printf '  "skipped": ['
        for k in "${!SKIPPED_NAMES[@]}"; do
            if [ "$k" -gt 0 ]; then
                printf ','
            fi
            printf '\n    {"name": "%s", "reason": "%s"}' \
                "$(json_escape "${SKIPPED_NAMES[$k]}")" \
                "$(json_escape "${SKIPPED_REASONS[$k]}")"
        done
        if [ "${#SKIPPED_NAMES[@]}" -gt 0 ]; then printf '\n  '; fi
        printf '],\n'

        printf '  "capabilities": {\n'
        printf '    "schema": "faktor-capability-manifest/v1",\n'
        printf '    "platform": {"os": "%s", "arch": "%s"},\n' "$(json_escape "$os")" "$(json_escape "$arch")"
        printf '    "platform_lanes": {\n'
        printf '      "pr-lane": {"status": "not-run-here", "owner": "CI", "reason": "extension builds (npm + kotlinc) are CI-only"},\n'
        printf '      "linux": {"status": "%s", "owner": "CI/local", "reason": "hosted CI lane; local unless this host is linux"},\n' \
            "$(if [ "$platform_lane" = "linux" ]; then printf 'run-here'; else printf 'not-run-here'; fi)"
        printf '      "macos": {"status": "%s", "owner": "CI/local", "reason": "hosted CI lane; local unless this host is darwin"},\n' \
            "$(if [ "$platform_lane" = "macos" ]; then printf 'run-here'; else printf 'not-run-here'; fi)"
        printf '      "windows": {"status": "not-run-here", "owner": "CI", "reason": "no Windows host locally; CI windows lane covers the process-tree crates"}\n'
        printf '    },\n'
        printf '    "ui_parity": {\n'
        printf '      "vscode": "%s",\n' "$(capability_status vscode_webview)"
        printf '      "jetbrains": "%s",\n' "$(capability_status jetbrains_frontend)"
        printf '      "overall": "%s",\n' "$(capability_status ui_parity)"
        printf '      "manifest": "capabilities.json"\n'
        printf '    },\n'
        printf '    "compat_fixtures": {"jetbrains712": %s},\n' \
            "$(if [ -d compat/jetbrains-712 ]; then printf true; else printf false; fi)"
        printf '    "surfaces": {\n'
        printf '      "workspace_tests": %s,\n' "$(section_passed workspace-tests)"
        printf '      "fault_smoke": %s,\n' "$(section_passed fault-smoke)"
        printf '      "static_authority": %s,\n' "$(section_passed static-authority)"
        printf '      "doctor_deep": %s,\n' "$(section_passed doctor-deep)"
        printf '      "release_cli_doctor": %s,\n' "$(section_passed release-cli)"
        printf '      "release_perf": %s,\n' "$(section_passed release-perf)"
        printf '      "fault_scale": %s,\n' "$(section_passed fault-scale)"
        printf '      "coding_benchmark_smoke": %s,\n' "$(section_passed coding-benchmark)"
        printf '      "efficiency_harness": %s,\n' "$(section_passed efficiency)"
        printf '      "acp_interop": %s,\n' "$(section_passed acp-interop)"
        printf '      "packaging_artifacts": %s,\n' "$(section_passed package-artifacts)"
        printf '      "install_matrix": %s,\n' "$(section_passed install-matrix)"
        printf '      "coding_benchmark_real_model": "skipped: provider-key run, offline local certification never spends"\n'
        printf '    },\n'
        printf '    "offline": {"network_required": false, "provider_keys_required": false},\n'
        printf '    "release_rule": "a release is certified only for its exact commit with dirty=false AND local_offline_certified AND cross-platform lanes + real-provider evidence"\n'
        printf '  }\n'
        printf '}\n'
    } >"$MANIFEST.tmp"
    mv "$MANIFEST.tmp" "$MANIFEST"
}

emit_manifest

# The force_fail selftest also proves the manifest carries the
# certification-level schema with every flag honestly false.
if [ "$SELFTEST" = "force_fail" ]; then
    schema_ok=1
    for needle in \
        '"certification_level": "none"' \
        '"local_offline_certified": false' \
        '"release_certified": false' \
        '"cross_platform_lanes": false' \
        '"real_provider": false'; do
        if ! grep -q -- "$needle" "$MANIFEST"; then
            printf 'selftest: manifest schema missing %s\n' "$needle" >&2
            schema_ok=0
        fi
    done
    if [ "$schema_ok" -eq 1 ]; then
        printf 'selftest: failure manifest schema verified (all certification flags false)\n'
    else
        FAILED=1
    fi
fi

# ---------------------------------------------------------------------------
# Summary.
# ---------------------------------------------------------------------------
printf '\n=====================\n'
if [ "$FAILED" -eq 0 ]; then
    printf 'CERTIFICATION: PASS (%s profile)\n' "$PROFILE"
else
    printf 'CERTIFICATION: FAIL (%s profile) at section %s\n' "$PROFILE" "$FAILED_SECTION"
fi
for ((k = 0; k < ATTEMPTED; k++)); do
    printf '  [%s] %-16s %7sms  %s\n' \
        "${SECTION_STATUS[$k]}" "${SECTION_ID[$k]}" "${SECTION_MS[$k]}" "${SECTION_DETAIL[$k]}"
done
printf 'manifest: %s\n' "$MANIFEST"
printf 'commit:   %s (dirty=%s)\n' \
    "$(git rev-parse HEAD 2>/dev/null || printf unknown)" \
    "$(git status --porcelain 2>/dev/null | wc -l | tr -d ' ')"
printf 'evidence: cross-platform=%s real-provider=%s (dir=%s)\n' \
    "$(evidence_gate cross_platform_lanes)" "$(evidence_gate real_provider)" "$EVIDENCE_DIR"
printf 'gates:    CERTIFY_* booleans are inert self-test inputs; only verified signed evidence certifies\n'
printf 'skipped:  %s section(s) recorded in the manifest\n' "${#SKIPPED_NAMES[@]}"
printf 'offline:  no network, no provider keys, no LLM calls\n'
printf '=====================\n'

exit "$FAILED"

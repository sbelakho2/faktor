#!/usr/bin/env bash
# Release certification (P0-97 / P0-74 / P0-100; P1-I/P1-J/P1-K hardened;
# P1 release-certificate completeness).
#
# The gate a release candidate must pass BEFORE shipping:
#   1. the canonical cargo gate list exactly as CI runs it (fmt/check/clippy/
#      tests, all with --all-features where CI uses it) — the list is defined
#      ONCE in `canonical_gate_commands()` and `--check-gate-parity` proves the
#      `.woodpecker` linux lanes still match it;
#   2. `doctor --deep` on a FRESH data dir — every audit invariant (store,
#      CAS, journal, cost reservations, verification records, active-turn
#      recoverable owners, orphan children, process ownership) must pass and
#      print zero FAIL sections;
#   3. the fault campaign — the #[ignore]-gated `[fault]` tests of the
#      `faktor-tests-fault` crate. Release class REQUIRES the crate and at
#      least one executed `[fault]` test; the certificate records the
#      executed count and the sha256 digest of the executed test identities.
#      Without `--release` a missing crate/zero tests is recorded as an
#      informational skip and can only ever yield SOURCE CERTIFICATE;
#   4. `doctor --deep` again on a SECOND fresh data dir AFTER the campaign:
#      the corruption the campaign proves contained must not leak into a
#      fresh release image (P0-97 release certification);
#   5. the declared ReleaseArtifactSet: the required kinds (daemon binary,
#      VS Code VSIX, JetBrains plugin zip) must ALL be present locally,
#      brand-scanned where applicable, and covered digest-for-digest by the
#      verified signed attestation. `collect_artifacts()` returning an empty
#      list can never satisfy this;
#   6. a printed certificate summary.
#
# Certificate classes (P1 completeness):
#   * `RELEASE CERTIFICATE: PASS` — requires every release precondition
#     (canonical local gates, CI evidence release-class, complete + attested
#     ReleaseArtifactSet with branding pass, fault campaign pass with
#     executed>0). `--release` makes every precondition mandatory: unmet
#     preconditions are a FAIL, never a downgrade.
#   * `SOURCE CERTIFICATE: PASS — NOT A RELEASE CERTIFICATE` — a passing run
#     without the release preconditions (e.g. source-only checkout, missing
#     packaged outputs, fault crate absent). It can never print the release
#     class.
#   * `CI EVIDENCE` / `LOCAL PRE-FLIGHT` — never release certificates.
#
# Gate 8 (additive, P0-70; P1 completeness): the byte-level artifact
# branding scan (scripts/branding-scan.sh --artifacts) over the directories
# of REQUIRED packaged outputs (.vsix / plugin zip). A bounded scan of
# packaged outputs only — the daemon binary is a documented exemption
# (target/release binary strings are slow to sweep and cargo test/debug
# outputs embed frozen fixture text). When the ReleaseArtifactSet is not
# complete the scan is recorded as not-applicable and release class is
# impossible — a skip can never satisfy the release precondition.
#
# Gate 9 (additive, Phase E item 14; P1-I/P1-J/P1-K hardened): REAL CI
# certification for the EXACT commit being shipped. Local green gates are
# necessary but never sufficient: commit messages, PR descriptions and
# local test runs are NOT evidence.
#
#   * CONTEXT REGISTRY (P1-I): `--context`/CERTIFY_CI_CONTEXT must name one
#     entry of the immutable registry below; anything else is rejected with
#     exit 2. Each entry fixes {event, workflow, class, config_file}:
#       ci/woodpecker/pr/pr          pull_request  pr       untrusted  .woodpecker/untrusted/
#       ci/woodpecker/push/trusted   push          trusted  trusted    .woodpecker/trusted/
#       ci/woodpecker/tag/trusted    tag           trusted  trusted    .woodpecker/trusted/
#     The verifier then checks the OBSERVED facts from the Woodpecker API:
#     repository identity/id, exact 40-hex source SHA, actual pipeline event,
#     actual workflow name+state, pipeline state, pipeline id/number, the
#     project config file and the observed trusted class (trusted.volumes).
#     The certificate records the OBSERVED values, never caller literals.
#
#   * TRUSTED-BUILD ATTESTATION (P1-J): the trusted workflow's `attestation`
#     step publishes a signed `faktor-build-attestation/v1` object
#     ({source_sha, tree_sha, workflow, event, pipeline_id, pipeline_number,
#     build_environment_digest (CI image digest), rust_toolchain,
#     artifacts{name: sha256}}) into the step log (base64 marker block).
#     Signing is FAIL-CLOSED: without the signing-key secret the step exits
#     with the typed `signing-key-missing` error and writes no attestation,
#     so an unsigned object can only arrive from foreign/legacy tooling.
#     This script FETCHES the attestation belonging to the exact trusted
#     pipeline, verifies the ed25519 signature with the ALLOWLISTED key (the
#     embedded public key must equal it), verifies repository full name,
#     source SHA, tree, workflow, event, pipeline number and the OBSERVED
#     pipeline id (no number/id substitution exemption), and re-hashes EVERY
#     local/shipped artifact against the attested digests. Any mismatch is a
#     failure. With an untrusted selected context, the trusted project id
#     (WOODPECKER_TRUSTED_REPO_ID or a `?project=trusted` lookup answer) is
#     only a claim: the project record is re-fetched and its observed
#     full_name/config_file/trusted.volumes must match before its pipelines
#     are scanned.
#     Distributing the CI-built artifacts (the attested bytes) rather than
#     locally rebuilt ones is the preferred release model. The documented
#     alternative to the ed25519 allowlist is Sigstore/keyless verification
#     of the same payload against the pipeline's OIDC identity
#     (docs/certification.md §2.12).
#
#   * CERTIFICATE CLASS (P1-K): `--verify-ci-evidence` (the renamed
#     `--ci-only`) verifies the embedded CI run and terminates with
#     "CI EVIDENCE: PASS — NOT A RELEASE CERTIFICATE". A release certificate
#     is emitted ONLY when the full local gates pass AND (a trusted context
#     verifies OR the signed remote attestation covers the gates). No flag
#     weakens checks while preserving the certificate class.
#
# Usage:
#   bash scripts/certify.sh                       # local gates + required CI evidence (auto class)
#   bash scripts/certify.sh --commit <sha>        # certify an exact shipped SHA
#   bash scripts/certify.sh --context <registry>  # select a registered context
#   bash scripts/certify.sh --release             # release class is MANDATORY (unmet precondition = FAIL)
#   bash scripts/certify.sh --local-only          # local gates only (NOT a release certificate)
#   bash scripts/certify.sh --verify-ci-evidence  # CI evidence only (NOT a release certificate)
#   bash scripts/certify.sh --check-gate-parity   # canonical gate list vs CI lanes (drift check)
#   bash scripts/certify.sh --selftest            # hermetic mock-API rejection matrix
#
# Env: WOODPECKER_HOST, WOODPECKER_TOKEN (required unless --local-only/
# --selftest); WOODPECKER_REPO / --repo owner/name;
# WOODPECKER_UNTRUSTED_REPO_ID / WOODPECKER_TRUSTED_REPO_ID (or
# WOODPECKER_REPO_ID); CERTIFY_CI_CONTEXT (default ci/woodpecker/pr/pr);
# FAKTOR_ATTEST_KEYS / --attestation-keys (ed25519 allowlist JSON; required
# to verify signed attestations); --artifact PATH (repeatable; packaged
# outputs auto-discovered when omitted); CERTIFY_ARTIFACT_ROOT (discovery
# root for the ReleaseArtifactSet); CERTIFY_RELEASE=1 is the env form of
# --release.
#
# Release preconditions (all mandatory for RELEASE CERTIFICATE: PASS):
#   * required_artifacts > 0 and matched == required and attested == required
#     (every required local artifact digest-covered by the signed attestation);
#   * every required packaged artifact passed the branding/scanner gate;
#   * the fault campaign ran and executed > 0 `[fault]` tests without failure.
# Any missing precondition yields SOURCE CERTIFICATE (or FAIL under --release).
#
# Exits non-zero on the first failing gate (exit 2 = operator/setup error:
# missing credentials, unknown ref, rejected token). Safe to run from any
# directory (resolves the workspace root); the only writes are the temp data
# dirs and the release manifest.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

step() { printf '\n== %s ==\n' "$*"; }
ok() { printf '   ok: %s\n' "$*"; }
bad() { printf '   FAIL: %s\n' "$*"; }

# --------------------------------------------------------------- temp dirs --
# P3: this script mints `faktor-*` temp dirs; stale `kp-*` dirs left by the
# pre-migration versions are still swept (best effort, older than a day).
sweep_legacy_tmpdirs() {
    local pattern dir
    for pattern in kp-cert. kp-cert-ci. kp-cert-selftest.; do
        for dir in "${TMPDIR:-/tmp}"/"$pattern"*; do
            [ -d "$dir" ] || continue
            [ -L "$dir" ] && continue
            if [ -n "$(find "$dir" -maxdepth 0 -mtime +0 2>/dev/null)" ]; then
                rm -rf "$dir"
                if [ "${FAKTOR_TMP_SWEEP_VERBOSE:-0}" = "1" ]; then
                    printf 'certify: swept legacy temp dir %s\n' "$dir" >&2
                fi
            fi
        done
    done
    return 0
}
sweep_legacy_tmpdirs

# ------------------------------------------------------ context registry --
# P1-I: IMMUTABLE. These are the only contexts this verifier accepts; the
# table is a pure function of the script (no env/file override), and
# `--context` merely SELECTS an entry. Values are never trusted from the
# caller: the verifier re-observes each one from the Woodpecker API.
context_registry_entry() { # context -> "event workflow class config_file" (rc 1 = unknown)
    case "$1" in
    ci/woodpecker/pr/pr) printf 'pull_request pr untrusted .woodpecker/untrusted/' ;;
    ci/woodpecker/push/trusted) printf 'push trusted trusted .woodpecker/trusted/' ;;
    ci/woodpecker/tag/trusted) printf 'tag trusted trusted .woodpecker/trusted/' ;;
    *) return 1 ;;
    esac
}
context_registry_list() {
    printf '%s\n' ci/woodpecker/pr/pr ci/woodpecker/push/trusted ci/woodpecker/tag/trusted
}

# ------------------------------------------------------- canonical gates --
# P1 gate parity: the canonical cargo gate list is defined EXACTLY ONCE here.
# `run_local_gates` consumes it, and `--check-gate-parity` (also exercised by
# --selftest against fixtures and the real .woodpecker files) proves the CI
# linux lanes still run the same commands. A local/CI drift is a failure: a
# green local pre-flight must mean the same thing as a green CI lane.
canonical_gate_commands() {
    printf '%s\n' \
        'cargo fmt --check' \
        'cargo check --workspace --all-features' \
        'cargo clippy --workspace --all-targets --all-features -- -D warnings' \
        'cargo test --workspace --all-features'
}

run_canonical_gates() { # runs the canonical list; 0 all pass, 1 any fail
    local cmd rc=0
    while IFS= read -r cmd; do
        [ -n "$cmd" ] || continue
        local -a argv=()
        read -r -a argv <<<"$cmd"
        step "local gate: $cmd"
        if "${argv[@]}"; then
            ok "$cmd"
        else
            bad "$cmd"
            rc=1
        fi
    done < <(canonical_gate_commands)
    return "$rc"
}

extract_ci_gate_commands() { # workflow_yaml lane -> unique cargo gate commands
    awk -v lane="$2" '
        /^  - name: / { in_lane = ($3 == lane); next }
        in_lane && /^      - cargo (fmt|check|clippy|test)( |$)/ { sub(/^      - /, ""); print }
    ' "$1" 2>/dev/null | sed 's/[[:space:]]*$//' | sort -u
}

check_gate_parity() { # canonical_file ci_yaml...
    local canonical="$1"
    shift
    local local_list ci_list ci_file rc=0
    local_list="$(grep -v '^[[:space:]]*$' "$canonical" | sed 's/[[:space:]]*$//' | sort -u)"
    if [ -z "$local_list" ]; then
        bad "gate parity: canonical gate list '$canonical' is empty"
        return 1
    fi
    for ci_file in "$@"; do
        if [ ! -f "$ci_file" ]; then
            bad "gate parity: CI workflow '$ci_file' is missing"
            rc=1
            continue
        fi
        ci_list="$(extract_ci_gate_commands "$ci_file" linux)"
        if [ -z "$ci_list" ]; then
            bad "gate parity: $ci_file has no cargo gate commands in the linux lane"
            rc=1
            continue
        fi
        if [ "$ci_list" = "$local_list" ]; then
            ok "gate parity: $ci_file linux lane matches the canonical gate list"
        else
            bad "gate parity: $ci_file linux lane DIVERGES from the canonical gate list"
            diff <(printf '%s\n' "$local_list") <(printf '%s\n' "$ci_list") || true
            rc=1
        fi
    done
    return "$rc"
}

# ---------------------------------------------------------------- arguments --
COMMIT=""
CONTEXT="${CERTIFY_CI_CONTEXT:-ci/woodpecker/pr/pr}"
MANIFEST_OUT="target/certification/release-manifest.json"
LOCAL_ONLY=0
VERIFY_CI=0
SELFTEST=0
GATE_PARITY=0
GATE_PARITY_CANONICAL=""
GATE_PARITY_CI=()
RELEASE_REQUIRED=0
if [ "${CERTIFY_RELEASE:-0}" = "1" ]; then
    RELEASE_REQUIRED=1
fi
PIPELINE=""
REPO_FULL_NAME="${CERTIFY_REPO:-${WOODPECKER_REPO:-}}"
ARTIFACTS=()
ATT_KEYS="${CERTIFY_ATTEST_KEYS:-${FAKTOR_ATTEST_KEYS:-}}"

usage() {
    sed -n '2,/^set -/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'
}

while [ "$#" -gt 0 ]; do
    case "$1" in
    --local-only)
        LOCAL_ONLY=1
        shift
        ;;
    --verify-ci-evidence)
        VERIFY_CI=1
        shift
        ;;
    --ci-only)
        echo "certify: --ci-only was renamed to --verify-ci-evidence; the old flag no longer exists" >&2
        exit 2
        ;;
    --selftest)
        SELFTEST=1
        shift
        ;;
    --release)
        RELEASE_REQUIRED=1
        shift
        ;;
    --check-gate-parity)
        GATE_PARITY=1
        shift
        ;;
    --gate-parity-canonical)
        [ "$#" -ge 2 ] || { echo "certify: --gate-parity-canonical needs a file" >&2; exit 2; }
        GATE_PARITY_CANONICAL="$2"
        shift 2
        ;;
    --gate-parity-ci)
        [ "$#" -ge 2 ] || { echo "certify: --gate-parity-ci needs a workflow file" >&2; exit 2; }
        GATE_PARITY_CI+=("$2")
        shift 2
        ;;
    --commit)
        [ "$#" -ge 2 ] || { echo "certify: --commit needs a value" >&2; exit 2; }
        COMMIT="$2"
        shift 2
        ;;
    --context)
        [ "$#" -ge 2 ] || { echo "certify: --context needs a value" >&2; exit 2; }
        CONTEXT="$2"
        shift 2
        ;;
    --manifest-out)
        [ "$#" -ge 2 ] || { echo "certify: --manifest-out needs a value" >&2; exit 2; }
        MANIFEST_OUT="$2"
        shift 2
        ;;
    --pipeline)
        [ "$#" -ge 2 ] || { echo "certify: --pipeline needs a value" >&2; exit 2; }
        PIPELINE="$2"
        shift 2
        ;;
    --repo)
        [ "$#" -ge 2 ] || { echo "certify: --repo needs owner/name" >&2; exit 2; }
        REPO_FULL_NAME="$2"
        shift 2
        ;;
    --attestation-keys)
        [ "$#" -ge 2 ] || { echo "certify: --attestation-keys needs a file" >&2; exit 2; }
        ATT_KEYS="$2"
        shift 2
        ;;
    --artifact)
        [ "$#" -ge 2 ] || { echo "certify: --artifact needs a path" >&2; exit 2; }
        ARTIFACTS+=("$2")
        shift 2
        ;;
    -h | --help)
        usage
        exit 0
        ;;
    *)
        echo "certify: unknown argument: $1 (see --help)" >&2
        exit 2
        ;;
    esac
done

# The context is validated against the registry before anything else runs.
# Gate-parity and selftest modes are pure offline checks and skip it.
if [ "$SELFTEST" -eq 0 ] && [ "$GATE_PARITY" -eq 0 ]; then
    if ! context_registry_entry "$CONTEXT" >/dev/null; then
        printf 'certify: context %s is not registered; the immutable registry has:\n' "$CONTEXT" >&2
        context_registry_list | sed 's/^/  /' >&2
        exit 2
    fi
fi
if [ -n "$ATT_KEYS" ] && [ ! -f "$ATT_KEYS" ]; then
    echo "certify: attestation key allowlist '$ATT_KEYS' does not exist" >&2
    exit 2
fi
if [ "$RELEASE_REQUIRED" -eq 1 ] && { [ "$LOCAL_ONLY" -eq 1 ] || [ "$VERIFY_CI" -eq 1 ]; }; then
    echo "certify: --release requires the full local + CI path; it cannot be combined with --local-only/--verify-ci-evidence" >&2
    exit 2
fi

# P1 gate parity: standalone drift check, no cargo/network required.
if [ "$GATE_PARITY" -eq 1 ]; then
    step "gate parity: canonical local gate list vs CI linux lanes"
    parity_canonical="$GATE_PARITY_CANONICAL"
    parity_tmp=""
    if [ -z "$parity_canonical" ]; then
        parity_tmp="$(mktemp "${TMPDIR:-/tmp}/faktor-cert-gates.XXXXXX")"
        canonical_gate_commands >"$parity_tmp"
        parity_canonical="$parity_tmp"
    fi
    parity_files=()
    for f in ${GATE_PARITY_CI[@]+"${GATE_PARITY_CI[@]}"}; do
        parity_files+=("$f")
    done
    if [ "${#parity_files[@]}" -eq 0 ]; then
        parity_files=(.woodpecker/trusted/trusted.yaml .woodpecker/untrusted/pr.yaml)
    fi
    if check_gate_parity "$parity_canonical" "${parity_files[@]}"; then
        [ -n "$parity_tmp" ] && rm -f "$parity_tmp"
        printf '\nGATE PARITY: PASS\n'
        exit 0
    fi
    [ -n "$parity_tmp" ] && rm -f "$parity_tmp"
    printf '\nGATE PARITY: FAIL (local and CI gate lists diverged)\n' >&2
    exit 1
fi

operator_error() {
    printf 'certify: OPERATOR ACTION REQUIRED — %s\n' "$*" >&2
    printf '  Exact-SHA CI certification needs the Woodpecker API:\n' >&2
    printf '    WOODPECKER_HOST=https://<woodpecker-host> WOODPECKER_TOKEN=<repo-read-token> \\\n' >&2
    printf '      bash scripts/certify.sh --commit %s\n' "${COMMIT:-<40-hex-sha>}" >&2
    printf '  Commit messages, PR descriptions, local green tests and other\n' >&2
    printf '  commit-message claims are NOT evidence: only the required context\n' >&2
    printf '  %s succeeding at the exact SHA certifies. Use --local-only for a\n' "$CONTEXT" >&2
    printf '  local pre-flight that is explicitly NOT a release certificate.\n' >&2
    exit 2
}

if [ "$SELFTEST" -eq 0 ] && [ "$LOCAL_ONLY" -eq 0 ]; then
    [ -n "${WOODPECKER_HOST:-}" ] || operator_error "WOODPECKER_HOST is not set"
    [ -n "${WOODPECKER_TOKEN:-}" ] || operator_error "WOODPECKER_TOKEN is not set"
fi

gates=()

build_gate_summary() {
    gates=()
    local cmd
    while IFS= read -r cmd; do
        [ -n "$cmd" ] && gates+=("$cmd")
    done < <(canonical_gate_commands)
    gates+=("doctor --deep (fresh dir)" "fault campaign [fault] (count+digest)" "doctor --deep (post-campaign)" "ReleaseArtifactSet complete + attested" "artifact branding scan (required outputs)" "gate parity (canonical vs CI)")
}
build_gate_summary

run_doctor_deep() {
    local dir label rc out
    dir="$(mktemp -d "${TMPDIR:-/tmp}/faktor-cert.XXXXXX")"
    label="$1"
    out="$(cargo run -q -p faktor-cli -- doctor --deep --data-dir "$dir" 2>&1)"
    rc=$?
    printf '%s\n' "$out"
    if [ "$rc" -ne 0 ]; then
        bad "$label: doctor exited non-zero ($rc)"
        # Surface every FAIL section for the certificate report.
        printf '%s\n' "$out" | grep -Ei 'inconsisten|dangling|orphan|without recoverable|corrupt|failed|issue' || true
        return 1
    fi
    if ! printf '%s\n' "$out" | grep -q 'doctor: all checks passed'; then
        bad "$label: doctor printed issues without a non-zero exit"
        return 1
    fi
    ok "$label: all sections passed, zero FAIL sections"
    rm -rf "$dir"
    return 0
}

# --------------------------------------------------- fault campaign gate --
# P1 release-certificate completeness: a release certificate REQUIRES the
# fault campaign — the `faktor-tests-fault` crate must exist, at least one
# `[fault]` test must actually execute, and none may fail. The certificate
# records the executed count and the sha256 digest of the executed test
# identities. In non-release runs absence/zero is an informational skip that
# can only ever yield SOURCE CERTIFICATE, never a release PASS.
FAULT_STATUS="not-run"
FAULT_COUNT=0
FAULT_DIGEST=""
FAULT_RECORD="${CERTIFY_FAULT_RECORD:-target/certification/fault-tests.txt}"

fault_crate_present() {
    cargo metadata --no-deps --format-version 1 2>/dev/null \
        | grep -q '"name":"faktor-tests-fault"'
}

fault_record() { # status executed names_file
    FAULT_STATUS="$1"
    FAULT_COUNT="$2"
    local names_file="${3:-}"
    if [ -n "$names_file" ] && [ -s "$names_file" ]; then
        mkdir -p "$(dirname "$FAULT_RECORD")" 2>/dev/null || true
        sort -u "$names_file" >"$FAULT_RECORD" 2>/dev/null || true
        if [ -s "$FAULT_RECORD" ]; then
            FAULT_DIGEST="sha256:$(hash_file "$FAULT_RECORD")"
        else
            FAULT_DIGEST=""
        fi
    else
        : >"$FAULT_RECORD" 2>/dev/null || true
        FAULT_DIGEST=""
    fi
}

fault_release_gate() {
    [ "$FAULT_STATUS" = "pass" ] && [ "$FAULT_COUNT" -gt 0 ]
}

fault_campaign() {
    local fault_tmp names_file log_file list_rc executed failed count
    if ! fault_crate_present; then
        fault_record "crate-absent" 0 ""
        if [ "$RELEASE_REQUIRED" -eq 1 ]; then
            bad "release class REQUIRES the fault campaign but faktor-tests-fault is absent from this workspace"
            return 1
        fi
        ok "fault campaign informational skip: no faktor-tests-fault crate; release class impossible"
        return 0
    fi
    fault_tmp="$(mktemp -d "${TMPDIR:-/tmp}/faktor-cert-fault.XXXXXX")"
    names_file="$fault_tmp/fault-tests.txt"
    log_file="$fault_tmp/fault-run.log"
    cargo test -p faktor-tests-fault -- --ignored --list >"$names_file" 2>&1
    list_rc=$?
    if [ "$list_rc" -ne 0 ] && ! grep -qE ': test$' "$names_file" 2>/dev/null; then
        rm -rf "$fault_tmp"
        fault_record "list-failed" 0 ""
        bad "fault campaign could not enumerate its ignored tests (exit $list_rc)"
        return 1
    fi
    grep -E ': test$' "$names_file" 2>/dev/null | sed 's/: test$//' | sort -u >"$fault_tmp/names.txt" || true
    mv "$fault_tmp/names.txt" "$names_file"
    count="$(wc -l <"$names_file" | tr -d ' ')"
    if [ "${count:-0}" -eq 0 ]; then
        rm -rf "$fault_tmp"
        fault_record "zero" 0 ""
        if [ "$RELEASE_REQUIRED" -eq 1 ]; then
            bad "release class REQUIRES the fault campaign but zero [fault] tests were enumerated"
            return 1
        fi
        printf '   note: fault campaign enumerated zero [fault] tests; release class impossible\n'
        return 0
    fi
    if ! cargo test -p faktor-tests-fault -- --ignored --test-threads=1 >"$log_file" 2>&1; then
        bad "fault campaign failed (log tail):"
        tail -n 50 "$log_file"
        rm -rf "$fault_tmp"
        fault_record "fail" 0 ""
        return 1
    fi
    executed="$(grep -cE '^test .+ \.\.\. ok$' "$log_file" || true)"
    failed="$(grep -cE '^test .+ \.\.\. FAILED$' "$log_file" || true)"
    if [ "${failed:-0}" -gt 0 ]; then
        bad "fault campaign reported $failed failed test(s) (log tail):"
        tail -n 50 "$log_file"
        rm -rf "$fault_tmp"
        fault_record "fail" 0 ""
        return 1
    fi
    [ "${executed:-0}" -gt 0 ] || executed="$count"
    grep -E '^test .+ \.\.\. ok$' "$log_file" 2>/dev/null | sed -E 's/^test (.*) \.\.\. ok$/\1/' | sort -u >"$fault_tmp/executed.txt" || true
    [ -s "$fault_tmp/executed.txt" ] || cp "$names_file" "$fault_tmp/executed.txt"
    fault_record "pass" "$executed" "$fault_tmp/executed.txt"
    rm -rf "$fault_tmp"
    ok "fault campaign passed ([fault] tests executed=$executed digest=$FAULT_DIGEST)"
    return 0
}

# ------------------------------------------------- ReleaseArtifactSet --
# P1 release-certificate completeness: `collect_artifacts()` may legally
# return an empty TSV, so a release-class certificate requires a DECLARED
# expected set instead. Every kind below is mandatory; release PASS needs
# matched == required, required > 0, digest coverage by the signed
# attestation for every required artifact, and a clean branding scan where
# the scanner applies.
RELEASE_ARTIFACT_KINDS=(faktor-cli vscode-vsix jetbrains-plugin)

release_artifact_kind() { # path -> kind ('' = not a required-set member)
    case "$1" in
    */faktor-cli | faktor-cli) printf 'faktor-cli' ;;
    *.vsix) printf 'vscode-vsix' ;;
    *jetbrains*.zip | */build/distributions/*.zip) printf 'jetbrains-plugin' ;;
    *) printf '' ;;
    esac
}

release_kind_index() { # kind -> index (rc 1 = unknown kind)
    local k i=0
    for k in ${RELEASE_ARTIFACT_KINDS[@]+"${RELEASE_ARTIFACT_KINDS[@]}"}; do
        if [ "$k" = "$1" ]; then
            printf '%s' "$i"
            return 0
        fi
        i=$((i + 1))
    done
    return 1
}

RA_STATUS="not-run"
RA_REQUIRED=0
RA_MATCHED=0
RA_ATTESTED=0
RA_BRANDING="not-run"
RA_MISSING=""
RA_REQUIRED_TSV=""
ARTIFACTS_TSV=""
ARTIFACT_TMP=""

evaluate_release_artifact_set() { # artifacts_tsv -> RA_* globals
    local tsv="$1" kind i sha path
    RA_REQUIRED_TSV="${tsv%.tsv}.required.tsv"
    RA_REQUIRED="${#RELEASE_ARTIFACT_KINDS[@]}"
    RA_MATCHED=0
    RA_ATTESTED=0
    RA_MISSING=""
    local counts=() paths=() digests=()
    i=0
    while [ "$i" -lt "$RA_REQUIRED" ]; do
        counts[$i]=0
        paths[$i]=""
        digests[$i]=""
        i=$((i + 1))
    done
    local ambiguous=0
    while IFS="$(printf '\t')" read -r sha path; do
        [ -n "$path" ] || continue
        kind="$(release_artifact_kind "$path")"
        [ -n "$kind" ] || continue
        i="$(release_kind_index "$kind")"
        counts[$i]=$(( ${counts[$i]} + 1 ))
        paths[$i]="$path"
        digests[$i]="$sha"
    done <"$tsv"
    : >"$RA_REQUIRED_TSV"
    i=0
    while [ "$i" -lt "$RA_REQUIRED" ]; do
        kind="${RELEASE_ARTIFACT_KINDS[$i]}"
        case "${counts[$i]}" in
        1)
            RA_MATCHED=$((RA_MATCHED + 1))
            printf '%s\t%s\t%s\n' "$kind" "${digests[$i]}" "${paths[$i]}" >>"$RA_REQUIRED_TSV"
            ;;
        0)
            RA_MISSING="${RA_MISSING}${RA_MISSING:+,}$kind"
            ;;
        *)
            RA_MISSING="${RA_MISSING}${RA_MISSING:+,}${kind}(duplicate)"
            ambiguous=1
            ;;
        esac
        i=$((i + 1))
    done
    if [ "$ambiguous" -eq 1 ]; then
        RA_STATUS="ambiguous"
    elif [ "$RA_MATCHED" -eq 0 ]; then
        RA_STATUS="empty"
    elif [ "$RA_MATCHED" -lt "$RA_REQUIRED" ]; then
        RA_STATUS="partial"
    else
        RA_STATUS="complete"
    fi
    return 0
}

release_branding_scan() {
    # Brand every REQUIRED packaged output. The daemon binary is the
    # documented exemption (binary strings are not swept; see the header).
    if [ "$RA_STATUS" != "complete" ]; then
        RA_BRANDING="not-applicable"
        ok "release artifact branding: not applicable (artifact set: $RA_STATUS ${RA_MATCHED}/${RA_REQUIRED})"
        return 0
    fi
    local kind sha path dir d dirs=() seen=0
    while IFS="$(printf '\t')" read -r kind sha path; do
        [ -n "$kind" ] || continue
        case "$kind" in
        faktor-cli) continue ;;
        esac
        dir="$(dirname "$path")"
        seen=0
        for d in ${dirs[@]+"${dirs[@]}"}; do
            [ "$d" = "$dir" ] && seen=1
        done
        [ "$seen" -eq 1 ] || dirs+=("$dir")
    done <"$RA_REQUIRED_TSV"
    if [ "${#dirs[@]}" -eq 0 ]; then
        RA_BRANDING="pass"
        ok "release artifact branding: only scanner-exempt artifacts in the required set"
        return 0
    fi
    for d in "${dirs[@]}"; do
        if bash scripts/branding-scan.sh --artifacts "$d"; then
            ok "release artifact branding clean: $d"
        else
            RA_BRANDING="fail"
            bad "release artifact branding FAILED: $d"
            return 1
        fi
    done
    RA_BRANDING="pass"
    return 0
}

# ===================================================== exact-SHA CI gate =
# Gate 9: verify the required Woodpecker context at the exact commit, fetch
# and verify the trusted-build attestation, and write the release manifest.
# Everything below is additive to the local gates above; it needs no cargo.

ATTESTATION_JS="$ROOT/scripts/certification/attestation.mjs"
ATTESTATION_STEP="attestation"

HASH_TOOL=()
if command -v sha256sum >/dev/null 2>&1; then
    HASH_TOOL=(sha256sum)
elif command -v shasum >/dev/null 2>&1; then
    HASH_TOOL=(shasum -a 256)
fi

hash_file() {
    [ "${#HASH_TOOL[@]}" -gt 0 ] || return 2
    "${HASH_TOOL[@]}" "$1" | cut -d' ' -f1
}

resolve_commit() {
    local ref="${COMMIT:-HEAD}" sha
    sha="$(git rev-parse --verify "${ref}^{commit}" 2>/dev/null)" || return 2
    case "$sha" in
    *[!0-9a-f]* | "") return 2 ;;
    esac
    [ "${#sha}" -eq 40 ] || return 2
    printf '%s' "$sha"
}

require_woodpecker_env() {
    if [ -z "${WOODPECKER_HOST:-}" ] || [ -z "${WOODPECKER_TOKEN:-}" ]; then
        printf 'certify: OPERATOR ACTION REQUIRED — exact-SHA CI verification cannot run without the Woodpecker API.\n' >&2
        printf '  Set WOODPECKER_HOST (https://<woodpecker-host>) and WOODPECKER_TOKEN (a repo-read token):\n' >&2
        printf '    WOODPECKER_HOST=... WOODPECKER_TOKEN=... bash scripts/certify.sh --commit %s\n' "${COMMIT:-<40-hex-sha>}" >&2
        printf '  Commit messages, PR descriptions and local green test runs are NOT evidence: only the\n' >&2
        printf '  required context %s succeeding at the exact SHA certifies. Use --local-only for a\n' "$CONTEXT" >&2
        printf '  pre-flight that is explicitly NOT a release certificate, or --selftest for the hermetic matrix.\n' >&2
        return 2
    fi
    command -v curl >/dev/null 2>&1 || {
        echo "certify: curl is required for CI verification" >&2
        return 2
    }
    command -v python3 >/dev/null 2>&1 || {
        echo "certify: python3 is required for CI verification" >&2
        return 2
    }
    command -v node >/dev/null 2>&1 || {
        echo "certify: node is required to verify the trusted-build attestation (scripts/certification/attestation.mjs)" >&2
        return 2
    }
    return 0
}

py_json() { # mode file [arg]
    python3 - "$@" <<'PY'
import base64
import json
import re
import sys


def load(path):
    try:
        with open(path) as fh:
            return json.load(fh)
    except Exception:
        return None


def number(value):
    try:
        return int(value)
    except Exception:
        return 0


mode = sys.argv[1]
data = load(sys.argv[2])
arg = sys.argv[3] if len(sys.argv) > 3 else ""
arg2 = sys.argv[4] if len(sys.argv) > 4 else ""

if mode == "repo_id":
    print(data.get("id", "") if isinstance(data, dict) else "")
elif mode == "field":
    value = data
    for part in arg.split("."):
        if isinstance(value, dict):
            value = value.get(part)
        else:
            value = None
            break
    if value is None:
        print("")
    elif isinstance(value, bool):
        print("true" if value else "false")
    else:
        print(value)
elif mode == "candidates":
    rows = data if isinstance(data, list) else []
    for p in sorted(rows, key=lambda x: number(x.get("number")), reverse=True):
        print("%s\t%s\t%s\t%s\t%s" % (
            p.get("number", ""), p.get("id", ""), p.get("status", ""),
            p.get("commit", ""), p.get("event", ""),
        ))
elif mode == "workflow_state":
    for w in (data or {}).get("workflows") or []:
        if w.get("name") == arg:
            print(w.get("state", ""))
            break
elif mode == "attestation_step":
    steps = []
    for w in (data or {}).get("workflows") or []:
        for key in ("children", "steps", "tasks"):
            if isinstance(w.get(key), list):
                steps.extend(w[key])
    if isinstance((data or {}).get("steps"), list):
        steps.extend(data["steps"])
    for s in steps:
        if isinstance(s, dict) and s.get("name") == arg:
            # `id` is the global step id the log endpoint addresses; `pid` is
            # only the step's ordinal inside its workflow/pipeline and fetches
            # a different step's log (which made a published attestation look
            # absent).
            print(s.get("id", s.get("pid", "")))
            break
elif mode == "context_boundaries":
    # Classify the workflows a certified context does NOT own when the
    # certified workflow is green but the overall pipeline is not: a pending
    # workflow whose platform label no CONNECTED agent advertises is a typed
    # self-hosted-absence boundary (visible, never a silent green); anything
    # else stays a hard failure for the caller.
    agents = load(arg2) if arg2 else []
    platforms = set()
    if isinstance(agents, list):
        for a in agents:
            if not isinstance(a, dict) or not a.get("last_contact"):
                continue
            labels = a.get("custom_labels") or {}
            if isinstance(labels, dict) and labels.get("platform"):
                platforms.add(labels["platform"])
            elif a.get("platform"):
                platforms.add(a["platform"])
    for w in (data or {}).get("workflows") or []:
        # Matrix workflows share the pipeline's name, so the certified one is
        # identified by state: every green workflow is already proven, and
        # anything else is classified below.
        name = w.get("name", "")
        state = w.get("state", "")
        if state == "success":
            continue
        platform = (w.get("environ") or {}).get("platform") or ""
        if state in ("pending", "running", "blocked", "created", "started") and platform and platform not in platforms:
            print("BOUNDARY\tworkflow %s is %s and no connected agent advertises platform=%s (self-hosted lane absent; typed, visible, never green)" % (name, state, platform))
        else:
            print("PROBLEM\tworkflow %s is %s (platform=%s)" % (name, state, platform or "unknown"))
elif mode == "attestation_extract":
    # The log endpoint returns each entry's `data` base64-encoded; older
    # shapes may carry plain text. Search BOTH renderings so the block is
    # found either way (never silently missing).
    raw_parts = []
    decoded_parts = []
    if isinstance(data, list):
        for e in data:
            if not isinstance(e, dict) or not isinstance(e.get("data"), str):
                continue
            raw_parts.append(e["data"])
            try:
                decoded_parts.append(base64.b64decode(e["data"]).decode("utf-8", "replace"))
            except Exception:
                decoded_parts.append(e["data"])
    elif isinstance(data, dict):
        if isinstance(data.get("data"), str):
            raw_parts.append(data["data"])
            try:
                decoded_parts.append(base64.b64decode(data["data"]).decode("utf-8", "replace"))
            except Exception:
                decoded_parts.append(data["data"])
        elif isinstance(data.get("lines"), list):
            decoded_parts.extend(x for x in data["lines"] if isinstance(x, str))
    text = "\n".join(raw_parts) + "\n" + "\n".join(decoded_parts)
    match = re.search(
        r"-----BEGIN FAKTOR ATTESTATION-----(.*?)-----END FAKTOR ATTESTATION-----",
        text, re.S,
    )
    if not match:
        sys.exit(3)
    blob = "".join(match.group(1).split())
    try:
        obj = json.loads(base64.b64decode(blob).decode())
    except Exception as exc:
        print("attestation-extract: %s" % exc, file=sys.stderr)
        sys.exit(4)
    if not isinstance(obj, dict):
        sys.exit(4)
    print(json.dumps(obj, sort_keys=True))
else:
    sys.exit(2)
PY
}

api_get() { # api path out_file -> http code on stdout
    local api="$1" path="$2" out="$3" code
    code="$(curl -sS -o "$out" -w '%{http_code}' \
        -H "Authorization: Bearer ${WOODPECKER_TOKEN}" \
        -H 'Accept: application/json' "${api}${path}" 2>/dev/null)" || code="000"
    printf '%s' "$code"
}

collect_artifacts() { # out_tsv -> 0 ok, 2 operator error
    local out="$1" p h paths=()
    if [ "${#ARTIFACTS[@]}" -gt 0 ]; then
        paths=("${ARTIFACTS[@]}")
    else
        # Discovery root: workspace-relative by default; CERTIFY_ARTIFACT_ROOT
        # lets selftests point discovery at a hermetic fixture tree.
        local art_root="${CERTIFY_ARTIFACT_ROOT:-}"
        while IFS= read -r p; do
            [ -n "$p" ] || continue
            paths+=("$p")
        done < <(
            {
                if [ -n "$art_root" ]; then
                    ls "$art_root"/apps/vscode/*.vsix 2>/dev/null
                    find "$art_root/apps/jetbrains" -type f -path '*/build/distributions/*.zip' 2>/dev/null
                    [ -f "$art_root/target/release/faktor-cli" ] && printf '%s\n' "$art_root/target/release/faktor-cli"
                else
                    ls apps/vscode/*.vsix 2>/dev/null
                    find apps/jetbrains -type f -path '*/build/distributions/*.zip' 2>/dev/null
                    [ -f target/release/faktor-cli ] && printf '%s\n' target/release/faktor-cli
                fi
            } | sort -u
        )
    fi
    : >"$out"
    for p in ${paths[@]+"${paths[@]}"}; do
        if [ ! -f "$p" ]; then
            echo "certify: artifact '$p' does not exist" >&2
            return 2
        fi
        h="$(hash_file "$p")" || {
            echo "certify: no sha256 tool available" >&2
            return 2
        }
        printf 'sha256:%s\t%s\n' "$h" "$p" >>"$out"
    done
    return 0
}

prepare_artifacts() { # [out_tsv] -> collects + evaluates the ReleaseArtifactSet
    local out="${1:-}"
    if [ -z "$out" ]; then
        [ -n "$ARTIFACT_TMP" ] || ARTIFACT_TMP="$(mktemp -d "${TMPDIR:-/tmp}/faktor-cert-artifacts.XXXXXX")"
        out="$ARTIFACT_TMP/artifacts.tsv"
    fi
    collect_artifacts "$out" || return 2
    ARTIFACTS_TSV="$out"
    evaluate_release_artifact_set "$out"
    return 0
}

# ---------------------------------------------------------------------------
# Attestation probe: find the `attestation` step of one pipeline, fetch its
# log block, verify the signature and every local artifact digest. Returns
# 0 verified, 1 invalid (problems appended), 3 no attestation step.
# ---------------------------------------------------------------------------
ATT_STATUS="absent"
ATT_ORIGIN=""
ATT_PIPELINE_NUMBER=""
ATT_PIPELINE_ID=""
ATT_STEP_PID=""
ATT_IDENTITY=""
ATT_SOURCE_SHA=""
ATT_TREE_SHA=""
ATT_MATCHED="0"
ATT_ARTIFACTS="0"
ATT_DIGEST=""

probe_attestation() { # api repo_id number detail event workflow commit tree artifacts_tsv problems tmp pipeline_id
    local api="$1" repo_id="$2" number="$3" detail="$4" event="$5" workflow="$6"
    local commit="$7" tree="$8" artifacts_tsv="$9" problems="${10}" tmp="${11}" pipeline_id="${12}"
    local pid log_file att_file out rc code
    pid="$(py_json attestation_step "$detail" "$ATTESTATION_STEP")"
    if [ -z "$pid" ]; then
        ATT_STATUS="absent"
        return 3
    fi
    log_file="$tmp/attestation-log.json"
    att_file="$tmp/attestation.json"
    code="$(api_get "$api" "/repos/$repo_id/logs/$number/$pid" "$log_file")"
    if [ "$code" != "200" ]; then
        printf 'attestation-log-fetch: pipeline %s step %s log fetch failed (HTTP %s)\n' "$number" "$pid" "$code" >>"$problems"
        ATT_STATUS="failed"
        return 1
    fi
    if ! py_json attestation_extract "$log_file" >"$att_file"; then
        printf 'attestation-log-parse: pipeline %s step %s published no valid attestation block\n' "$number" "$pid" >>"$problems"
        ATT_STATUS="failed"
        return 1
    fi
    if [ -z "$ATT_KEYS" ]; then
        printf 'attestation-keys-missing: a signed attestation exists for pipeline %s but no allowlist is configured (set FAKTOR_ATTEST_KEYS or pass --attestation-keys)\n' "$number" >>"$problems"
        ATT_STATUS="failed"
        return 1
    fi
    command -v node >/dev/null 2>&1 || return 2
    local args=()
    local art_sha art_path
    while IFS="$(printf '\t')" read -r art_sha art_path; do
        [ -n "$art_path" ] || continue
        args+=(--artifact "$art_path")
    done <"$artifacts_tsv"
    out="$(node "$ATTESTATION_JS" verify --attestation "$att_file" \
        --repo "$REPO_FULL_NAME" \
        --source-sha "$commit" --tree-sha "$tree" --workflow "$workflow" --event "$event" \
        --pipeline-number "$number" --pipeline-id "$pipeline_id" \
        --require-signed --keys "$ATT_KEYS" ${args[@]+"${args[@]}"} 2>&1)"
    rc=$?
    ATT_DIGEST="sha256:$(hash_file "$att_file")"
    ATT_STEP_PID="$pid"
    ATT_SOURCE_SHA="$commit"
    ATT_TREE_SHA="$tree"
    ATT_IDENTITY="$(py_json field "$att_file" signature.identity)"
    ATT_ARTIFACTS="$(python3 -c 'import json,sys; a=json.load(open(sys.argv[1])).get("artifacts"); print(len(a) if isinstance(a, dict) else 0)' "$att_file" 2>/dev/null || printf '0')"
    if [ "$rc" -ne 0 ]; then
        printf 'attestation-invalid: pipeline %s: %s\n' "$number" "$(printf '%s\n' "$out" | grep -m1 'attestation problem:' || printf '%s\n' "$out" | head -n1)" >>"$problems"
        ATT_STATUS="failed"
        return 1
    fi
    ATT_STATUS="verified"
    ATT_PIPELINE_NUMBER="$number"
    ATT_PIPELINE_ID="$pipeline_id"
    ATT_MATCHED="$(printf '%s\n' "$out" | sed -n 's/.*artifacts-matched=\([0-9][0-9]*\).*/\1/p' | head -n1)"
    [ -n "$ATT_MATCHED" ] || ATT_MATCHED="0"
    # P1 completeness: every REQUIRED artifact must appear in this signed
    # attestation with a matching digest. This is computed from the decoded
    # attestation itself, so an empty local artifact list can never pass.
    RA_ATTESTED="0"
    if [ -n "$RA_REQUIRED_TSV" ] && [ -f "$RA_REQUIRED_TSV" ]; then
        RA_ATTESTED="$(python3 - "$att_file" "$RA_REQUIRED_TSV" <<'PY'
import json
import os
import sys

try:
    artifacts = json.load(open(sys.argv[1])).get("artifacts") or {}
except Exception:
    artifacts = {}
if not isinstance(artifacts, dict):
    artifacts = {}
count = 0
try:
    lines = open(sys.argv[2]).read().splitlines()
except Exception:
    lines = []
for line in lines:
    parts = line.split("\t")
    if len(parts) != 3:
        continue
    _kind, sha, path = parts
    if artifacts.get(os.path.basename(path)) == sha:
        count += 1
print(count)
PY
)"
    fi
    [ -n "$RA_ATTESTED" ] || RA_ATTESTED="0"
    return 0
}

# Find a trusted pipeline at the exact SHA that published an attestation and
# verify it (used when the selected context itself is untrusted, so a signed
# attestation is what upgrades the run to release class). A supplied
# WOODPECKER_TRUSTED_REPO_ID or a `?project=trusted` lookup answer is only a
# CLAIM: the project record is re-fetched and its observed identity, config
# file and trusted class must match before any pipeline is read from it.
# Returns 0 verified, 1 fetched-but-invalid, 3 absent.
find_trusted_attestation() { # api commit tree artifacts_tsv problems notes tmp
    local api="$1" commit="$2" tree="$3" artifacts_tsv="$4" problems="$5" notes="$6" tmp="$7"
    local tctx tspec tevent twf tconfig trepo_id tbody tcand tnumber tid tdetail tcommit tevent_obs code prc
    local number id status cand_commit cand_event trepo_file trepo_full trepo_config trepo_trusted tobs_id
    for tctx in $(context_registry_list); do
        [ "$tctx" = "$CONTEXT" ] && continue
        tspec="$(context_registry_entry "$tctx")" || continue
        set -- $tspec
        tevent="$1"
        twf="$2"
        tconfig="$4"
        trepo_id="${WOODPECKER_TRUSTED_REPO_ID:-${WOODPECKER_REPO_ID:-}}"
        if [ -z "$trepo_id" ]; then
            tbody="$tmp/trusted-lookup.json"
            code="$(api_get "$api" "/repos/lookup/$REPO_FULL_NAME?project=trusted" "$tbody")"
            if [ "$code" != "200" ]; then
                printf 'attestation-scan: trusted project lookup for %s failed (HTTP %s)\n' "$tctx" "$code" >>"$notes"
                continue
            fi
            trepo_id="$(py_json repo_id "$tbody")"
            [ -n "$trepo_id" ] || continue
        fi
        # P1: the claimed trusted project is re-fetched and must expose the
        # observed identity/config/trust facts; the claim alone certifies nothing.
        trepo_file="$tmp/trusted-repo.json"
        code="$(api_get "$api" "/repos/$trepo_id" "$trepo_file")"
        if [ "$code" != "200" ]; then
            printf 'attestation-scan: trusted repository %s detail fetch failed (HTTP %s)\n' "$trepo_id" "$code" >>"$notes"
            continue
        fi
        trepo_full="$(py_json field "$trepo_file" full_name)"
        trepo_config="$(py_json field "$trepo_file" config_file)"
        trepo_trusted="$(py_json field "$trepo_file" trusted.volumes)"
        if [ "$trepo_full" != "$REPO_FULL_NAME" ]; then
            printf 'attestation-trusted-repo-mismatch: repository %s is %s, not %s\n' "$trepo_id" "${trepo_full:-<unknown>}" "$REPO_FULL_NAME" >>"$problems"
            continue
        fi
        if [ "$trepo_config" != "$tconfig" ]; then
            printf 'attestation-trusted-config-mismatch: repository %s config_file %s is not %s\n' "$trepo_id" "${trepo_config:-<unknown>}" "$tconfig" >>"$problems"
            continue
        fi
        if [ "$trepo_trusted" != "true" ]; then
            printf 'attestation-trusted-class-mismatch: repository %s trusted.volumes=%s is not true\n' "$trepo_id" "${trepo_trusted:-<unknown>}" >>"$problems"
            continue
        fi
        tcand="$tmp/trusted-candidates.tsv"
        tbody="$tmp/trusted-pipelines.json"
        code="$(api_get "$api" "/repos/$trepo_id/pipelines?event=$tevent&per_page=50" "$tbody")"
        if [ "$code" != "200" ]; then
            printf 'attestation-scan: pipeline list for %s failed (HTTP %s)\n' "$tctx" "$code" >>"$notes"
            continue
        fi
        py_json candidates "$tbody" >"$tcand"
        tnumber=""
        tid=""
        while IFS="$(printf '\t')" read -r number id status cand_commit cand_event; do
            [ -n "$number" ] || continue
            [ "$cand_commit" = "$commit" ] || continue
            [ "$cand_event" = "$tevent" ] || continue
            tnumber="$number"
            tid="$id"
            break
        done <"$tcand"
        if [ -z "$tnumber" ]; then
            printf 'attestation-scan: no %s pipeline at %s\n' "$tctx" "$commit" >>"$notes"
            continue
        fi
        tdetail="$tmp/trusted-detail.json"
        code="$(api_get "$api" "/repos/$trepo_id/pipelines/$tnumber" "$tdetail")"
        if [ "$code" != "200" ]; then
            printf 'attestation-scan: pipeline %s detail fetch failed (HTTP %s)\n' "$tnumber" "$code" >>"$notes"
            continue
        fi
        tcommit="$(py_json field "$tdetail" commit)"
        tevent_obs="$(py_json field "$tdetail" event)"
        if [ "$tcommit" != "$commit" ] || [ "$tevent_obs" != "$tevent" ]; then
            printf 'attestation-scan: trusted pipeline %s does not bind the exact SHA/event (commit=%s event=%s)\n' "$tnumber" "$tcommit" "$tevent_obs" >>"$notes"
            continue
        fi
        # The verifier must see the OBSERVED detail id, never the list id: the
        # unconditional pipeline-id binding then rejects a number/id substitution.
        tobs_id="$(py_json field "$tdetail" id)"
        if [ -n "$tid" ] && [ -n "$tobs_id" ] && [ "$tobs_id" != "$tid" ]; then
            printf 'attestation-scan: trusted pipeline %s listed id %s but detail id %s\n' "$tnumber" "$tid" "$tobs_id" >>"$problems"
            continue
        fi
        [ -n "$tobs_id" ] || tobs_id="$tid"
        probe_attestation "$api" "$trepo_id" "$tnumber" "$tdetail" "$tevent" "$twf" "$commit" "$tree" "$artifacts_tsv" "$problems" "$tmp" "$tobs_id"
        prc=$?
        case "$prc" in
        0)
            ATT_ORIGIN="$tctx"
            return 0
            ;;
        1)
            ATT_ORIGIN="$tctx"
            return 1
            ;;
        2)
            return 2
            ;;
        *)
            continue
            ;;
        esac
    done
    ATT_STATUS="absent"
    return 3
}

write_ci_manifest() { # out_file artifacts_file problems_file notes_file
    CERTIFY_M_OUT="$1" CERTIFY_M_ARTIFACTS="$2" CERTIFY_M_PROBLEMS="$3" CERTIFY_M_NOTES="$4" \
        CERTIFY_M_COMMIT="$OBS_COMMIT" CERTIFY_M_TREE="$OBS_TREE" \
        CERTIFY_M_REPO="$OBS_REPO" CERTIFY_M_REPO_ID="$OBS_REPO_ID" \
        CERTIFY_M_HOST="${WOODPECKER_HOST%/}" \
        CERTIFY_M_CONTEXT="$CONTEXT" \
        CERTIFY_M_CONTEXT_EVENT="$CTX_EVENT" CERTIFY_M_CONTEXT_WORKFLOW="$CTX_WORKFLOW" \
        CERTIFY_M_CONTEXT_CLASS="$CTX_CLASS" CERTIFY_M_CONTEXT_CONFIG_FILE="$CTX_CONFIG_FILE" \
        CERTIFY_M_OBS_EVENT="$OBS_EVENT" CERTIFY_M_OBS_WORKFLOW="$OBS_WORKFLOW" \
        CERTIFY_M_OBS_WORKFLOW_STATE="$OBS_WORKFLOW_STATE" CERTIFY_M_OBS_PIPELINE_STATUS="$OBS_PIPELINE_STATUS" \
        CERTIFY_M_OBS_PIPELINE_ID="$OBS_PIPELINE_ID" CERTIFY_M_OBS_PIPELINE_NUMBER="$OBS_PIPELINE_NUMBER" \
        CERTIFY_M_OBS_CONFIG_FILE="$OBS_CONFIG_FILE" CERTIFY_M_OBS_TRUSTED_VOLUMES="$OBS_TRUSTED_VOLUMES" \
        CERTIFY_M_RUN_URL="$RUN_URL" \
        CERTIFY_M_ATT_STATUS="$ATT_STATUS" CERTIFY_M_ATT_ORIGIN="$ATT_ORIGIN" \
        CERTIFY_M_ATT_PIPELINE="$ATT_PIPELINE_NUMBER" CERTIFY_M_ATT_PIPELINE_ID="$ATT_PIPELINE_ID" \
        CERTIFY_M_ATT_STEP_PID="$ATT_STEP_PID" CERTIFY_M_ATT_IDENTITY="$ATT_IDENTITY" \
        CERTIFY_M_ATT_SOURCE="$ATT_SOURCE_SHA" CERTIFY_M_ATT_TREE="$ATT_TREE_SHA" \
        CERTIFY_M_ATT_MATCHED="$ATT_MATCHED" CERTIFY_M_ATT_ARTIFACTS="$ATT_ARTIFACTS" \
        CERTIFY_M_ATT_DIGEST="$ATT_DIGEST" \
        CERTIFY_M_RA_STATUS="$RA_STATUS" CERTIFY_M_RA_REQUIRED="$RA_REQUIRED" \
        CERTIFY_M_RA_MATCHED="$RA_MATCHED" CERTIFY_M_RA_ATTESTED="$RA_ATTESTED" \
        CERTIFY_M_RA_BRANDING="$RA_BRANDING" CERTIFY_M_RA_MISSING="$RA_MISSING" \
        CERTIFY_M_FAULT_STATUS="$FAULT_STATUS" CERTIFY_M_FAULT_COUNT="$FAULT_COUNT" \
        CERTIFY_M_FAULT_DIGEST="$FAULT_DIGEST" CERTIFY_M_FAULT_TESTS="$FAULT_RECORD" \
        CERTIFY_M_LOCAL_STATUS="$LOCAL_STATUS" \
        python3 <<'PY'
import hashlib
import json
import os
from datetime import datetime, timezone


def env(name):
    return os.environ.get(name, "")


def digest(text):
    return "sha256:" + hashlib.sha256(text.encode()).hexdigest()


artifacts = []
with open(env("CERTIFY_M_ARTIFACTS")) as fh:
    for line in fh:
        line = line.rstrip("\n")
        if not line:
            continue
        sha, path = line.split("\t", 1)
        artifacts.append({"path": path, "sha256": sha})
artifacts.sort(key=lambda a: a["path"])
artifact_digest = digest("".join("%s\t%s\n" % (a["sha256"], a["path"]) for a in artifacts))

problems = []
with open(env("CERTIFY_M_PROBLEMS")) as fh:
    for line in fh:
        line = line.strip()
        if line:
            problems.append(line)

notes = []
with open(env("CERTIFY_M_NOTES")) as fh:
    for line in fh:
        line = line.strip()
        if line:
            notes.append(line)

context = {
    "name": env("CERTIFY_M_CONTEXT"),
    "event": env("CERTIFY_M_CONTEXT_EVENT"),
    "workflow": env("CERTIFY_M_CONTEXT_WORKFLOW"),
    "class": env("CERTIFY_M_CONTEXT_CLASS"),
    "config_file": env("CERTIFY_M_CONTEXT_CONFIG_FILE"),
    "registry": "immutable",
}
observed = {
    "repository": env("CERTIFY_M_REPO"),
    "repository_id": env("CERTIFY_M_REPO_ID"),
    "event": env("CERTIFY_M_OBS_EVENT"),
    "workflow": env("CERTIFY_M_OBS_WORKFLOW"),
    "workflow_state": env("CERTIFY_M_OBS_WORKFLOW_STATE"),
    "pipeline_status": env("CERTIFY_M_OBS_PIPELINE_STATUS"),
    "pipeline_id": env("CERTIFY_M_OBS_PIPELINE_ID"),
    "pipeline_number": env("CERTIFY_M_OBS_PIPELINE_NUMBER"),
    "config_file": env("CERTIFY_M_OBS_CONFIG_FILE"),
    "trusted_volumes": env("CERTIFY_M_OBS_TRUSTED_VOLUMES"),
}
attestation = {
    "status": env("CERTIFY_M_ATT_STATUS"),
    "origin_context": env("CERTIFY_M_ATT_ORIGIN"),
    "pipeline_number": env("CERTIFY_M_ATT_PIPELINE"),
    "pipeline_id": env("CERTIFY_M_ATT_PIPELINE_ID"),
    "step_pid": env("CERTIFY_M_ATT_STEP_PID"),
    "signature_identity": env("CERTIFY_M_ATT_IDENTITY"),
    "source_sha": env("CERTIFY_M_ATT_SOURCE"),
    "tree_sha": env("CERTIFY_M_ATT_TREE"),
    "artifacts_matched": env("CERTIFY_M_ATT_MATCHED"),
    "artifacts_attested": env("CERTIFY_M_ATT_ARTIFACTS"),
    "evidence_digest": env("CERTIFY_M_ATT_DIGEST"),
}
local_status = env("CERTIFY_M_LOCAL_STATUS") or "not-run"


def as_int(value):
    try:
        return int(value)
    except Exception:
        return 0


release_artifacts = {
    "status": env("CERTIFY_M_RA_STATUS") or "not-run",
    "required": as_int(env("CERTIFY_M_RA_REQUIRED")),
    "matched": as_int(env("CERTIFY_M_RA_MATCHED")),
    "attested": as_int(env("CERTIFY_M_RA_ATTESTED")),
    "missing": [item for item in env("CERTIFY_M_RA_MISSING").split(",") if item],
    "branding": env("CERTIFY_M_RA_BRANDING") or "not-run",
}
fault_tests = []
fault_tests_path = env("CERTIFY_M_FAULT_TESTS")
if fault_tests_path and os.path.exists(fault_tests_path):
    try:
        with open(fault_tests_path) as fh:
            fault_tests = [line.strip() for line in fh if line.strip()][:200]
    except Exception:
        fault_tests = []
fault_campaign = {
    "status": env("CERTIFY_M_FAULT_STATUS") or "not-run",
    "executed": as_int(env("CERTIFY_M_FAULT_COUNT")),
    "evidence_digest": env("CERTIFY_M_FAULT_DIGEST"),
    "tests": fault_tests,
}
# P1 completeness: a release certificate additionally requires the declared
# ReleaseArtifactSet to be complete (matched == required > 0), digest-covered
# by the signed attestation (attested == required), brand-scanned where
# applicable, plus a passed fault campaign with executed > 0. An empty
# artifact list or a branding skip can therefore never print the release class.
release_preconditions = {
    "required_artifacts": release_artifacts["required"] > 0,
    "matched_equals_required": (
        release_artifacts["required"] > 0
        and release_artifacts["matched"] == release_artifacts["required"]
    ),
    "attested_equals_required": (
        release_artifacts["required"] > 0
        and release_artifacts["attested"] == release_artifacts["required"]
    ),
    "artifact_set_complete": release_artifacts["status"] == "complete",
    "artifact_branding_pass": release_artifacts["branding"] == "pass",
    "fault_campaign_pass": fault_campaign["status"] == "pass",
    "fault_tests_executed": fault_campaign["executed"] > 0,
}
release_ready = all(release_preconditions.values())
release_certificate = (
    not problems
    and local_status == "pass"
    and (context["class"] == "trusted" or attestation["status"] == "verified")
    and release_ready
)
certificate_class = "release" if release_certificate else ("source" if not problems else "none")
core = {
    "artifacts": artifacts,
    "commit": env("CERTIFY_M_COMMIT"),
    "context": context,
    "event": observed["event"],
    "pipeline_number": observed["pipeline_number"],
    "pipeline_status": observed["pipeline_status"],
    "repository": observed["repository"],
    "run_url": env("CERTIFY_M_RUN_URL"),
    "tree": env("CERTIFY_M_TREE"),
    "workflow_state": observed["workflow_state"],
    "attestation": attestation,
    "release_artifacts": release_artifacts,
    "fault_campaign": fault_campaign,
}
manifest = {
    "schema": "faktor-release-certification/v2",
    "status": "passed" if not problems else "failed",
    "repository": observed["repository"],
    "repository_id": observed["repository_id"],
    "commit": env("CERTIFY_M_COMMIT"),
    "tree": env("CERTIFY_M_TREE"),
    "context": context,
    "woodpecker_host": env("CERTIFY_M_HOST"),
    "pipeline_number": observed["pipeline_number"],
    "pipeline_id": observed["pipeline_id"],
    "pipeline_status": observed["pipeline_status"],
    "workflow": observed["workflow"],
    "workflow_state": observed["workflow_state"],
    "event": observed["event"],
    "trusted_class": {
        "expected": context["class"],
        "observed_volumes": observed["trusted_volumes"],
    },
    "run_url": env("CERTIFY_M_RUN_URL"),
    "artifacts": artifacts,
    "artifact_digest": artifact_digest,
    "attestation": attestation,
    "release_artifacts": release_artifacts,
    "fault_campaign": fault_campaign,
    "release_preconditions": release_preconditions,
    "certificate_class": certificate_class,
    "local_gates": local_status,
    "release_certificate": release_certificate,
    "release_rule": (
        "release = full local gates pass AND (trusted context verified OR signed "
        "build attestation verified) AND ReleaseArtifactSet complete+attested "
        "(required>0, matched==required, branding pass) AND fault campaign pass "
        "with executed>0; no flag weakens this"
    ),
    "observed": observed,
    "evidence_digest": digest(json.dumps(core, sort_keys=True, separators=(",", ":"))),
    "evidence_policy": "commit-message claims are not evidence; only the observed CI context and signed attestation at this exact commit certify",
    "certified_at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
    "problems": problems,
    "notes": notes,
}
out = env("CERTIFY_M_OUT")
with open(out, "w") as fh:
    fh.write(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
print(manifest["status"])
print("true" if release_certificate else "false")
print(manifest["evidence_digest"])
PY
}

# Observed values (P1-I): every field below is loaded from the Woodpecker API
# responses, never from caller input.
OBS_COMMIT=""
OBS_TREE=""
OBS_REPO=""
OBS_REPO_ID=""
OBS_EVENT=""
OBS_WORKFLOW=""
OBS_WORKFLOW_STATE=""
OBS_PIPELINE_STATUS=""
OBS_PIPELINE_ID=""
OBS_PIPELINE_NUMBER=""
OBS_CONFIG_FILE=""
OBS_TRUSTED_VOLUMES=""
CTX_EVENT=""
CTX_WORKFLOW=""
CTX_CLASS=""
CTX_CONFIG_FILE=""
RUN_URL=""

verify_woodpecker_context() { # commit tree manifest_out -> 0 verified, 1 not verified, 2 operator error
    local commit="$1" tree="$2" manifest_out="$3"
    local spec
    if ! spec="$(context_registry_entry "$CONTEXT")"; then
        printf 'certify: context %s is not registered; the immutable registry has:\n' "$CONTEXT" >&2
        context_registry_list | sed 's/^/  /' >&2
        return 2
    fi
    set -- $spec
    CTX_EVENT="$1"
    CTX_WORKFLOW="$2"
    CTX_CLASS="$3"
    CTX_CONFIG_FILE="$4"

    require_woodpecker_env || return 2

    OBS_COMMIT="$commit"
    OBS_TREE="$tree"
    OBS_EVENT=""
    OBS_WORKFLOW=""
    OBS_WORKFLOW_STATE=""
    OBS_PIPELINE_STATUS=""
    OBS_PIPELINE_ID=""
    OBS_PIPELINE_NUMBER=""
    OBS_CONFIG_FILE=""
    OBS_TRUSTED_VOLUMES=""
    OBS_REPO=""
    OBS_REPO_ID=""
    RUN_URL=""
    ATT_STATUS="absent"
    ATT_ORIGIN=""
    ATT_PIPELINE_NUMBER=""
    ATT_PIPELINE_ID=""
    ATT_STEP_PID=""
    ATT_IDENTITY=""
    ATT_SOURCE_SHA=""
    ATT_TREE_SHA=""
    ATT_MATCHED="0"
    ATT_ARTIFACTS="0"
    ATT_DIGEST=""
    RA_ATTESTED="0"

    local api="${WOODPECKER_HOST%/}/api"
    local tmp body repo_file candidates_file detail_file artifacts_file problems_file notes_file
    local repo_id code number status cand_commit cand_event
    local pipeline_number="" pipeline_id=""
    local detail_commit="" detail_event="" detail_number="" detail_id=""

    if [ -z "$REPO_FULL_NAME" ]; then
        local remote
        remote="$(git remote get-url origin 2>/dev/null || true)"
        case "$remote" in
        *github.com[:/]*)
            REPO_FULL_NAME="$(printf '%s' "$remote" | sed -E 's#.*github\.com[:/]([^/]+/[^/]+?)(\.git)?$#\1#')"
            ;;
        esac
    fi
    case "$REPO_FULL_NAME" in
    */*) ;;
    *)
        echo "certify: cannot determine the repository; pass --repo owner/name or set WOODPECKER_REPO" >&2
        return 2
        ;;
    esac

    tmp="$(mktemp -d "${TMPDIR:-/tmp}/faktor-cert-ci.XXXXXX")"
    body="$tmp/body.json"
    repo_file="$tmp/repo.json"
    candidates_file="$tmp/candidates.tsv"
    detail_file="$tmp/pipeline.json"
    artifacts_file="$tmp/artifacts.tsv"
    problems_file="$tmp/problems.txt"
    notes_file="$tmp/notes.txt"
    : >"$problems_file"
    : >"$notes_file"

    # 1. repository id for the context's project class.
    if [ "$CTX_CLASS" = "trusted" ]; then
        repo_id="${WOODPECKER_TRUSTED_REPO_ID:-${WOODPECKER_REPO_ID:-}}"
    else
        repo_id="${WOODPECKER_UNTRUSTED_REPO_ID:-${WOODPECKER_REPO_ID:-}}"
    fi
    if [ -z "$repo_id" ]; then
        local lookup_path="/repos/lookup/$REPO_FULL_NAME"
        if [ "$CTX_CLASS" = "trusted" ]; then
            lookup_path="$lookup_path?project=trusted"
        fi
        code="$(api_get "$api" "$lookup_path" "$body")"
        if [ "$code" != "200" ]; then
            rm -rf "$tmp"
            if [ "$code" = "401" ] || [ "$code" = "403" ]; then
                echo "certify: Woodpecker rejected the token (HTTP $code) for ${api}${lookup_path}" >&2
            else
                echo "certify: Woodpecker repo lookup failed (HTTP $code) for ${api}${lookup_path}" >&2
            fi
            return 2
        fi
        repo_id="$(py_json repo_id "$body")"
        if [ -z "$repo_id" ]; then
            echo "certify: Woodpecker repo lookup returned no id" >&2
            rm -rf "$tmp"
            return 2
        fi
    fi
    OBS_REPO_ID="$repo_id"

    # 2. observed repository facts: identity, config file, trusted class.
    code="$(api_get "$api" "/repos/$repo_id" "$repo_file")"
    if [ "$code" != "200" ]; then
        rm -rf "$tmp"
        echo "certify: Woodpecker repository detail fetch failed (HTTP $code) for ${api}/repos/$repo_id" >&2
        return 2
    fi
    OBS_REPO="$(py_json field "$repo_file" full_name)"
    [ -n "$OBS_REPO" ] || OBS_REPO="$REPO_FULL_NAME"
    OBS_CONFIG_FILE="$(py_json field "$repo_file" config_file)"
    OBS_TRUSTED_VOLUMES="$(py_json field "$repo_file" trusted.volumes)"
    if [ "$OBS_REPO" != "$REPO_FULL_NAME" ]; then
        printf 'repository-mismatch: Woodpecker repository %s is not the requested %s\n' "$OBS_REPO" "$REPO_FULL_NAME" >>"$problems_file"
    fi
    if [ -n "$OBS_CONFIG_FILE" ] && [ "$OBS_CONFIG_FILE" != "$CTX_CONFIG_FILE" ]; then
        printf 'config-file-mismatch: project config_file %s is not the context path %s\n' "$OBS_CONFIG_FILE" "$CTX_CONFIG_FILE" >>"$problems_file"
    fi
    if [ "$CTX_CLASS" = "trusted" ]; then
        expected_trusted="true"
    else
        expected_trusted="false"
    fi
    if [ -z "$OBS_TRUSTED_VOLUMES" ]; then
        printf 'trusted-class-unknown: repository %s exposes no trusted.volumes fact\n' "$repo_id" >>"$problems_file"
    elif [ "$OBS_TRUSTED_VOLUMES" != "$expected_trusted" ]; then
        printf 'trusted-class-mismatch: observed trusted.volumes=%s, context %s requires %s\n' "$OBS_TRUSTED_VOLUMES" "$CONTEXT" "$expected_trusted" >>"$problems_file"
    fi

    # 3. candidates: the exact SHA's pipelines for the context's event.
    if [ -n "$PIPELINE" ]; then
        pipeline_number="$PIPELINE"
    else
        code="$(api_get "$api" "/repos/$repo_id/pipelines?event=$CTX_EVENT&per_page=50" "$body")"
        if [ "$code" != "200" ]; then
            rm -rf "$tmp"
            echo "certify: Woodpecker pipeline list failed (HTTP $code) for ${api}/repos/$repo_id/pipelines" >&2
            return 2
        fi
        py_json candidates "$body" >"$candidates_file"
        while IFS="$(printf '\t')" read -r number id status cand_commit cand_event; do
            [ -n "$number" ] || continue
            [ "$cand_commit" = "$commit" ] || continue
            [ "$cand_event" = "$CTX_EVENT" ] || continue
            pipeline_number="$number"
            pipeline_id="$id"
            break
        done <"$candidates_file"
        if [ -z "$pipeline_number" ]; then
            if [ -s "$candidates_file" ]; then
                printf 'context-absent: no %s pipeline at commit %s (pipelines exist for other commits/events; the context belongs to another SHA)\n' "$CTX_EVENT" "$commit" >>"$problems_file"
            else
                printf 'context-absent: no %s pipeline found for commit %s\n' "$CTX_EVENT" "$commit" >>"$problems_file"
            fi
        fi
    fi

    # 4. pipeline detail: OBSERVED event/workflow/state/id.
    if [ -n "$pipeline_number" ]; then
        code="$(api_get "$api" "/repos/$repo_id/pipelines/$pipeline_number" "$detail_file")"
        if [ "$code" != "200" ]; then
            rm -rf "$tmp"
            echo "certify: Woodpecker pipeline $pipeline_number fetch failed (HTTP $code)" >&2
            return 2
        fi
        detail_commit="$(py_json field "$detail_file" commit)"
        detail_event="$(py_json field "$detail_file" event)"
        detail_number="$(py_json field "$detail_file" number)"
        detail_id="$(py_json field "$detail_file" id)"
        OBS_EVENT="$detail_event"
        OBS_PIPELINE_NUMBER="$detail_number"
        OBS_PIPELINE_ID="$detail_id"
        [ -n "$OBS_PIPELINE_ID" ] || OBS_PIPELINE_ID="$detail_number"
        OBS_PIPELINE_STATUS="$(py_json field "$detail_file" status)"
        if [ "$detail_commit" != "$commit" ]; then
            printf 'sha-mismatch: pipeline %s belongs to commit %s, not the exact %s\n' "$pipeline_number" "${detail_commit:-<unknown>}" "$commit" >>"$problems_file"
        fi
        if [ "$detail_event" != "$CTX_EVENT" ]; then
            printf 'observed-event-mismatch: pipeline %s event=%s, context %s expects %s\n' "$pipeline_number" "${detail_event:-<unknown>}" "$CONTEXT" "$CTX_EVENT" >>"$problems_file"
        fi
        if [ -n "$detail_number" ] && [ "$detail_number" != "$pipeline_number" ]; then
            printf 'pipeline-number-mismatch: detail says %s, selected pipeline %s\n' "$detail_number" "$pipeline_number" >>"$problems_file"
        fi
        if [ -n "$pipeline_id" ] && [ -n "$detail_id" ] && [ "$detail_id" != "$pipeline_id" ]; then
            printf 'pipeline-id-mismatch: detail id %s, listed id %s\n' "$detail_id" "$pipeline_id" >>"$problems_file"
        fi
        OBS_WORKFLOW_STATE="$(py_json workflow_state "$detail_file" "$CTX_WORKFLOW")"
        if [ -z "$OBS_WORKFLOW_STATE" ]; then
            printf 'context-absent: pipeline %s has no workflow named %s\n' "$pipeline_number" "$CTX_WORKFLOW" >>"$problems_file"
        else
            OBS_WORKFLOW="$CTX_WORKFLOW"
            case "$OBS_WORKFLOW_STATE" in
            success)
                if [ "$OBS_PIPELINE_STATUS" != "success" ]; then
                    agents_file="$tmp/agents.json"
                    agents_code="$(api_get "$api" "/agents" "$agents_file")"
                    boundary_out=""
                    if [ "$agents_code" = "200" ]; then
                        boundary_out="$(py_json context_boundaries "$detail_file" "$CTX_WORKFLOW" "$agents_file" 2>/dev/null || true)"
                    fi
                    boundary_problems="$(printf '%s\n' "$boundary_out" | sed -n 's/^PROBLEM\t//p')"
                    boundary_notes="$(printf '%s\n' "$boundary_out" | sed -n 's/^BOUNDARY\t//p')"
                    if [ -n "$boundary_problems" ]; then
                        printf '%s\n' "$boundary_problems" >>"$problems_file"
                    fi
                    if [ -n "$boundary_notes" ]; then
                        printf '%s\n' "$boundary_notes"
                        printf 'context-boundary-checked: pipeline status is %s while the %s workflow is success; listed workflow(s) are typed self-hosted-absence boundaries (no connected agent, never green)\n' "$OBS_PIPELINE_STATUS" "$CTX_WORKFLOW"
                    elif [ -z "$boundary_problems" ]; then
                        printf 'context-failure: pipeline status is %s while the %s workflow is success\n' "$OBS_PIPELINE_STATUS" "$CTX_WORKFLOW" >>"$problems_file"
                    fi
                fi
                ;;
            pending | running | blocked | created | started)
                printf 'context-pending: %s workflow is %s (pipeline %s)\n' "$CTX_WORKFLOW" "$OBS_WORKFLOW_STATE" "$pipeline_number" >>"$problems_file"
                ;;
            failure | killed | canceled | declined | skipped)
                printf 'context-failure: %s workflow is %s (pipeline %s)\n' "$CTX_WORKFLOW" "$OBS_WORKFLOW_STATE" "$pipeline_number" >>"$problems_file"
                ;;
            error)
                printf 'context-error: %s workflow is error (pipeline %s)\n' "$CTX_WORKFLOW" "$pipeline_number" >>"$problems_file"
                ;;
            *)
                printf 'context-unknown: %s workflow state %s (pipeline %s)\n' "$CTX_WORKFLOW" "$OBS_WORKFLOW_STATE" "$pipeline_number" >>"$problems_file"
                ;;
            esac
        fi
    fi

    # 5. artifacts to bind into the manifest and the attestation. When the
    # caller already prepared the ReleaseArtifactSet (local flow or selftest),
    # reuse that exact list so the certificate binds the evaluated bytes.
    if [ -n "$ARTIFACTS_TSV" ] && [ -f "$ARTIFACTS_TSV" ]; then
        artifacts_file="$ARTIFACTS_TSV"
    else
        collect_artifacts "$artifacts_file" || {
            rm -rf "$tmp"
            return 2
        }
        evaluate_release_artifact_set "$artifacts_file"
    fi

    # 6. trusted-build attestation (P1-J).
    local att_rc=0
    if [ -n "$pipeline_number" ]; then
        case "$CTX_CLASS" in
        trusted)
            probe_attestation "$api" "$repo_id" "$pipeline_number" "$detail_file" \
                "$CTX_EVENT" "$CTX_WORKFLOW" "$commit" "$tree" "$artifacts_file" \
                "$problems_file" "$tmp" "$OBS_PIPELINE_ID"
            att_rc=$?
            case "$att_rc" in
            0) ATT_ORIGIN="$CONTEXT" ;;
            2)
                rm -rf "$tmp"
                return 2
                ;;
            3) printf 'attestation-absent: trusted pipeline %s published no %s step\n' "$pipeline_number" "$ATTESTATION_STEP" >>"$problems_file" ;;
            *) : ;; # probe_attestation already recorded the problem
            esac
            ;;
        untrusted)
            if [ -n "$ATT_KEYS" ]; then
                find_trusted_attestation "$api" "$commit" "$tree" "$artifacts_file" \
                    "$problems_file" "$notes_file" "$tmp"
                att_rc=$?
                case "$att_rc" in
                0) ok "trusted attestation verified (${ATT_ORIGIN} pipeline ${ATT_PIPELINE_NUMBER})" ;;
                2)
                    rm -rf "$tmp"
                    return 2
                    ;;
                1) : ;; # recorded as a problem
                *)
                    printf 'attestation-absent: no trusted pipeline at %s published a signed attestation\n' "$commit" >>"$notes_file"
                    ;;
                esac
            else
                ATT_STATUS="not-checked"
                printf 'attestation-not-checked: no key allowlist configured (FAKTOR_ATTEST_KEYS / --attestation-keys); release class requires a trusted context or a verified signed attestation\n' >>"$notes_file"
            fi
            ;;
        esac
    fi

    if [ -n "$pipeline_number" ]; then
        RUN_URL="${WOODPECKER_HOST%/}/repos/$repo_id/pipeline/$pipeline_number"
    fi

    # 7. manifest (written for pass and fail alike), plus observed-value facts.
    local manifest_output manifest_status release_flag
    manifest_output="$(write_ci_manifest "$manifest_out" "$artifacts_file" "$problems_file" "$notes_file")"
    manifest_status="$(printf '%s\n' "$manifest_output" | sed -n '1p')"
    release_flag="$(printf '%s\n' "$manifest_output" | sed -n '2p')"
    rm -rf "$tmp"

    case "$manifest_status" in
    passed)
        ok "Woodpecker context $CONTEXT verified at $commit (observed event=$OBS_EVENT workflow=$OBS_WORKFLOW state=$OBS_WORKFLOW_STATE pipeline=$OBS_PIPELINE_ID/$OBS_PIPELINE_NUMBER)"
        if [ "$release_flag" = "true" ]; then
            ok "release-class evidence: trusted context or verified signed attestation"
        fi
        return 0
        ;;
    *)
        echo "certify: CI verification FAILED for $commit — problems:" >&2
        if [ -f "$manifest_out" ]; then
            python3 -c 'import json,sys; [print("  - " + p) for p in json.load(open(sys.argv[1])).get("problems", [])]' "$manifest_out" >&2
        fi
        bad "CI context not certified; see $manifest_out"
        return 1
        ;;
    esac
}

# P1 completeness: release preconditions beyond context/attestation. Every
# one is mandatory for RELEASE CERTIFICATE; SOURCE CERTIFICATE can never
# print the release class.
release_preconditions_met() {
    [ "$RA_STATUS" = "complete" ] || return 1
    [ "$RA_REQUIRED" -gt 0 ] || return 1
    [ "$RA_MATCHED" -eq "$RA_REQUIRED" ] || return 1
    [ "$RA_ATTESTED" -eq "$RA_REQUIRED" ] || return 1
    [ "$RA_BRANDING" = "pass" ] || return 1
    [ "$FAULT_STATUS" = "pass" ] || return 1
    [ "$FAULT_COUNT" -gt 0 ] || return 1
    return 0
}

release_denial_reasons() {
    local reasons=()
    [ "$RA_REQUIRED" -gt 0 ] || reasons+=("ReleaseArtifactSet declares zero required artifacts")
    [ "$RA_STATUS" = "complete" ] || reasons+=("ReleaseArtifactSet status=$RA_STATUS matched=$RA_MATCHED/$RA_REQUIRED${RA_MISSING:+ missing=$RA_MISSING}")
    if [ "$RA_REQUIRED" -gt 0 ] && [ "$RA_ATTESTED" -ne "$RA_REQUIRED" ]; then
        reasons+=("signed attestation covers $RA_ATTESTED/$RA_REQUIRED required artifacts")
    fi
    [ "$RA_BRANDING" = "pass" ] || reasons+=("required-artifact branding/scanner: $RA_BRANDING")
    [ "$FAULT_STATUS" = "pass" ] || reasons+=("fault campaign: $FAULT_STATUS")
    [ "$FAULT_COUNT" -gt 0 ] || reasons+=("fault campaign executed 0 [fault] tests")
    printf '%s\n' ${reasons[@]+"${reasons[@]}"}
}

# P1-K/P1 completeness: the only rule that yields a release certificate.
certificate_class() { # local_pass ci_status ctx_class att_status -> release|source|none
    local local_pass="$1" ci_status="$2" ctx_class="$3" att_status="$4"
    if [ "$ci_status" -ne 0 ]; then
        printf 'none'
        return
    fi
    if [ "$local_pass" -ne 1 ]; then
        printf 'none'
        return
    fi
    if { [ "$ctx_class" = "trusted" ] || [ "$att_status" = "verified" ]; } \
        && release_preconditions_met; then
        printf 'release'
        return
    fi
    printf 'source'
}

run_ci_selftest() {
    command -v python3 >/dev/null 2>&1 || {
        echo "certify selftest: python3 is required" >&2
        return 2
    }
    command -v curl >/dev/null 2>&1 || {
        echo "certify selftest: curl is required" >&2
        return 2
    }
    command -v node >/dev/null 2>&1 || {
        echo "certify selftest: node is required (attestation verification)" >&2
        return 2
    }
    local fixtures tmp port pid ready sha tree failures=0 code manifest att_rc
    fixtures="$ROOT/scripts/certification/fixtures/woodpecker-api"
    if [ ! -d "$fixtures" ]; then
        echo "certify selftest: fixtures missing at $fixtures" >&2
        return 2
    fi
    sha="0123456789abcdef0123456789abcdef01234567"
    tree="fedcba9876543210fedcba9876543210fedcba98"
    tmp="$(mktemp -d "${TMPDIR:-/tmp}/faktor-cert-selftest.XXXXXX")"
    cp -R "$fixtures/." "$tmp/"
    printf 'selftest artifact\n' >"$tmp/artifact.bin"
    # P1 ReleaseArtifactSet fixtures: complete / subset / empty trees.
    release_fixtures="$ROOT/scripts/certification/fixtures/release-artifacts"
    ART_FULL=(
        "$release_fixtures/full/bin/faktor-cli"
        "$release_fixtures/full/extension/faktor.vsix"
        "$release_fixtures/full/plugin/faktor-jetbrains.zip"
    )
    # Tamper tests operate on a private copy so repo fixtures stay pristine.
    mkdir -p "$tmp/relfull/bin" "$tmp/relfull/extension" "$tmp/relfull/plugin"
    cp "${ART_FULL[0]}" "$tmp/relfull/bin/faktor-cli"
    cp "${ART_FULL[1]}" "$tmp/relfull/extension/faktor.vsix"
    cp "${ART_FULL[2]}" "$tmp/relfull/plugin/faktor-jetbrains.zip"
    ART_FULL=("$tmp/relfull/bin/faktor-cli" "$tmp/relfull/extension/faktor.vsix" "$tmp/relfull/plugin/faktor-jetbrains.zip")
    node "$ATTESTATION_JS" keygen --out-key "$tmp/sign.pem" --out-keys "$tmp/keys.json" --key-id faktor-selftest >/dev/null
    node "$ATTESTATION_JS" keygen --out-key "$tmp/foreign.pem" --out-keys "$tmp/foreign-keys.json" --key-id foreign >/dev/null

    port="$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')"
    python3 "$tmp/mock_server.py" "$port" "$tmp" >/dev/null 2>&1 &
    pid=$!
    ready=0
    for _ in $(seq 1 50); do
        if curl -sS -o /dev/null -H "Authorization: Bearer selftest-token" \
            "http://127.0.0.1:$port/api/repos/lookup/acme/widgets" 2>/dev/null; then
            ready=1
            break
        fi
        sleep 0.1
    done
    if [ "$ready" -ne 1 ]; then
        kill "$pid" 2>/dev/null || true
        echo "certify selftest: mock API did not become ready" >&2
        return 1
    fi

    WOODPECKER_HOST="http://127.0.0.1:$port"
    WOODPECKER_TOKEN="selftest-token"
    REPO_FULL_NAME="acme/widgets"
    CONTEXT="ci/woodpecker/pr/pr"
    PIPELINE=""
    ARTIFACTS=("${ART_FULL[@]}")
    ATT_KEYS="$tmp/keys.json"
    export WOODPECKER_HOST WOODPECKER_TOKEN

    # P1: the mock run certifies against the complete ReleaseArtifactSet, a
    # passing fault campaign and a clean branding scan of the required
    # packaged outputs, so release-class facts are real (not assumed).
    prepare_artifacts "$tmp/selftest-artifacts.tsv" || {
        echo "certify selftest: could not prepare the ReleaseArtifactSet" >&2
        kill "$pid" 2>/dev/null || true
        return 1
    }
    release_branding_scan
    FAULT_RECORD="$tmp/fault-tests.txt"
    printf '%s\n' 'campaigns::a' 'campaigns::b' 'campaigns::c' >"$tmp/fault-names.txt"
    fault_record "pass" 3 "$tmp/fault-names.txt"

    list_json() { # number id status commit event
        python3 -c 'import json,sys; print(json.dumps({"number":int(sys.argv[1]),"id":int(sys.argv[2]),"status":sys.argv[3],"commit":sys.argv[4],"event":sys.argv[5]}))' "$@"
    }

    detail_json() { # number id status commit event workflows [attestation_pid]
        python3 - "$@" <<'PY'
import json
import sys

number, pid_value, status, commit, event, workflows = sys.argv[1:7]
children = []
if len(sys.argv) > 7 and sys.argv[7]:
    children = [{"name": "attestation", "pid": int(sys.argv[7]), "state": "success"}]
items = []
for pair in workflows.split(","):
    if not pair:
        continue
    name, state = pair.split("=")
    items.append({"name": name, "state": state, "children": children if name == "trusted" else []})
print(json.dumps({"number": int(number), "id": int(pid_value), "status": status, "commit": commit, "event": event, "workflows": items}))
PY
    }

    details_json() { # number detail_json [number detail_json ...]
        python3 - "$@" <<'PY'
import json
import sys

out = {}
args = sys.argv[1:]
for i in range(0, len(args) - 1, 2):
    out[args[i]] = json.loads(args[i + 1])
print(json.dumps(out))
PY
    }

    set_state() {
        python3 - "$tmp/state.json" <<'PY'
import json
import os
import sys

state = {
    "lookup": {"id": 7, "full_name": "acme/widgets", "config_file": ".woodpecker/untrusted/", "trusted": {"volumes": False}},
    "lookup_trusted": {"id": 8, "full_name": "acme/widgets", "config_file": ".woodpecker/trusted/", "trusted": {"volumes": True}},
    "repo7": {"id": 7, "full_name": "acme/widgets", "config_file": ".woodpecker/untrusted/", "trusted": {"volumes": False}},
    "repo8": {"id": 8, "full_name": "acme/widgets", "config_file": ".woodpecker/trusted/", "trusted": {"volumes": True}},
    "pipelines": json.loads(os.environ.get("CERTIFY_TEST_PIPELINES", "[]")),
    "detail": json.loads(os.environ.get("CERTIFY_TEST_DETAIL", "null")),
    "details": json.loads(os.environ.get("CERTIFY_TEST_DETAILS", "{}")),
    "logs": json.loads(os.environ.get("CERTIFY_TEST_LOGS", "{}")),
}
if os.environ.get("CERTIFY_TEST_REPO7"):
    state["repo7"] = json.loads(os.environ["CERTIFY_TEST_REPO7"])
if os.environ.get("CERTIFY_TEST_REPO8"):
    state["repo8"] = json.loads(os.environ["CERTIFY_TEST_REPO8"])
with open(sys.argv[1], "w") as fh:
    json.dump(state, fh)
PY
    }

    log_block() { # attestation_file -> marker block text
        printf '%s\n' '-----BEGIN FAKTOR ATTESTATION-----'
        base64 <"$1" | tr -d '\n'
        printf '\n'
        printf '%s\n' '-----END FAKTOR ATTESTATION-----'
    }

    logs_json() { # marker_block_file pid
        python3 - "$1" "$2" <<'PY'
import json
import sys

with open(sys.argv[1]) as fh:
    block = fh.read()
print(json.dumps({sys.argv[2]: [{"data": block}]}))
PY
    }

    make_attestation() { # out_file [extra create args...]; ATT_REPO/ATT_PIPELINE_ID override
        local out="$1"
        shift
        local art_args=()
        local art
        for art in "${ART_FULL[@]}"; do
            art_args+=(--artifact "$art")
        done
        node "$ATTESTATION_JS" create \
            --out "$out" --workflow trusted --event push --repo "${ATT_REPO:-acme/widgets}" \
            --source-sha "$sha" --tree-sha "$tree" \
            --pipeline-number 21 --pipeline-id "${ATT_PIPELINE_ID:-211}" \
            --ci-image-ref 'node:24@sha256:64af3819f9275802414d7cdc38c27e9d82bd564dec4d4da87d008255d36c63b4' \
            --rust-toolchain 1.98.0 \
            "${art_args[@]}" \
            "$@"
    }

    write_block() { # out_file marker_text
        printf '%s\n' "$2" >"$1"
    }

    expect() { # name want
        local name="$1" want="$2"
        manifest="$tmp/manifest-$name.json"
        verify_woodpecker_context "$sha" "$tree" "$manifest" >/dev/null 2>&1
        code=$?
        if [ "$code" -eq "$want" ]; then
            echo "selftest ok: $name -> exit $code"
        else
            echo "selftest FAIL: $name -> exit $code (want $want)" >&2
            failures=$((failures + 1))
        fi
    }

    expect_problem() { # name code
        local name="$1" want_code="$2" hit
        hit="$(python3 - "$tmp/manifest-$name.json" "$want_code" <<'PY'
import json
import sys

try:
    problems = json.load(open(sys.argv[1])).get("problems", [])
except Exception:
    problems = []
print("yes" if any(sys.argv[2] in p for p in problems) else "no")
PY
)"
        if [ "$hit" = "yes" ]; then
            echo "selftest ok: $name carries problem '$want_code'"
        else
            echo "selftest FAIL: $name carries no problem '$want_code'" >&2
            failures=$((failures + 1))
        fi
    }

    expect_manifest() { # name python_assertions
        local name="$1" body="$2" out
        if out="$(python3 -c "$body" "$tmp/manifest-$name.json" 2>&1)"; then
            echo "selftest ok: $name manifest"
        else
            echo "selftest FAIL: $name manifest" >&2
            printf '%s\n' "$out" | tail -n 5 >&2
            failures=$((failures + 1))
        fi
    }

    # --- success: exact SHA, pull_request, pr workflow success, observed facts.
    CERTIFY_TEST_PIPELINES="[$(list_json 11 111 success "$sha" pull_request)]" \
        CERTIFY_TEST_DETAIL="$(detail_json 11 111 success "$sha" pull_request 'pr=success,linux=success')" \
        set_state
    expect success 0
    expect_manifest success '
import json, sys
m = json.load(open(sys.argv[1]))
assert m["status"] == "passed", m
assert m["schema"] == "faktor-release-certification/v2", m
assert m["commit"] == "0123456789abcdef0123456789abcdef01234567", m
assert m["context"]["name"] == "ci/woodpecker/pr/pr", m
assert m["context"]["class"] == "untrusted", m
assert m["context"]["registry"] == "immutable", m
# values OBSERVED from the API, not caller literals
assert m["event"] == "pull_request", m
assert m["workflow"] == "pr", m
assert m["workflow_state"] == "success", m
assert m["pipeline_id"] == "111", m
assert m["pipeline_number"] == "11", m
assert m["trusted_class"] == {"expected": "untrusted", "observed_volumes": "false"}, m
assert m["observed"]["pipeline_id"] == "111" and m["observed"]["event"] == "pull_request", m
assert m["release_certificate"] is False, m
assert m["attestation"]["status"] == "absent", m
assert m["evidence_digest"].startswith("sha256:") and len(m["evidence_digest"]) == 71, m
assert m["run_url"].endswith("/repos/7/pipeline/11"), m
assert len(m["artifacts"]) == 3, m
assert m["release_artifacts"]["status"] == "complete", m
assert m["release_artifacts"]["required"] == 3 and m["release_artifacts"]["matched"] == 3, m
assert m["release_artifacts"]["attested"] == 0, m
assert m["fault_campaign"]["status"] == "pass" and m["fault_campaign"]["executed"] > 0, m
assert m["certificate_class"] == "source", m
'
    first_digest="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["evidence_digest"])' "$tmp/manifest-success.json")"
    expect success-again 0
    second_digest="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["evidence_digest"])' "$tmp/manifest-success-again.json")"
    if [ "$first_digest" = "$second_digest" ]; then
        echo "selftest ok: evidence digest is deterministic"
    else
        echo "selftest FAIL: evidence digest not deterministic" >&2
        failures=$((failures + 1))
    fi

    # --- context registry rejection (P1-I).
    (
        CONTEXT="ci/woodpecker/pr/nope"
        verify_woodpecker_context "$sha" "$tree" "$tmp/manifest-bad-context.json" >/dev/null 2>&1
    )
    code=$?
    if [ "$code" -eq 2 ]; then
        echo "selftest ok: unregistered context -> operator error (exit 2)"
    else
        echo "selftest FAIL: unregistered context -> exit $code (want 2)" >&2
        failures=$((failures + 1))
    fi

    # --- absent: no pipeline for the SHA.
    CERTIFY_TEST_PIPELINES='[]' \
        CERTIFY_TEST_DETAIL="$(detail_json 11 111 success "$sha" pull_request 'pr=success')" \
        set_state
    expect absent 1
    # --- other SHA: pipeline exists but for a different commit.
    CERTIFY_TEST_PIPELINES="[$(list_json 12 112 success 'ffffffffffffffffffffffffffffffffffffffff' pull_request)]" \
        CERTIFY_TEST_DETAIL="$(detail_json 12 112 success 'ffffffffffffffffffffffffffffffffffffffff' pull_request 'pr=success')" \
        set_state
    expect other-sha 1
    # --- pending.
    CERTIFY_TEST_PIPELINES="[$(list_json 11 111 running "$sha" pull_request)]" \
        CERTIFY_TEST_DETAIL="$(detail_json 11 111 running "$sha" pull_request 'pr=running,linux=running')" \
        set_state
    expect pending 1
    # --- failure.
    CERTIFY_TEST_PIPELINES="[$(list_json 11 111 failure "$sha" pull_request)]" \
        CERTIFY_TEST_DETAIL="$(detail_json 11 111 failure "$sha" pull_request 'pr=failure,linux=success')" \
        set_state
    expect failure 1
    # --- error.
    CERTIFY_TEST_PIPELINES="[$(list_json 11 111 error "$sha" pull_request)]" \
        CERTIFY_TEST_DETAIL="$(detail_json 11 111 error "$sha" pull_request 'pr=error,linux=success')" \
        set_state
    expect error 1
    # --- wrong workflow name: the context is absent even though the pipeline passed.
    CERTIFY_TEST_PIPELINES="[$(list_json 11 111 success "$sha" pull_request)]" \
        CERTIFY_TEST_DETAIL="$(detail_json 11 111 success "$sha" pull_request 'trusted=success')" \
        set_state
    expect wrong-workflow 1
    expect_problem wrong-workflow 'context-absent'
    # --- observed event mismatch: caller selected pr, API says push.
    PIPELINE=11
    CERTIFY_TEST_PIPELINES="[$(list_json 11 111 success "$sha" pull_request)]" \
        CERTIFY_TEST_DETAIL="$(detail_json 11 111 success "$sha" push 'pr=success')" \
        set_state
    expect event-mismatch 1
    expect_problem event-mismatch 'observed-event-mismatch'
    PIPELINE=""
    # --- pipeline id mismatch between list and detail.
    CERTIFY_TEST_PIPELINES="[$(list_json 11 111 success "$sha" pull_request)]" \
        CERTIFY_TEST_DETAIL="$(detail_json 11 999 success "$sha" pull_request 'pr=success')" \
        set_state
    expect id-mismatch 1
    expect_problem id-mismatch 'pipeline-id-mismatch'
    # --- trusted class mismatch: pr context against a trusted project.
    CERTIFY_TEST_PIPELINES="[$(list_json 11 111 success "$sha" pull_request)]" \
        CERTIFY_TEST_DETAIL="$(detail_json 11 111 success "$sha" pull_request 'pr=success')" \
        CERTIFY_TEST_REPO7='{"id":7,"full_name":"acme/widgets","config_file":".woodpecker/untrusted/","trusted":{"volumes":true}}' \
        set_state
    expect class-mismatch 1
    expect_problem class-mismatch 'trusted-class-mismatch'
    # --- config file mismatch.
    CERTIFY_TEST_PIPELINES="[$(list_json 11 111 success "$sha" pull_request)]" \
        CERTIFY_TEST_DETAIL="$(detail_json 11 111 success "$sha" pull_request 'pr=success')" \
        CERTIFY_TEST_REPO7='{"id":7,"full_name":"acme/widgets","config_file":".woodpecker/trusted/","trusted":{"volumes":false}}' \
        set_state
    expect config-mismatch 1
    expect_problem config-mismatch 'config-file-mismatch'
    # --- repository mismatch.
    CERTIFY_TEST_PIPELINES="[$(list_json 11 111 success "$sha" pull_request)]" \
        CERTIFY_TEST_DETAIL="$(detail_json 11 111 success "$sha" pull_request 'pr=success')" \
        CERTIFY_TEST_REPO7='{"id":7,"full_name":"acme/other","config_file":".woodpecker/untrusted/","trusted":{"volumes":false}}' \
        set_state
    expect repo-mismatch 1
    expect_problem repo-mismatch 'repository-mismatch'

    # --- trusted context success with a signed attestation (P1-I + P1-J).
    printf 'selftest artifact\n' >"$tmp/artifact.bin"
    LOCAL_STATUS="pass" # simulate a full local gate pass for release-class facts
    make_attestation "$tmp/trusted-att.json" --sign-key "$tmp/sign.pem" --key-id faktor-selftest >/dev/null
    log_block "$tmp/trusted-att.json" >"$tmp/trusted-block.txt"
    CERTIFY_TEST_PIPELINES="[$(list_json 21 211 success "$sha" push)]" \
        CERTIFY_TEST_DETAIL="$(detail_json 21 211 success "$sha" push 'trusted=success' 701)" \
        CERTIFY_TEST_LOGS="$(logs_json "$tmp/trusted-block.txt" 701)" \
        set_state
    CONTEXT="ci/woodpecker/push/trusted"
    expect trusted-success 0
    expect_manifest trusted-success '
import json, sys
m = json.load(open(sys.argv[1]))
assert m["status"] == "passed", m
assert m["context"]["class"] == "trusted", m
assert m["trusted_class"] == {"expected": "trusted", "observed_volumes": "true"}, m
assert m["event"] == "push" and m["workflow"] == "trusted", m
assert m["observed"]["pipeline_id"] == "211", m
assert m["attestation"]["status"] == "verified", m
assert m["attestation"]["pipeline_number"] == "21", m
assert m["attestation"]["signature_identity"] == "faktor-selftest", m
assert m["attestation"]["evidence_digest"].startswith("sha256:"), m
assert m["release_artifacts"]["status"] == "complete", m
assert m["release_artifacts"]["required"] == 3 and m["release_artifacts"]["attested"] == 3, m
assert m["release_artifacts"]["branding"] == "pass", m
assert m["fault_campaign"]["status"] == "pass" and m["fault_campaign"]["executed"] > 0, m
assert m["certificate_class"] == "release", m
assert m["release_certificate"] is True, m
'
    # --- attestation tamper: the local artifact no longer matches the attested digest.
    printf 'tampered artifact\n' >>"${ART_FULL[0]}"
    expect trusted-tampered 1
    expect_problem trusted-tampered 'attestation-invalid'
    cp "$release_fixtures/full/bin/faktor-cli" "${ART_FULL[0]}"
    # --- attestation signed by a foreign identity.
    make_attestation "$tmp/foreign-att.json" --sign-key "$tmp/foreign.pem" --key-id foreign >/dev/null
    log_block "$tmp/foreign-att.json" >"$tmp/foreign-block.txt"
    CERTIFY_TEST_PIPELINES="[$(list_json 21 211 success "$sha" push)]" \
        CERTIFY_TEST_DETAIL="$(detail_json 21 211 success "$sha" push 'trusted=success' 701)" \
        CERTIFY_TEST_LOGS="$(logs_json "$tmp/foreign-block.txt" 701)" \
        set_state
    expect foreign-signature 1
    expect_problem foreign-signature 'attestation-invalid'
    # --- P0 key substitution (pre-fix bypass): allowlisted identity name, the
    # attacker's keypair. The unfixed verifier accepted signature.public_key
    # from the attestation, so this object verified; it must now fail closed.
    make_attestation "$tmp/subst-att.json" --sign-key "$tmp/foreign.pem" --key-id faktor-selftest >/dev/null
    log_block "$tmp/subst-att.json" >"$tmp/subst-block.txt"
    CERTIFY_TEST_PIPELINES="[$(list_json 21 211 success "$sha" push)]" \
        CERTIFY_TEST_DETAIL="$(detail_json 21 211 success "$sha" push 'trusted=success' 701)" \
        CERTIFY_TEST_LOGS="$(logs_json "$tmp/subst-block.txt" 701)" \
        set_state
    expect key-substitution 1
    expect_problem key-substitution 'attestation-invalid'
    # --- P1 repository binding: attestation claims another repository.
    ATT_REPO=acme/other make_attestation "$tmp/repo-spoof.json" --sign-key "$tmp/sign.pem" --key-id faktor-selftest >/dev/null
    log_block "$tmp/repo-spoof.json" >"$tmp/repo-spoof-block.txt"
    CERTIFY_TEST_PIPELINES="[$(list_json 21 211 success "$sha" push)]" \
        CERTIFY_TEST_DETAIL="$(detail_json 21 211 success "$sha" push 'trusted=success' 701)" \
        CERTIFY_TEST_LOGS="$(logs_json "$tmp/repo-spoof-block.txt" 701)" \
        set_state
    expect repo-spoof 1
    expect_problem repo-spoof 'attestation-invalid'
    # --- P1 unconditional pipeline id: the number copied into the id field.
    ATT_PIPELINE_ID=21 make_attestation "$tmp/id-spoof.json" --sign-key "$tmp/sign.pem" --key-id faktor-selftest >/dev/null
    log_block "$tmp/id-spoof.json" >"$tmp/id-spoof-block.txt"
    CERTIFY_TEST_PIPELINES="[$(list_json 21 211 success "$sha" push)]" \
        CERTIFY_TEST_DETAIL="$(detail_json 21 211 success "$sha" push 'trusted=success' 701)" \
        CERTIFY_TEST_LOGS="$(logs_json "$tmp/id-spoof-block.txt" 701)" \
        set_state
    expect pipeline-id-spoof 1
    expect_problem pipeline-id-spoof 'attestation-invalid'
    # --- unsigned attestation is not release-grade. The trusted workflow's
    # `create` is fail-closed (no unsigned code path), so this fixture is a
    # legacy/foreign object: a valid signed attestation with the signature
    # stripped, exactly what the verifier must refuse.
    python3 - "$tmp/trusted-att.json" "$tmp/unsigned-att.json" <<'PY'
import json
import sys

with open(sys.argv[1]) as fh:
    attestation = json.load(fh)
attestation["signature"] = None
with open(sys.argv[2], "w") as fh:
    json.dump(attestation, fh)
PY
    log_block "$tmp/unsigned-att.json" >"$tmp/unsigned-block.txt"
    CERTIFY_TEST_PIPELINES="[$(list_json 21 211 success "$sha" push)]" \
        CERTIFY_TEST_DETAIL="$(detail_json 21 211 success "$sha" push 'trusted=success' 701)" \
        CERTIFY_TEST_LOGS="$(logs_json "$tmp/unsigned-block.txt" 701)" \
        set_state
    expect unsigned-attestation 1
    expect_problem unsigned-attestation 'attestation-invalid'
    # --- trusted context without an attestation step must fail (P1-J).
    CERTIFY_TEST_PIPELINES="[$(list_json 21 211 success "$sha" push)]" \
        CERTIFY_TEST_DETAIL="$(detail_json 21 211 success "$sha" push 'trusted=success')" \
        set_state
    expect trusted-absent 1
    expect_problem trusted-absent 'attestation-absent'

    # --- pr context upgraded by a trusted attestation at the same SHA.
    make_attestation "$tmp/upgrade-att.json" --sign-key "$tmp/sign.pem" --key-id faktor-selftest >/dev/null
    log_block "$tmp/upgrade-att.json" >"$tmp/upgrade-block.txt"
    CERTIFY_TEST_PIPELINES="[$(list_json 11 111 success "$sha" pull_request),$(list_json 21 211 success "$sha" push)]" \
        CERTIFY_TEST_DETAILS="$(details_json 11 "$(detail_json 11 111 success "$sha" pull_request 'pr=success,linux=success')" 21 "$(detail_json 21 211 success "$sha" push 'trusted=success' 701)")" \
        CERTIFY_TEST_LOGS="$(logs_json "$tmp/upgrade-block.txt" 701)" \
        set_state
    CONTEXT="ci/woodpecker/pr/pr"
    ATT_KEYS="$tmp/keys.json"
    expect pr-upgraded 0
    expect_manifest pr-upgraded '
import json, sys
m = json.load(open(sys.argv[1]))
assert m["context"]["class"] == "untrusted", m
assert m["attestation"]["status"] == "verified", m
assert m["attestation"]["origin_context"] == "ci/woodpecker/push/trusted", m
assert m["release_artifacts"]["attested"] == 3, m
assert m["certificate_class"] == "release", m
assert m["release_certificate"] is True, m
'
    # --- P1: the trusted project id (WOODPECKER_TRUSTED_REPO_ID or a
    # ?project=trusted lookup answer) is a claim, not authority: the project
    # record is re-fetched and its observed identity/config/trust facts must
    # match. Pre-fix, a lying record still upgraded the run to release class.
    CERTIFY_TEST_REPO8='{"id":8,"full_name":"acme/other","config_file":".woodpecker/trusted/","trusted":{"volumes":true}}' \
        CERTIFY_TEST_PIPELINES="[$(list_json 11 111 success "$sha" pull_request),$(list_json 21 211 success "$sha" push)]" \
        CERTIFY_TEST_DETAILS="$(details_json 11 "$(detail_json 11 111 success "$sha" pull_request 'pr=success,linux=success')" 21 "$(detail_json 21 211 success "$sha" push 'trusted=success' 701)")" \
        CERTIFY_TEST_LOGS="$(logs_json "$tmp/upgrade-block.txt" 701)" \
        set_state
    expect trusted-repo-spoof 1
    expect_problem trusted-repo-spoof 'attestation-trusted-repo-mismatch'
    CERTIFY_TEST_REPO8='{"id":8,"full_name":"acme/widgets","config_file":".woodpecker/untrusted/","trusted":{"volumes":true}}' \
        CERTIFY_TEST_PIPELINES="[$(list_json 11 111 success "$sha" pull_request),$(list_json 21 211 success "$sha" push)]" \
        CERTIFY_TEST_DETAILS="$(details_json 11 "$(detail_json 11 111 success "$sha" pull_request 'pr=success,linux=success')" 21 "$(detail_json 21 211 success "$sha" push 'trusted=success' 701)")" \
        CERTIFY_TEST_LOGS="$(logs_json "$tmp/upgrade-block.txt" 701)" \
        set_state
    expect trusted-config-spoof 1
    expect_problem trusted-config-spoof 'attestation-trusted-config-mismatch'
    CERTIFY_TEST_REPO8='{"id":8,"full_name":"acme/widgets","config_file":".woodpecker/trusted/","trusted":{"volumes":false}}' \
        CERTIFY_TEST_PIPELINES="[$(list_json 11 111 success "$sha" pull_request),$(list_json 21 211 success "$sha" push)]" \
        CERTIFY_TEST_DETAILS="$(details_json 11 "$(detail_json 11 111 success "$sha" pull_request 'pr=success,linux=success')" 21 "$(detail_json 21 211 success "$sha" push 'trusted=success' 701)")" \
        CERTIFY_TEST_LOGS="$(logs_json "$tmp/upgrade-block.txt" 701)" \
        set_state
    expect trusted-class-spoof 1
    expect_problem trusted-class-spoof 'attestation-trusted-class-mismatch'
    # --- without an allowlist the attestation is not consulted; no release class.
    ATT_KEYS=""
    expect pr-no-keys 0
    expect_manifest pr-no-keys '
import json, sys
m = json.load(open(sys.argv[1]))
assert m["attestation"]["status"] == "not-checked", m
assert m["release_certificate"] is False, m
assert any("attestation-not-checked" in n for n in m["notes"]), m
'
    ATT_KEYS="$tmp/keys.json"

    # --- operator error: credentials missing.
    (
        unset WOODPECKER_HOST WOODPECKER_TOKEN
        verify_woodpecker_context "$sha" "$tree" "$tmp/manifest-no-creds.json" >/dev/null 2>&1
    )
    code=$?
    if [ "$code" -eq 2 ]; then
        echo "selftest ok: missing credentials -> operator error (exit 2)"
    else
        echo "selftest FAIL: missing credentials -> exit $code (want 2)" >&2
        failures=$((failures + 1))
    fi

    # --- certificate class rule (P1-K + P1 completeness): only local pass
    # AND (trusted or signed attestation) AND every release precondition.
    release_facts_on() {
        RA_STATUS="complete"
        RA_REQUIRED=3
        RA_MATCHED=3
        RA_ATTESTED=3
        RA_BRANDING="pass"
        FAULT_STATUS="pass"
        FAULT_COUNT=3
    }
    release_facts_off() {
        RA_STATUS="empty"
        RA_REQUIRED=3
        RA_MATCHED=0
        RA_ATTESTED=0
        RA_BRANDING="not-applicable"
        FAULT_STATUS="crate-absent"
        FAULT_COUNT=0
    }
    check_class() { # local ci class att want
        local got
        got="$(certificate_class "$1" "$2" "$3" "$4")"
        if [ "$got" = "$5" ]; then
            echo "selftest ok: certificate_class($1,$2,$3,$4) = $got"
        else
            echo "selftest FAIL: certificate_class($1,$2,$3,$4) = $got (want $5)" >&2
            failures=$((failures + 1))
        fi
    }
    release_facts_on
    check_class 1 0 untrusted verified release
    check_class 1 0 trusted absent release
    release_facts_off
    check_class 1 0 untrusted verified source
    check_class 1 0 trusted absent source
    release_facts_on
    check_class 0 0 trusted verified none
    check_class 1 1 trusted verified none

    # --- CLI wording (P1-K): --verify-ci-evidence never certifies a release.
    real_sha="$(git rev-parse HEAD)"
    CERTIFY_TEST_PIPELINES="[$(list_json 31 311 success "$real_sha" pull_request)]" \
        CERTIFY_TEST_DETAIL="$(detail_json 31 311 success "$real_sha" pull_request 'pr=success')" \
        set_state
    cli_out="$(
        FAKTOR_ATTEST_KEYS='' WOODPECKER_HOST="http://127.0.0.1:$port" WOODPECKER_TOKEN=selftest-token \
            bash "$ROOT/scripts/certify.sh" --verify-ci-evidence --repo acme/widgets \
            --context ci/woodpecker/pr/pr --commit "$real_sha" \
            --manifest-out "$tmp/cli-ci-manifest.json" 2>&1
    )"
    code=$?
    if [ "$code" -eq 0 ] && printf '%s\n' "$cli_out" | grep -qF 'CI EVIDENCE: PASS — NOT A RELEASE CERTIFICATE'; then
        echo "selftest ok: --verify-ci-evidence terminates with the not-a-release-certificate wording"
    else
        echo "selftest FAIL: --verify-ci-evidence wording/exit ($code)" >&2
        printf '%s\n' "$cli_out" | tail -n 5 >&2
        failures=$((failures + 1))
    fi
    if python3 -c '
import json, sys
m = json.load(open(sys.argv[1]))
assert m["release_certificate"] is False, m
assert m["local_gates"] == "skipped", m
' "$tmp/cli-ci-manifest.json" 2>/dev/null; then
        echo "selftest ok: --verify-ci-evidence manifest is not a release certificate"
    else
        echo "selftest FAIL: --verify-ci-evidence manifest" >&2
        failures=$((failures + 1))
    fi
    # pending state: CI EVIDENCE: FAIL
    CERTIFY_TEST_PIPELINES="[$(list_json 32 312 running "$real_sha" pull_request)]" \
        CERTIFY_TEST_DETAIL="$(detail_json 32 312 running "$real_sha" pull_request 'pr=running')" \
        set_state
    cli_out="$(
        FAKTOR_ATTEST_KEYS='' WOODPECKER_HOST="http://127.0.0.1:$port" WOODPECKER_TOKEN=selftest-token \
            bash "$ROOT/scripts/certify.sh" --verify-ci-evidence --repo acme/widgets \
            --context ci/woodpecker/pr/pr --commit "$real_sha" \
            --manifest-out "$tmp/cli-ci-fail-manifest.json" 2>&1
    )"
    code=$?
    if [ "$code" -eq 1 ] && printf '%s\n' "$cli_out" | grep -qF 'CI EVIDENCE: FAIL'; then
        echo "selftest ok: --verify-ci-evidence failure wording (CI EVIDENCE: FAIL)"
    else
        echo "selftest FAIL: --verify-ci-evidence failure wording/exit ($code)" >&2
        failures=$((failures + 1))
    fi
    # the renamed legacy flag is rejected, never silently accepted.
    if bash "$ROOT/scripts/certify.sh" --ci-only >/dev/null 2>&1; then
        echo "selftest FAIL: --ci-only was accepted" >&2
        failures=$((failures + 1))
    else
        code=$?
        if [ "$code" -eq 2 ]; then
            echo "selftest ok: --ci-only is rejected (renamed to --verify-ci-evidence)"
        else
            echo "selftest FAIL: --ci-only -> exit $code (want 2)" >&2
            failures=$((failures + 1))
        fi
    fi

    # --- P3 temp-name migration: no kp-* minting; legacy dirs still swept.
    local script
    for script in scripts/certify.sh scripts/cross-target-check.sh scripts/check-gradle-integrity.sh; do
        if grep -qE 'mktemp -d "[^"]*kp-' "$ROOT/$script"; then
            echo "selftest FAIL: $script still mints a kp-* temp name" >&2
            failures=$((failures + 1))
        else
            echo "selftest ok: $script mints no kp-* temp name"
        fi
    done
    for needle in 'faktor-cert.XXXXXX' 'faktor-cert-ci.XXXXXX' 'faktor-cert-selftest.XXXXXX'; do
        if grep -qF "$needle" "$ROOT/scripts/certify.sh"; then
            echo "selftest ok: certify.sh mints $needle"
        else
            echo "selftest FAIL: certify.sh does not mint $needle" >&2
            failures=$((failures + 1))
        fi
    done
    legacy_root="$tmp/legacy-tmp"
    mkdir -p "$legacy_root/kp-cert.dead" "$legacy_root/kp-cert-ci.dead" "$legacy_root/kp-cert-selftest.dead" "$legacy_root/faktor-cert.keep" "$legacy_root/unrelated.keep"
    touch -t 202001010000 "$legacy_root/kp-cert.dead" "$legacy_root/kp-cert-ci.dead" "$legacy_root/kp-cert-selftest.dead" "$legacy_root/faktor-cert.keep" "$legacy_root/unrelated.keep"
    (
        TMPDIR="$legacy_root"
        sweep_legacy_tmpdirs
    )
    if [ ! -e "$legacy_root/kp-cert.dead" ] && [ ! -e "$legacy_root/kp-cert-ci.dead" ] && [ ! -e "$legacy_root/kp-cert-selftest.dead" ]; then
        echo "selftest ok: stale kp-* legacy temp dirs are swept"
    else
        echo "selftest FAIL: legacy kp-* temp dirs survived the sweep" >&2
        failures=$((failures + 1))
    fi
    if [ -d "$legacy_root/faktor-cert.keep" ] && [ -d "$legacy_root/unrelated.keep" ]; then
        echo "selftest ok: the sweep touches only legacy kp-* prefixes"
    else
        echo "selftest FAIL: the sweep removed non-legacy dirs" >&2
        failures=$((failures + 1))
    fi

    # --- P1 ReleaseArtifactSet matrix (fixtures): empty and subset sets can
    # never reach the release class; the full set with branding + fault passes.
    check_release_set() { # name dir want_status want_matched
        local name="$1" dir="$2" want_status="$3" want_matched="$4"
        local tsv="$tmp/ras-$name.tsv" p
        : >"$tsv"
        while IFS= read -r p; do
            [ -n "$p" ] || continue
            printf 'sha256:%s\t%s\n' "$(hash_file "$p")" "$p" >>"$tsv"
        done < <(find "$dir" -type f 2>/dev/null | sort)
        evaluate_release_artifact_set "$tsv"
        if [ "$RA_STATUS" = "$want_status" ] && [ "$RA_MATCHED" -eq "$want_matched" ]; then
            echo "selftest ok: ReleaseArtifactSet $name -> status=$RA_STATUS matched=$RA_MATCHED/$RA_REQUIRED"
        else
            echo "selftest FAIL: ReleaseArtifactSet $name -> status=$RA_STATUS matched=$RA_MATCHED/$RA_REQUIRED (want $want_status matched=$want_matched)" >&2
            failures=$((failures + 1))
        fi
    }
    check_release_set full "$release_fixtures/full" complete 3
    check_release_set subset "$release_fixtures/subset" partial 2
    check_release_set empty "$release_fixtures/empty" empty 0

    release_facts_on
    RA_STATUS="empty"
    RA_MATCHED=0
    RA_BRANDING="not-applicable"
    if release_preconditions_met; then
        echo "selftest FAIL: empty ReleaseArtifactSet satisfied the release preconditions" >&2
        failures=$((failures + 1))
    else
        echo "selftest ok: empty ReleaseArtifactSet fails the release class (no release certificate possible)"
    fi
    RA_STATUS="partial"
    RA_MATCHED=2
    RA_BRANDING="not-applicable"
    if release_preconditions_met; then
        echo "selftest FAIL: subset ReleaseArtifactSet satisfied the release preconditions" >&2
        failures=$((failures + 1))
    else
        echo "selftest ok: subset ReleaseArtifactSet fails the release class"
    fi
    evaluate_release_artifact_set "$tmp/ras-full.tsv"
    release_branding_scan >/dev/null
    release_facts_on
    if release_preconditions_met; then
        echo "selftest ok: complete ReleaseArtifactSet + branding pass + fault pass satisfies the release class"
    else
        echo "selftest FAIL: complete ReleaseArtifactSet did not satisfy the release preconditions (RA_STATUS=$RA_STATUS RA_BRANDING=$RA_BRANDING FAULT=$FAULT_STATUS/$FAULT_COUNT)" >&2
        failures=$((failures + 1))
    fi

    # --- P1 fault campaign gate matrix (hermetic fake cargo).
    local stub_bin saved_path saved_release saved_fault_record saved_fault_status saved_fault_count
    stub_bin="$tmp/fakebin"
    mkdir -p "$stub_bin"
    cat >"$stub_bin/cargo" <<'STUB'
#!/usr/bin/env bash
case "$*" in
  *metadata*) printf '%s\n' "${FAKE_METADATA:-[]}" ;;
  *--list*) printf '%s\n' "${FAKE_LIST:-}" ;;
  *) printf '%s\n' "${FAKE_RUN:-}"; exit "${FAKE_RC:-0}" ;;
esac
STUB
    chmod +x "$stub_bin/cargo"
    export FAKE_METADATA FAKE_LIST FAKE_RUN FAKE_RC
    saved_path="$PATH"
    saved_release="$RELEASE_REQUIRED"
    saved_fault_record="$FAULT_RECORD"
    saved_fault_status="$FAULT_STATUS"
    saved_fault_count="$FAULT_COUNT"
    FAULT_RECORD="$tmp/fault-matrix.txt"
    PATH="$stub_bin:$PATH"
    RELEASE_REQUIRED=0
    FAKE_METADATA='[]'
    FAULT_STATUS="not-run"
    fault_campaign >/dev/null 2>&1 || true
    if [ "$FAULT_STATUS" = "crate-absent" ] && ! release_preconditions_met; then
        echo "selftest ok: absent faktor-tests-fault -> crate-absent, release class impossible"
    else
        echo "selftest FAIL: absent fault crate handling (status=$FAULT_STATUS)" >&2
        failures=$((failures + 1))
    fi
    RELEASE_REQUIRED=1
    FAULT_STATUS="not-run"
    if fault_campaign >/dev/null 2>&1; then
        echo "selftest FAIL: --release accepted an absent fault crate" >&2
        failures=$((failures + 1))
    else
        echo "selftest ok: --release fails when faktor-tests-fault is absent"
    fi
    RELEASE_REQUIRED=0
    FAKE_METADATA='[{"name":"faktor-tests-fault"}]'
    FAKE_LIST=''
    FAULT_STATUS="not-run"
    fault_campaign >/dev/null 2>&1 || true
    if [ "$FAULT_STATUS" = "zero" ]; then
        echo "selftest ok: zero executed [fault] tests -> status=zero, release class impossible"
    else
        echo "selftest FAIL: zero-test fault handling (status=$FAULT_STATUS)" >&2
        failures=$((failures + 1))
    fi
    RELEASE_REQUIRED=1
    FAULT_STATUS="not-run"
    if fault_campaign >/dev/null 2>&1; then
        echo "selftest FAIL: --release accepted zero executed [fault] tests" >&2
        failures=$((failures + 1))
    else
        echo "selftest ok: --release fails when zero [fault] tests execute"
    fi
    RELEASE_REQUIRED=1
    FAKE_LIST="$(printf 'campaigns::a: test\ncampaigns::b: test\ncampaigns::c: test\n')"
    FAKE_RUN="$(printf 'test campaigns::a ... ok\ntest campaigns::b ... ok\ntest campaigns::c ... ok\n')"
    if fault_campaign >/dev/null 2>&1 && [ "$FAULT_STATUS" = "pass" ] && [ "$FAULT_COUNT" -eq 3 ] \
        && [ -n "$FAULT_DIGEST" ] && fault_release_gate; then
        echo "selftest ok: fault campaign pass records executed=3 digest=$FAULT_DIGEST"
    else
        echo "selftest FAIL: fault campaign pass recording (status=$FAULT_STATUS count=$FAULT_COUNT digest=$FAULT_DIGEST)" >&2
        failures=$((failures + 1))
    fi
    PATH="$saved_path"
    RELEASE_REQUIRED="$saved_release"
    FAULT_RECORD="$saved_fault_record"
    FAULT_STATUS="$saved_fault_status"
    FAULT_COUNT="$saved_fault_count"
    release_facts_on

    # --- P1 gate parity: fixture divergence must fail; the real workflow
    # linux lanes must match the canonical list exactly.
    local parity_fixtures
    parity_fixtures="$ROOT/scripts/certification/fixtures/gate-parity"
    check_parity() { # name want canonical ci...
        local name="$1" want="$2"
        shift 2
        local rc=0
        check_gate_parity "$@" >/dev/null 2>&1 || rc=$?
        if [ "$rc" -eq "$want" ]; then
            echo "selftest ok: gate parity $name -> exit $rc"
        else
            echo "selftest FAIL: gate parity $name -> exit $rc (want $want)" >&2
            failures=$((failures + 1))
        fi
    }
    check_parity matching 0 "$parity_fixtures/canonical.txt" "$parity_fixtures/ci-match.yaml"
    check_parity ci-diverge 1 "$parity_fixtures/canonical.txt" "$parity_fixtures/ci-diverge.yaml"
    check_parity canonical-diverge 1 "$parity_fixtures/canonical-diverge.txt" "$parity_fixtures/ci-match.yaml"
    canonical_gate_commands >"$tmp/canonical-gates.txt"
    check_parity real-workflows 0 "$tmp/canonical-gates.txt" "$ROOT/.woodpecker/trusted/trusted.yaml" "$ROOT/.woodpecker/untrusted/pr.yaml"

    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    rm -rf "$tmp"

    if [ "$failures" -eq 0 ]; then
        echo "certify selftest: PASS (context registry + observed-value matrix, attestation fetch/verification matrix, ReleaseArtifactSet matrix, gate parity, fault-campaign gate, certificate classes, temp-name migration)"
    else
        echo "certify selftest: FAIL ($failures case(s))" >&2
        return 1
    fi
    return 0
}

status=0
commit_sha=""
tree_sha=""
ci_status=0
LOCAL_STATUS="not-run"
local_ran=0
local_pass=0
ci_ran=0
release_class="none"

if [ "$SELFTEST" -eq 1 ]; then
    step "hermetic CI-verification selftest (mock Woodpecker API)"
    if run_ci_selftest; then exit 0; else exit 1; fi
fi

if [ "$VERIFY_CI" -eq 0 ]; then
    local_ran=1

    step "gates 1-4/9: canonical local cargo gates (single source: canonical_gate_commands)"
    if run_canonical_gates; then
        ok "canonical cargo gates pass"
    else
        status=1
        bad "canonical cargo gates failed"
    fi

    step "gate 5/9: gate parity (canonical list vs CI linux lanes)"
    parity_file="$(mktemp "${TMPDIR:-/tmp}/faktor-cert-gates.XXXXXX")"
    canonical_gate_commands >"$parity_file"
    if [ -f .woodpecker/trusted/trusted.yaml ] && [ -f .woodpecker/untrusted/pr.yaml ]; then
        if check_gate_parity "$parity_file" .woodpecker/trusted/trusted.yaml .woodpecker/untrusted/pr.yaml; then
            ok "gate parity clean: local gates are the CI linux-lane gates"
        else
            status=1
            bad "gate parity failed: local and CI gate lists diverged"
        fi
    else
        ok "gate parity not applicable: CI workflow files absent from this checkout"
    fi
    rm -f "$parity_file"

    step "gate 6/9: doctor --deep on a fresh data dir"
    if run_doctor_deep "pre-campaign doctor --deep"; then ok "pre-campaign doctor --deep clean"; else status=1; fi

    step "gate 7/9: fault campaign ([fault] ignored tests; count+digest recorded)"
    if fault_campaign; then ok "fault campaign gate complete (status=$FAULT_STATUS executed=$FAULT_COUNT)"; else status=1; fi

    step "gate 8/9: doctor --deep after the fault campaign (release certification)"
    if run_doctor_deep "post-campaign doctor --deep"; then ok "post-campaign doctor --deep clean"; else status=1; fi

    step "gate 9/9: ReleaseArtifactSet + required-artifact branding scan"
    if prepare_artifacts; then
        ok "ReleaseArtifactSet: status=$RA_STATUS matched=$RA_MATCHED/$RA_REQUIRED${RA_MISSING:+ missing=$RA_MISSING}"
        if release_branding_scan; then ok "release artifact branding gate complete"; else status=1; fi
    else
        status=2
        bad "artifact collection failed (operator action required)"
    fi

    if [ "$status" -eq 0 ]; then
        LOCAL_STATUS="pass"
        local_pass=1
    else
        LOCAL_STATUS="fail"
    fi
else
    step "verify-ci-evidence: local cargo gates skipped by --verify-ci-evidence"
    LOCAL_STATUS="skipped"
fi

if [ "$LOCAL_ONLY" -eq 1 ]; then
    step "gate 9/9: CI context verification skipped (--local-only)"
    bad "--local-only is a local pre-flight, NOT a release certificate: the required CI context was not verified"
else
    step "gate 9/9: Woodpecker context $CONTEXT at the EXACT commit"
    ci_ran=1
    if commit_sha="$(resolve_commit)"; then
        tree_sha="$(git rev-parse --verify "${commit_sha}^{tree}" 2>/dev/null)" || {
            commit_sha=""
            tree_sha=""
            ci_status=2
            echo "certify: cannot resolve the tree of the shipped commit" >&2
        }
    else
        commit_sha=""
        ci_status=2
        echo "certify: cannot resolve the exact shipped commit; pass --commit <40-hex-sha>" >&2
    fi
    if [ "$ci_status" -eq 0 ]; then
        verify_woodpecker_context "$commit_sha" "$tree_sha" "$MANIFEST_OUT"
        ci_status=$?
        if [ "$ci_status" -eq 0 ]; then
            ok "release manifest written: $MANIFEST_OUT"
        else
            bad "CI certification NOT verified (exit $ci_status)"
        fi
    fi
fi

if [ "$ci_ran" -eq 1 ] && [ "$ci_status" -eq 0 ]; then
    release_class="$(certificate_class "$local_pass" "$ci_status" "$CTX_CLASS" "$ATT_STATUS")"
fi

final="$status"
if [ "$ci_ran" -eq 1 ] && [ "$ci_status" -gt "$final" ]; then
    final="$ci_status"
fi
# --release makes every precondition mandatory: an unmet precondition is a
# FAIL, never a silent downgrade to the source class.
if [ "$RELEASE_REQUIRED" -eq 1 ] && [ "$final" -eq 0 ] && [ "$release_class" != "release" ]; then
    final=1
    bad "release class required (--release) but a release precondition is unmet"
fi

summary_gates=()
if [ "$VERIFY_CI" -eq 0 ] && [ "$local_ran" -eq 1 ]; then
    summary_gates=("${gates[@]}")
    summary_gates+=("ReleaseArtifactSet: status=$RA_STATUS matched=$RA_MATCHED/$RA_REQUIRED attested=$RA_ATTESTED/$RA_REQUIRED branding=$RA_BRANDING")
    summary_gates+=("fault campaign: $FAULT_STATUS executed=$FAULT_COUNT digest=${FAULT_DIGEST:-none}")
fi
if [ "$LOCAL_ONLY" -eq 0 ]; then
    summary_gates+=("Woodpecker context $CONTEXT at the exact shipped commit")
fi
if [ "$release_class" = "release" ]; then
    summary_gates+=("release-class evidence: trusted context or verified signed attestation")
fi

printf '\n=====================\n'
if [ "$VERIFY_CI" -eq 1 ]; then
    case "$final" in
    0)
        printf 'CI EVIDENCE: PASS — NOT A RELEASE CERTIFICATE\n'
        for g in "${summary_gates[@]}"; do printf '  [x] %s\n' "$g"; done
        printf '  commit %s, tree %s\n' "$commit_sha" "$tree_sha"
        printf 'The local cargo gates did not run; only a full local+trusted run emits a release certificate.\n'
        ;;
    2)
        printf 'CI EVIDENCE: INCOMPLETE (operator action required)\n'
        printf 'Certification evidence could not be verified — do not ship.\n'
        ;;
    *)
        printf 'CI EVIDENCE: FAIL\n'
        for g in "${summary_gates[@]}"; do printf '  [ ] %s\n' "$g"; done
        printf 'At least one CI evidence gate failed — do not ship.\n'
        ;;
    esac
elif [ "$LOCAL_ONLY" -eq 1 ]; then
    case "$final" in
    0)
        printf 'LOCAL PRE-FLIGHT: PASS — NOT A RELEASE CERTIFICATE\n'
        for g in "${summary_gates[@]}"; do printf '  [x] %s\n' "$g"; done
        ;;
    2)
        printf 'LOCAL PRE-FLIGHT: INCOMPLETE (operator action required)\n'
        ;;
    *)
        printf 'LOCAL PRE-FLIGHT: FAIL\n'
        for g in "${summary_gates[@]}"; do printf '  [ ] %s\n' "$g"; done
        ;;
    esac
elif [ "$final" -eq 2 ]; then
    printf 'CERTIFICATION: INCOMPLETE (operator action required)\n'
    for g in "${summary_gates[@]}"; do printf '  [ ] %s\n' "$g"; done
    printf 'Certification could not run to completion — do not ship.\n'
elif [ "$final" -eq 0 ] && [ "$release_class" = "release" ]; then
    printf 'RELEASE CERTIFICATE: PASS\n'
    for g in "${summary_gates[@]}"; do printf '  [x] %s\n' "$g"; done
    printf '  commit %s, tree %s\n' "$commit_sha" "$tree_sha"
    printf '  ReleaseArtifactSet: required=%s matched=%s attested=%s branding=%s\n' \
        "$RA_REQUIRED" "$RA_MATCHED" "$RA_ATTESTED" "$RA_BRANDING"
    printf '  fault campaign: executed=%s digest=%s\n' "$FAULT_COUNT" "${FAULT_DIGEST:-none}"
    printf '  fault test identities: %s\n' "$FAULT_RECORD"
    printf 'Exact-SHA CI evidence verified; manifest at %s.\n' "$MANIFEST_OUT"
elif [ "$RELEASE_REQUIRED" -eq 1 ]; then
    printf 'RELEASE CERTIFICATE: FAIL (release class required, preconditions unmet)\n'
    for g in "${summary_gates[@]}"; do printf '  [ ] %s\n' "$g"; done
    printf 'Unmet release preconditions:\n'
    release_denial_reasons | sed 's/^/  - /'
    printf 'Manifest at %s. Do not ship.\n' "$MANIFEST_OUT"
elif [ "$final" -eq 0 ]; then
    printf 'SOURCE CERTIFICATE: PASS — NOT A RELEASE CERTIFICATE\n'
    for g in "${summary_gates[@]}"; do printf '  [x] %s\n' "$g"; done
    printf '  commit %s, tree %s\n' "$commit_sha" "$tree_sha"
    printf 'Release-class preconditions not met:\n'
    release_denial_reasons | sed 's/^/  - /'
    printf 'Manifest at %s.\n' "$MANIFEST_OUT"
else
    printf 'CERTIFICATION: FAIL\n'
    for g in "${summary_gates[@]}"; do printf '  [ ] %s\n' "$g"; done
    printf 'At least one gate failed — do not ship.\n'
fi
if [ "$LOCAL_ONLY" -eq 1 ]; then
    printf 'NOTE: --local-only runs never certify a release (CI evidence is mandatory).\n'
fi
if [ "$VERIFY_CI" -eq 1 ]; then
    printf 'NOTE: --verify-ci-evidence verified the CI evidence only; the local cargo gates were not run.\n'
fi
if [ "$SELFTEST" -eq 1 ]; then
    printf 'NOTE: --selftest is a hermetic fixture run, never a release certificate.\n'
fi
printf '=====================\n'
[ -n "$ARTIFACT_TMP" ] && rm -rf "$ARTIFACT_TMP"
exit "$final"




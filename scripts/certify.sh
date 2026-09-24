#!/usr/bin/env bash
# Release certification (P0-97 / P0-74 / P0-100; P1-I/P1-J/P1-K hardened).
#
# The gate a release candidate must pass BEFORE shipping:
#   1. the full cargo gates exactly as CI runs them (fmt/check/clippy/tests);
#   2. `doctor --deep` on a FRESH data dir — every audit invariant (store,
#      CAS, journal, cost reservations, verification records, active-turn
#      recoverable owners, orphan children, process ownership) must pass and
#      print zero FAIL sections;
#   3. the fault campaign — the #[ignore]-gated `[fault]` tests, when the
#      faktor-tests-fault crate is present in this workspace (sibling wave);
#   4. `doctor --deep` again on a SECOND fresh data dir AFTER the campaign:
#      the corruption the campaign proves contained must not leak into a
#      fresh release image (P0-97 release certification);
#   5. a printed certificate summary.
#
# Gate 8 (additive, P0-70): the byte-level artifact branding scan
# (scripts/branding-scan.sh --artifacts) over PACKAGED application outputs
# (.vsix / plugin jars) when this workspace produced them. A bounded scan
# of packaged outputs only — target/release binary strings are deliberately
# not swept here (slow, and cargo test/debug outputs embed frozen fixture
# text); when no packaged output exists the gate is recorded as skipped,
# never silently dropped.
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
#     This script FETCHES the attestation belonging to the exact trusted
#     pipeline, verifies the ed25519 signature against the operator allowlist
#     (FAKTOR_ATTEST_KEYS / --attestation-keys), verifies source SHA, tree,
#     workflow, event, pipeline number/id, and re-hashes EVERY local/shipped
#     artifact against the attested digests. Any mismatch is a failure.
#     Distributing the CI-built artifacts (the attested bytes) rather than
#     locally rebuilt ones is the preferred release model.
#
#   * CERTIFICATE CLASS (P1-K): `--verify-ci-evidence` (the renamed
#     `--ci-only`) verifies the embedded CI run and terminates with
#     "CI EVIDENCE: PASS — NOT A RELEASE CERTIFICATE". A release certificate
#     is emitted ONLY when the full local gates pass AND (a trusted context
#     verifies OR the signed remote attestation covers the gates). No flag
#     weakens checks while preserving the certificate class.
#
# Usage:
#   bash scripts/certify.sh                       # local gates + required CI evidence
#   bash scripts/certify.sh --commit <sha>        # certify an exact shipped SHA
#   bash scripts/certify.sh --context <registry>  # select a registered context
#   bash scripts/certify.sh --local-only          # local gates only (NOT a release certificate)
#   bash scripts/certify.sh --verify-ci-evidence  # CI evidence only (NOT a release certificate)
#   bash scripts/certify.sh --selftest            # hermetic mock-API rejection matrix
#
# Env: WOODPECKER_HOST, WOODPECKER_TOKEN (required unless --local-only/
# --selftest); WOODPECKER_REPO / --repo owner/name;
# WOODPECKER_UNTRUSTED_REPO_ID / WOODPECKER_TRUSTED_REPO_ID (or
# WOODPECKER_REPO_ID); CERTIFY_CI_CONTEXT (default ci/woodpecker/pr/pr);
# FAKTOR_ATTEST_KEYS / --attestation-keys (ed25519 allowlist JSON; required
# to verify signed attestations); --artifact PATH (repeatable; packaged
# outputs auto-discovered when omitted).
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

# ---------------------------------------------------------------- arguments --
COMMIT=""
CONTEXT="${CERTIFY_CI_CONTEXT:-ci/woodpecker/pr/pr}"
MANIFEST_OUT="target/certification/release-manifest.json"
LOCAL_ONLY=0
VERIFY_CI=0
SELFTEST=0
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
if [ "$SELFTEST" -eq 0 ]; then
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

gates=("fmt --check" "cargo check --workspace" "clippy -D warnings" "cargo test --workspace" "doctor --deep (fresh dir)" "fault campaign [fault]" "doctor --deep (post-campaign)" "artifact branding scan (packaged outputs)")

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

fault_campaign() {
    # The [fault] ignored suite lives in the faktor-tests-fault crate of the
    # fault-containment sibling wave; until it lands in this workspace the
    # gate is skipped (recorded in the certificate, not silently dropped).
    if ! cargo metadata --no-deps --format-version 1 2>/dev/null \
        | grep -q '"name":"faktor-tests-fault"'; then
        ok "fault campaign skipped: no faktor-tests-fault crate in this workspace"
        return 0
    fi
    if ! cargo test -p faktor-tests-fault -- --ignored > /tmp/faktor-ci-fault.log 2>&1; then
        bad "fault campaign failed (log tail):"
        tail -n 50 /tmp/faktor-ci-fault.log
        return 1
    fi
    ok "fault campaign passed ([fault] ignored tests)"
    return 0
}

artifact_scan() {
    # Packaged application outputs only (.vsix, plugin jars) — scan the
    # app tree that produced one; skip gracefully (recorded, not silent)
    # when this workspace built none. source-mode content under those trees
    # is already scan-clean, so the byte-level pass is bounded.
    local pass=1 scanned=0 d found
    for d in apps/vscode apps/jetbrains; do
        [ -d "$d" ] || continue
        found="$(find "$d" -type f \( -name '*.vsix' -o -name '*.jar' \) -print -quit 2>/dev/null)"
        if [ -n "$found" ]; then
            scanned=1
            if bash scripts/branding-scan.sh --artifacts "$d"; then
                ok "artifact scan clean: $d"
            else
                bad "artifact scan failed: $d"
                pass=0
            fi
        fi
    done
    if [ "$scanned" -eq 0 ]; then
        ok "artifact scan skipped: no packaged .vsix/.jar outputs present in apps/"
        return 0
    fi
    return $((1 - pass))
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
            print(s.get("pid", s.get("id", "")))
            break
elif mode == "attestation_extract":
    text = ""
    if isinstance(data, list):
        text = "".join(
            e.get("data", "") for e in data
            if isinstance(e, dict) and isinstance(e.get("data"), str)
        )
    elif isinstance(data, dict):
        if isinstance(data.get("data"), str):
            text = data["data"]
        elif isinstance(data.get("lines"), list):
            text = "".join(x if isinstance(x, str) else "" for x in data["lines"])
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
        while IFS= read -r p; do
            [ -n "$p" ] || continue
            paths+=("$p")
        done < <(
            {
                ls apps/vscode/*.vsix 2>/dev/null
                find apps/jetbrains -type f -path '*/build/distributions/*.zip' 2>/dev/null
                [ -f target/release/faktor-cli ] && printf '%s\n' target/release/faktor-cli
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
    code="$(api_get "$api" "/repos/$repo_id/pipelines/$number/logs/$pid" "$log_file")"
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
    return 0
}

# Find a trusted pipeline at the exact SHA that published an attestation and
# verify it (used when the selected context itself is untrusted, so a signed
# attestation is what upgrades the run to release class). Returns 0 verified,
# 1 fetched-but-invalid, 3 absent.
find_trusted_attestation() { # api commit tree artifacts_tsv problems notes tmp
    local api="$1" commit="$2" tree="$3" artifacts_tsv="$4" problems="$5" notes="$6" tmp="$7"
    local tctx tspec tevent twf trepo_id tbody tcand tnumber tid tdetail tcommit tevent_obs code prc
    local number id status cand_commit cand_event
    for tctx in $(context_registry_list); do
        [ "$tctx" = "$CONTEXT" ] && continue
        tspec="$(context_registry_entry "$tctx")" || continue
        set -- $tspec
        tevent="$1"
        twf="$2"
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
        probe_attestation "$api" "$trepo_id" "$tnumber" "$tdetail" "$tevent" "$twf" "$commit" "$tree" "$artifacts_tsv" "$problems" "$tmp" "$tid"
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
release_certificate = (
    not problems
    and local_status == "pass"
    and (context["class"] == "trusted" or attestation["status"] == "verified")
)
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
    "local_gates": local_status,
    "release_certificate": release_certificate,
    "release_rule": (
        "release = full local gates pass AND (trusted context verified OR signed "
        "build attestation verified); no flag weakens this"
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
                [ "$OBS_PIPELINE_STATUS" = "success" ] ||
                    printf 'context-failure: pipeline status is %s while the %s workflow is success\n' "$OBS_PIPELINE_STATUS" "$CTX_WORKFLOW" >>"$problems_file"
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

    # 5. artifacts to bind into the manifest and the attestation.
    collect_artifacts "$artifacts_file" || {
        rm -rf "$tmp"
        return 2
    }

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

# P1-K: the only rule that yields a release certificate.
certificate_class() { # local_pass ci_status ctx_class att_status -> release|evidence|none
    local local_pass="$1" ci_status="$2" ctx_class="$3" att_status="$4"
    if [ "$ci_status" -ne 0 ]; then
        printf 'none'
        return
    fi
    if [ "$local_pass" -ne 1 ]; then
        printf 'evidence'
        return
    fi
    if [ "$ctx_class" = "trusted" ] || [ "$att_status" = "verified" ]; then
        printf 'release'
        return
    fi
    printf 'evidence'
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
    ARTIFACTS=("$tmp/artifact.bin")
    ATT_KEYS="$tmp/keys.json"
    export WOODPECKER_HOST WOODPECKER_TOKEN

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

    make_attestation() { # out_file [extra create args...]
        local out="$1"
        shift
        node "$ATTESTATION_JS" create \
            --out "$out" --workflow trusted --event push --repo acme/widgets \
            --source-sha "$sha" --tree-sha "$tree" \
            --pipeline-number 21 --pipeline-id 211 \
            --ci-image-ref 'node:24@sha256:64af3819f9275802414d7cdc38c27e9d82bd564dec4d4da87d008255d36c63b4' \
            --rust-toolchain 1.98.0 \
            --artifact "$tmp/artifact.bin" \
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
assert m["artifacts"] and m["artifacts"][0]["path"].endswith("artifact.bin"), m
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
assert m["release_certificate"] is True, m
'
    # --- attestation tamper: the local artifact no longer matches the attested digest.
    printf 'tampered artifact\n' >"$tmp/artifact.bin"
    expect trusted-tampered 1
    expect_problem trusted-tampered 'attestation-invalid'
    printf 'selftest artifact\n' >"$tmp/artifact.bin"
    # --- attestation signed by a foreign identity.
    make_attestation "$tmp/foreign-att.json" --sign-key "$tmp/foreign.pem" --key-id foreign >/dev/null
    log_block "$tmp/foreign-att.json" >"$tmp/foreign-block.txt"
    CERTIFY_TEST_PIPELINES="[$(list_json 21 211 success "$sha" push)]" \
        CERTIFY_TEST_DETAIL="$(detail_json 21 211 success "$sha" push 'trusted=success' 701)" \
        CERTIFY_TEST_LOGS="$(logs_json "$tmp/foreign-block.txt" 701)" \
        set_state
    expect foreign-signature 1
    expect_problem foreign-signature 'attestation-invalid'
    # --- unsigned attestation is not release-grade.
    make_attestation "$tmp/unsigned-att.json" >/dev/null 2>&1
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
assert m["release_certificate"] is True, m
'
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

    # --- certificate class rule (P1-K): only local pass AND (trusted or signed attestation).
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
    check_class 1 0 untrusted verified release
    check_class 1 0 trusted absent release
    check_class 1 0 untrusted absent evidence
    check_class 0 0 trusted verified evidence
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

    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    rm -rf "$tmp"

    if [ "$failures" -eq 0 ]; then
        echo "certify selftest: PASS (context registry + observed-value matrix, attestation fetch/verification matrix, certificate wording, temp-name migration)"
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

    step "gate 1/8: cargo fmt --check"
    if cargo fmt --check; then ok "formatting clean"; else status=1; bad "formatting drift"; fi

    step "gate 2/8: cargo check --workspace"
    if cargo check --workspace; then ok "workspace check clean"; else status=1; bad "workspace check failed"; fi

    step "gate 3/8: cargo clippy --workspace --all-targets -- -D warnings"
    if cargo clippy --workspace --all-targets -- -D warnings; then
        ok "clippy clean (-D warnings)"
    else
        status=1
        bad "clippy warnings"
    fi

    step "gate 4/8: cargo test --workspace"
    if cargo test --workspace; then ok "workspace tests pass"; else status=1; bad "workspace tests failed"; fi

    step "gate 5/8: doctor --deep on a fresh data dir"
    if run_doctor_deep "pre-campaign doctor --deep"; then ok "pre-campaign doctor --deep clean"; else status=1; fi

    step "gate 6/8: fault campaign ([fault] ignored tests)"
    if fault_campaign; then ok "fault campaign complete"; else status=1; fi

    step "gate 7/8: doctor --deep after the fault campaign (release certification)"
    if run_doctor_deep "post-campaign doctor --deep"; then ok "post-campaign doctor --deep clean"; else status=1; fi

    step "gate 8/8: artifact branding scan (packaged outputs)"
    if artifact_scan; then ok "artifact branding gate complete"; else status=1; fi

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

summary_gates=()
if [ "$VERIFY_CI" -eq 0 ] && [ "$local_ran" -eq 1 ]; then
    summary_gates=("${gates[@]}")
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
elif [ "$final" -eq 0 ]; then
    if [ "$release_class" = "release" ]; then
        printf 'RELEASE CERTIFICATE: PASS\n'
        for g in "${summary_gates[@]}"; do printf '  [x] %s\n' "$g"; done
        printf '  commit %s, tree %s\n' "$commit_sha" "$tree_sha"
        printf 'Exact-SHA CI evidence verified; manifest at %s.\n' "$MANIFEST_OUT"
    else
        printf 'CERTIFICATION: PASS — NOT A RELEASE CERTIFICATE\n'
        for g in "${summary_gates[@]}"; do printf '  [x] %s\n' "$g"; done
        printf '  commit %s, tree %s\n' "$commit_sha" "$tree_sha"
        printf 'Reason: release class requires a trusted context (push/trusted or tag/trusted)\n'
        printf 'or a verified signed build attestation at this exact commit.\n'
        printf 'Manifest at %s.\n' "$MANIFEST_OUT"
    fi
elif [ "$final" -eq 2 ]; then
    printf 'CERTIFICATION: INCOMPLETE (operator action required)\n'
    for g in "${summary_gates[@]}"; do printf '  [ ] %s\n' "$g"; done
    printf 'Certification could not run to completion — do not ship.\n'
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
exit "$final"




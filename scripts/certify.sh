#!/usr/bin/env bash
# Release certification (P0-97 / P0-74 / P0-100).
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
# Gate 9 (additive, Phase E item 14): REAL CI certification for the EXACT
# commit being shipped. Local green gates are necessary but never sufficient:
# commit messages, PR descriptions and local test runs are NOT evidence. The
# release certificate exists only when the required Woodpecker context
# `ci/woodpecker/pr/pr` succeeded at the exact 40-hex commit SHA being
# shipped. The check queries the Woodpecker API (WOODPECKER_HOST +
# WOODPECKER_TOKEN; a clear operator error otherwise), resolves the pipeline
# for that SHA (pull_request event), requires its `pr` workflow to be
# `success`, and writes a release manifest recording commit/tree/context/run
# URL/artifact digests plus a canonical certification-evidence digest. It
# fails on absent / pending / error / failure / killed contexts and on any
# pipeline belonging to another SHA.
#
# Usage:
#   bash scripts/certify.sh                       # local gates + required CI evidence
#   bash scripts/certify.sh --commit <sha>        # certify an exact shipped SHA
#   bash scripts/certify.sh --local-only          # local gates only (NOT a release certificate)
#   bash scripts/certify.sh --ci-only             # CI evidence only (no cargo gates)
#   bash scripts/certify.sh --selftest            # hermetic mock-API rejection matrix
#
# Env: WOODPECKER_HOST, WOODPECKER_TOKEN (required unless --local-only/
# --selftest); WOODPECKER_REPO / --repo owner/name; WOODPECKER_REPO_ID;
# CERTIFY_CI_CONTEXT (default ci/woodpecker/pr/pr); --artifact PATH
# (repeatable; packaged outputs auto-discovered when omitted).
#
# Exits non-zero on the first failing gate (exit 2 = operator/setup error:
# missing credentials, unknown ref, rejected token). Safe to run from any
# directory (resolves the workspace root); the only writes are the two temp
# data dirs and the release manifest.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

step() { printf '\n== %s ==\n' "$*"; }
ok() { printf '   ok: %s\n' "$*"; }
bad() { printf '   FAIL: %s\n' "$*"; }

# --------------------------------------------------------------- arguments --
COMMIT=""
CONTEXT="${CERTIFY_CI_CONTEXT:-ci/woodpecker/pr/pr}"
MANIFEST_OUT="target/certification/release-manifest.json"
LOCAL_ONLY=0
CI_ONLY=0
SELFTEST=0
PIPELINE=""
REPO_FULL_NAME="${CERTIFY_REPO:-${WOODPECKER_REPO:-}}"
ARTIFACTS=()

usage() {
    sed -n '2,/^set -/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'
}

while [ "$#" -gt 0 ]; do
    case "$1" in
    --local-only)
        LOCAL_ONLY=1
        shift
        ;;
    --ci-only)
        CI_ONLY=1
        shift
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
    dir="$(mktemp -d "${TMPDIR:-/tmp}/kp-cert.XXXXXX")"
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
# Gate 9: verify the required Woodpecker context at the exact commit and
# write target/certification/release-manifest.json. Everything below is
# additive to the local gates above; it needs no cargo.

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
    return 0
}

py_json() { # mode file [name]
    python3 - "$@" <<'PY'
import json
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
if mode == "repo_id":
    print(data.get("id", "") if isinstance(data, dict) else "")
elif mode == "candidates":
    rows = data if isinstance(data, list) else []
    for p in sorted(rows, key=lambda x: number(x.get("number")), reverse=True):
        print("%s\t%s\t%s\t%s" % (p.get("number", ""), p.get("status", ""), p.get("commit", ""), p.get("event", "")))
elif mode == "workflows":
    for w in (data or {}).get("workflows") or []:
        print("%s\t%s" % (w.get("name", ""), w.get("state", "")))
elif mode == "pipeline_status":
    print((data or {}).get("status", ""))
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

write_ci_manifest() { # out_file artifacts_file problems_file commit tree repo_id pipeline number status workflow_state event run_url
    CERTIFY_M_OUT="$1" CERTIFY_M_ARTIFACTS="$2" CERTIFY_M_PROBLEMS="$3" \
        CERTIFY_M_COMMIT="$4" CERTIFY_M_TREE="$5" CERTIFY_M_REPO_ID="$6" \
        CERTIFY_M_PIPELINE="$7" CERTIFY_M_PIPELINE_STATUS="$8" CERTIFY_M_WORKFLOW_STATE="$9" \
        CERTIFY_M_EVENT="${10}" CERTIFY_M_RUN_URL="${11}" \
        CERTIFY_M_REPO="$REPO_FULL_NAME" CERTIFY_M_CONTEXT="$CONTEXT" CERTIFY_M_HOST="${WOODPECKER_HOST%/}" \
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

core = {
    "artifacts": artifacts,
    "commit": env("CERTIFY_M_COMMIT"),
    "context": env("CERTIFY_M_CONTEXT"),
    "event": env("CERTIFY_M_EVENT"),
    "pipeline_number": env("CERTIFY_M_PIPELINE"),
    "pipeline_status": env("CERTIFY_M_PIPELINE_STATUS"),
    "repository": env("CERTIFY_M_REPO"),
    "run_url": env("CERTIFY_M_RUN_URL"),
    "tree": env("CERTIFY_M_TREE"),
    "workflow_state": env("CERTIFY_M_WORKFLOW_STATE"),
}
manifest = {
    "schema": "faktor-release-certification/v1",
    "status": "passed" if not problems else "failed",
    "repository": env("CERTIFY_M_REPO"),
    "repository_id": env("CERTIFY_M_REPO_ID"),
    "commit": env("CERTIFY_M_COMMIT"),
    "tree": env("CERTIFY_M_TREE"),
    "context": env("CERTIFY_M_CONTEXT"),
    "woodpecker_host": env("CERTIFY_M_HOST"),
    "pipeline_number": env("CERTIFY_M_PIPELINE"),
    "pipeline_status": env("CERTIFY_M_PIPELINE_STATUS"),
    "workflow": "pr",
    "workflow_state": env("CERTIFY_M_WORKFLOW_STATE"),
    "event": env("CERTIFY_M_EVENT"),
    "run_url": env("CERTIFY_M_RUN_URL"),
    "artifacts": artifacts,
    "artifact_digest": artifact_digest,
    "evidence_digest": digest(json.dumps(core, sort_keys=True, separators=(",", ":"))),
    "evidence_policy": "commit-message claims are not evidence; only the required CI context succeeding at this exact commit certifies",
    "certified_at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
    "problems": problems,
}
out = env("CERTIFY_M_OUT")
with open(out, "w") as fh:
    fh.write(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
print(manifest["status"])
print(manifest["evidence_digest"])
PY
}

verify_woodpecker_context() { # commit tree manifest_out -> 0 verified, 1 not verified, 2 operator error
    local commit="$1" tree="$2" manifest_out="$3"
    require_woodpecker_env || return 2

    local api="${WOODPECKER_HOST%/}/api"
    local tmp body candidates_file detail_file artifacts_file problems_file
    local repo_id code line number status cand_commit cand_event
    local pipeline_number="" pipeline_status="" workflow_state="" event="" run_url="" detail_commit=""

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

    tmp="$(mktemp -d "${TMPDIR:-/tmp}/kp-cert-ci.XXXXXX")"
    body="$tmp/body.json"
    candidates_file="$tmp/candidates.tsv"
    detail_file="$tmp/pipeline.json"
    artifacts_file="$tmp/artifacts.tsv"
    problems_file="$tmp/problems.txt"
    : >"$problems_file"

    # 1. repository id (lookup by owner/name).
    repo_id="${WOODPECKER_REPO_ID:-}"
    if [ -z "$repo_id" ]; then
        code="$(api_get "$api" "/repos/lookup/$REPO_FULL_NAME" "$body")"
        if [ "$code" != "200" ]; then
            rm -rf "$tmp"
            if [ "$code" = "401" ] || [ "$code" = "403" ]; then
                echo "certify: Woodpecker rejected the token (HTTP $code) for ${api}/repos/lookup/$REPO_FULL_NAME" >&2
            else
                echo "certify: Woodpecker repo lookup failed (HTTP $code) for ${api}/repos/lookup/$REPO_FULL_NAME" >&2
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

    # 2. candidates: the exact SHA's pull_request pipelines.
    pipeline_number=""
    if [ -n "$PIPELINE" ]; then
        pipeline_number="$PIPELINE"
    else
        code="$(api_get "$api" "/repos/$repo_id/pipelines?event=pull_request&per_page=50" "$body")"
        if [ "$code" != "200" ]; then
            rm -rf "$tmp"
            echo "certify: Woodpecker pipeline list failed (HTTP $code) for ${api}/repos/$repo_id/pipelines" >&2
            return 2
        fi
        py_json candidates "$body" >"$candidates_file"
        while IFS="$(printf '\t')" read -r number status cand_commit cand_event; do
            [ -n "$number" ] || continue
            [ "$cand_commit" = "$commit" ] || continue
            pipeline_number="$number"
            break
        done <"$candidates_file"
        if [ -z "$pipeline_number" ]; then
            if [ -s "$candidates_file" ]; then
                printf 'context-absent: no pull_request pipeline at commit %s (pipelines exist for other commits; the context belongs to another SHA)\n' "$commit" >>"$problems_file"
            else
                printf 'context-absent: no pull_request pipeline found for commit %s\n' "$commit" >>"$problems_file"
            fi
        fi
    fi

    # 3. pipeline detail + pr workflow state.
    if [ -n "$pipeline_number" ]; then
        code="$(api_get "$api" "/repos/$repo_id/pipelines/$pipeline_number" "$detail_file")"
        if [ "$code" != "200" ]; then
            rm -rf "$tmp"
            echo "certify: Woodpecker pipeline $pipeline_number fetch failed (HTTP $code)" >&2
            return 2
        fi
        event="pull_request"
        pipeline_status="$(py_json pipeline_status "$detail_file")"
        workflow_state="$(py_json workflows "$detail_file" | awk -F'\t' -v want="pr" '$1 == want { print $2; exit }')"
        if [ -n "$PIPELINE" ]; then
            # Explicit pipeline override still must belong to the exact SHA.
            detail_commit="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("commit",""))' "$detail_file" 2>/dev/null)"
            if [ "$detail_commit" != "$commit" ]; then
                echo "certify: pipeline $pipeline_number belongs to commit ${detail_commit}, not ${commit}" >&2
                rm -rf "$tmp"
                return 1
            fi
        fi
        if [ -z "$workflow_state" ]; then
            printf 'context-absent: pipeline %s has no workflow named pr\n' "$pipeline_number" >>"$problems_file"
        else
            case "$workflow_state" in
            success)
                [ "$pipeline_status" = "success" ] ||
                    printf 'context-failure: pipeline status is %s while the pr workflow is success\n' "$pipeline_status" >>"$problems_file"
                ;;
            pending | running | blocked | created | started)
                printf 'context-pending: pr workflow is %s (pipeline %s)\n' "$workflow_state" "$pipeline_number" >>"$problems_file"
                ;;
            failure | killed | canceled | declined | skipped)
                printf 'context-failure: pr workflow is %s (pipeline %s)\n' "$workflow_state" "$pipeline_number" >>"$problems_file"
                ;;
            error)
                printf 'context-error: pr workflow is error (pipeline %s)\n' "$pipeline_number" >>"$problems_file"
                ;;
            *)
                printf 'context-unknown: pr workflow state %s (pipeline %s)\n' "$workflow_state" "$pipeline_number" >>"$problems_file"
                ;;
            esac
        fi
    fi

    # 4. artifacts to bind into the manifest.
    collect_artifacts "$artifacts_file" || {
        rm -rf "$tmp"
        return 2
    }

    if [ -n "$pipeline_number" ]; then
        run_url="${WOODPECKER_HOST%/}/repos/$repo_id/pipeline/$pipeline_number"
    fi

    # 5. manifest (written for pass and fail alike).
    manifest_status="$(write_ci_manifest "$manifest_out" "$artifacts_file" "$problems_file" \
        "$commit" "$tree" "$repo_id" "$pipeline_number" "$pipeline_status" "$workflow_state" "$event" "$run_url" | sed -n '1p')"
    rm -rf "$tmp"

    case "$manifest_status" in
    passed)
        ok "Woodpecker context $CONTEXT succeeded at $commit (pipeline $pipeline_number, $run_url)"
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

run_ci_selftest() {
    command -v python3 >/dev/null 2>&1 || {
        echo "certify selftest: python3 is required" >&2
        return 2
    }
    command -v curl >/dev/null 2>&1 || {
        echo "certify selftest: curl is required" >&2
        return 2
    }
    local fixtures tmp port pid ready sha tree failures=0 code manifest
    fixtures="$ROOT/scripts/certification/fixtures/woodpecker-api"
    if [ ! -d "$fixtures" ]; then
        echo "certify selftest: fixtures missing at $fixtures" >&2
        return 2
    fi
    sha="0123456789abcdef0123456789abcdef01234567"
    tree="fedcba9876543210fedcba9876543210fedcba98"
    tmp="$(mktemp -d "${TMPDIR:-/tmp}/kp-cert-selftest.XXXXXX")"
    cp -R "$fixtures/." "$tmp/"
    printf 'selftest artifact\n' >"$tmp/artifact.bin"
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
    commit="$sha"
    export WOODPECKER_HOST WOODPECKER_TOKEN

    set_state() { # pipelines_json detail_json
        python3 - "$tmp/state.json" "$1" "$2" <<'PY'
import json
import sys

state_path, pipelines, detail = sys.argv[1:4]
with open(state_path, "w") as fh:
    json.dump({"pipelines": json.loads(pipelines), "detail": json.loads(detail)}, fh)
PY
    }

    set_pipeline() { # number status detail_workflows detail_status [list_commit]
        local number="$1" status="$2" workflows="$3" detail_status="$4" list_commit="${5:-$sha}"
        local entries
        entries="$(python3 -c '
import json
import sys

number, status, workflows = sys.argv[1:4]
items = []
for pair in workflows.split(","):
    if not pair:
        continue
    name, state = pair.split("=")
    items.append({"name": name, "state": state})
print(json.dumps({"number": int(number), "status": status, "workflows": items}))
' "$number" "$detail_status" "$workflows")"
        set_state "[{\"number\": $number, \"status\": \"$status\", \"commit\": \"$list_commit\", \"event\": \"pull_request\"}]" "$entries"
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

    # success: exact SHA, pull_request, pr workflow success.
    set_pipeline 11 success "pr=success,linux=success" success
    expect success 0
    manifest="$tmp/manifest-success.json"
    if [ -f "$manifest" ] && python3 - "$manifest" "$sha" <<'PY'
import json
import sys

m = json.load(open(sys.argv[1]))
assert m["status"] == "passed", m
assert m["commit"] == sys.argv[2], m
assert m["context"] == "ci/woodpecker/pr/pr", m
assert m["evidence_digest"].startswith("sha256:") and len(m["evidence_digest"]) == 71, m
assert m["run_url"].endswith("/repos/7/pipeline/11"), m
assert m["artifacts"] and m["artifacts"][0]["path"].endswith("artifact.bin"), m
PY
    then
        echo "selftest ok: manifest binds commit/tree/context/run URL/artifacts/evidence digest"
    else
        echo "selftest FAIL: manifest binding" >&2
        failures=$((failures + 1))
    fi
    first_digest="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["evidence_digest"])' "$manifest")"
    expect success-again 0
    second_digest="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["evidence_digest"])' "$tmp/manifest-success-again.json")"
    if [ "$first_digest" = "$second_digest" ]; then
        echo "selftest ok: evidence digest is deterministic"
    else
        echo "selftest FAIL: evidence digest not deterministic" >&2
        failures=$((failures + 1))
    fi

    # absent: no pipeline for the SHA.
    set_state '[]' '{"number": 11, "status": "success", "workflows": [{"name": "pr", "state": "success"}]}'
    expect absent 1
    # other SHA: pipeline exists but for a different commit.
    set_state '[{"number": 12, "status": "success", "commit": "ffffffffffffffffffffffffffffffffffffffff", "event": "pull_request"}]' '{"number": 12, "status": "success", "workflows": [{"name": "pr", "state": "success"}]}'
    expect other-sha 1
    # pending.
    set_pipeline 11 running "pr=running,linux=running" running
    expect pending 1
    # failure.
    set_pipeline 11 failure "pr=failure,linux=success" failure
    expect failure 1
    # error.
    set_pipeline 11 error "pr=error,linux=success" error
    expect error 1
    # wrong workflow name: the context is absent even though the pipeline passed.
    set_pipeline 11 success "trusted=success" success
    expect wrong-workflow 1
    # operator error: credentials missing.
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

    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    rm -rf "$tmp"

    if [ "$failures" -eq 0 ]; then
        echo "certify selftest: PASS (Woodpecker context rejection matrix + operator error + manifest binding)"
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

if [ "$SELFTEST" -eq 1 ]; then
    step "hermetic CI-verification selftest (mock Woodpecker API)"
    if run_ci_selftest; then exit 0; else exit 1; fi
fi

if [ "$CI_ONLY" -eq 0 ]; then
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
else
    step "ci-only: local cargo gates skipped by --ci-only"
fi

if [ "$LOCAL_ONLY" -eq 1 ]; then
    step "gate 9/9: CI context verification skipped (--local-only)"
    bad "--local-only is a local pre-flight, NOT a release certificate: the required CI context was not verified"
else
    step "gate 9/9: Woodpecker context $CONTEXT at the EXACT commit"
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

final="$status"
[ "$ci_status" -gt "$final" ] && final="$ci_status"

summary_gates=()
if [ "$CI_ONLY" -eq 0 ]; then
    summary_gates=("${gates[@]}")
fi
if [ "$LOCAL_ONLY" -eq 0 ]; then
    summary_gates+=("CI context $CONTEXT at the exact shipped commit")
fi

printf '\n=====================\n'
if [ "$final" -eq 0 ]; then
    printf 'RELEASE CERTIFICATE: PASS\n'
    for g in "${summary_gates[@]}"; do printf '  [x] %s\n' "$g"; done
    printf '  commit %s, tree %s\n' "$commit_sha" "$tree_sha"
    printf 'Exact-SHA CI evidence verified; manifest at %s.\n' "$MANIFEST_OUT"
elif [ "$final" -eq 2 ]; then
    printf 'RELEASE CERTIFICATE: INCOMPLETE (operator action required)\n'
    for g in "${summary_gates[@]}"; do printf '  [ ] %s\n' "$g"; done
    printf 'Certification could not run to completion — do not ship.\n'
else
    printf 'RELEASE CERTIFICATE: FAIL\n'
    for g in "${summary_gates[@]}"; do printf '  [ ] %s\n' "$g"; done
    printf 'At least one gate failed — do not ship.\n'
fi
if [ "$LOCAL_ONLY" -eq 1 ]; then
    printf 'NOTE: --local-only runs never certify a release (CI evidence is mandatory).\n'
fi
if [ "$CI_ONLY" -eq 1 ]; then
    printf 'NOTE: --ci-only verified the CI evidence only; the local cargo gates were not run.\n'
fi
if [ "$SELFTEST" -eq 1 ]; then
    printf 'NOTE: --selftest is a hermetic fixture run, never a release certificate.\n'
fi
printf '=====================\n'
exit "$final"

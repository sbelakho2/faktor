#!/usr/bin/env bash
# Verify the untrusted-PR / trusted-push boundary in two layers:
#
#   1. REPOSITORY LAYOUT (always, offline): the two workflow file sets exist,
#      the untrusted PR workflow declares no volumes and no `curl | sh`
#      bootstrap, the trusted file set is the only one with named volumes, and
#      no workflow sits at the `.woodpecker/` top level (the top-level
#      symlinks are in-repo tooling shims only).
#   2. SERVER-SIDE PROJECT SETTINGS (when WOODPECKER_HOST + WOODPECKER_TOKEN
#      are set): project ids, pipeline paths, allow_pr and trusted.volumes
#      per project, and the cron job existing on the trusted project only.
#      These settings ARE the trust boundary; layer 1 is defense-in-depth.
#
# Usage:
#   bash scripts/woodpecker/verify-boundary.sh [owner/repo] [--dry-run]
#
# Exit codes: 0 all checks pass; 1 a violation; 2 usage/setup error.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT" || exit 2

HOST="${WOODPECKER_HOST:-}"
TOKEN="${WOODPECKER_TOKEN:-}"
DRY_RUN=0
REPO_FULL_NAME=""
UNTRUSTED_CONFIG_FILE=".woodpecker/untrusted/"
TRUSTED_CONFIG_FILE=".woodpecker/trusted/"

die() { echo "verify-boundary: $*" >&2; exit 2; }
bad() {
    echo "verify-boundary: VIOLATION: $*" >&2
    FAIL=1
}
ok() { echo "verify-boundary: ok: $*"; }

FAIL=0

while [ $# -gt 0 ]; do
    case "$1" in
    --dry-run)
        DRY_RUN=1
        shift
        ;;
    -h | --help)
        sed -n '2,/^set -/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'
        exit 0
        ;;
    --*)
        die "unknown option: $1"
        ;;
    *)
        [ -z "$REPO_FULL_NAME" ] || die "unexpected argument: $1"
        REPO_FULL_NAME="$1"
        shift
        ;;
    esac
done

# --------------------------------------------------- 1. repository layout --
PR_FILE=".woodpecker/untrusted/pr.yaml"
TRUSTED_FILES=(".woodpecker/trusted/trusted.yaml" ".woodpecker/trusted/nightly.yaml")

[ -f "$PR_FILE" ] || bad "$PR_FILE is missing"
for f in "${TRUSTED_FILES[@]}"; do
    [ -f "$f" ] || bad "$f is missing"
done

if [ -f "$PR_FILE" ]; then
    if grep -nE '^[[:space:]]*volumes:' "$PR_FILE"; then
        bad "$PR_FILE declares volumes (untrusted PR workflow)"
    fi
    if grep -vE '^[[:space:]]*#' "$PR_FILE" | grep -nE 'faktor-(trusted|nightly)-'; then
        bad "$PR_FILE references trusted/cron cache volumes"
    fi
    if grep -vE '^[[:space:]]*#' "$PR_FILE" | grep -nE '\|[[:space:]]*(ba)?sh([[:space:]]|$)|sh\.rustup\.rs -sSf|\|[[:space:]]*sh -s'; then
        bad "$PR_FILE pipes a remote download into a shell"
    fi
fi

for f in "${TRUSTED_FILES[@]}"; do
    [ -f "$f" ] && grep -qE '^[[:space:]]*volumes:' "$f" &&
        ok "$f declares the trusted/nightly named volumes"
done

stray="$(find .woodpecker -maxdepth 1 -type f \( -name '*.yaml' -o -name '*.yml' \) -print 2>/dev/null || true)"
[ -z "$stray" ] || bad "workflow file(s) at the .woodpecker/ top level: $stray"
for link in .woodpecker/pr.yaml .woodpecker/trusted.yaml .woodpecker/nightly.yaml; do
    if [ -e "$link" ] && [ ! -L "$link" ]; then
        bad "$link must be a symlink (in-repo tooling shim) or absent"
    fi
done

grep -q -- '--yaml-dir .woodpecker/untrusted' "$PR_FILE" ||
    bad "$PR_FILE certificate does not pass --yaml-dir .woodpecker/untrusted"
grep -q -- '--yaml-dir .woodpecker/trusted' .woodpecker/trusted/trusted.yaml ||
    bad ".woodpecker/trusted/trusted.yaml certificate does not pass --yaml-dir .woodpecker/trusted"
grep -q -- '--yaml-dir .woodpecker/trusted' .woodpecker/trusted/nightly.yaml ||
    bad ".woodpecker/trusted/nightly.yaml certificate does not pass --yaml-dir .woodpecker/trusted"

[ "$FAIL" -eq 0 ] && ok "repository layout and in-repo guards"

# ------------------------------------------- 2. server-side project settings --
if [ -z "$HOST" ] || [ -z "$TOKEN" ]; then
    echo "verify-boundary: server settings NOT checked (set WOODPECKER_HOST + WOODPECKER_TOKEN)"
    [ "$FAIL" -eq 0 ] || exit 1
    exit 0
fi

command -v curl >/dev/null 2>&1 || die "curl is required for API checks"
command -v python3 >/dev/null 2>&1 || die "python3 is required for API checks"

if [ -z "$REPO_FULL_NAME" ]; then
    remote="$(git remote get-url origin 2>/dev/null || true)"
    case "$remote" in
    *github.com[:/]*)
        REPO_FULL_NAME="$(printf '%s' "$remote" | sed -E 's#.*github\.com[:/]([^/]+/[^/]+?)(\.git)?$#\1#')"
        ;;
    esac
fi
case "$REPO_FULL_NAME" in
*/*) ;;
*) die "pass the repository as owner/repo" ;;
esac
OWNER="${REPO_FULL_NAME%%/*}"
REPO="${REPO_FULL_NAME##*/}"
API="${HOST%/}/api"

RESPONSE_BODY=""
RESPONSE_STATUS=""
request() {
    local method="$1" path="$2" url body_file
    url="${API}${path}"
    if [ "$DRY_RUN" -eq 1 ]; then
        echo "verify-boundary: DRY-RUN ${method} ${url}"
        RESPONSE_BODY="{}"
        RESPONSE_STATUS="200"
        return 0
    fi
    body_file="$(mktemp)"
    RESPONSE_STATUS="$(curl -sS -o "$body_file" -w '%{http_code}' -X "$method" \
        -H "Authorization: Bearer ${TOKEN}" "$url" || echo "000")"
    RESPONSE_BODY="$(cat "$body_file")"
    rm -f "$body_file"
}

json_field() {
    local path="$1" default="${2:-}"
    printf '%s' "$RESPONSE_BODY" | python3 -c '
import json, sys
try:
    data = json.load(sys.stdin)
except Exception:
    print(sys.argv[2])
    sys.exit(0)
for key in sys.argv[1].split("."):
    if key == "":
        continue
    try:
        if isinstance(data, list):
            data = data[int(key)]
        else:
            data = data.get(key)
    except Exception:
        print(sys.argv[2])
        sys.exit(0)
    if data is None:
        print(sys.argv[2])
        sys.exit(0)
print(data if not isinstance(data, (dict, list)) else json.dumps(data))
' "$path" "$default"
}

check_project() { # NAME QUERY CONFIG_FILE ALLOW_PR VOLUMES
    local name="$1" query="$2" config_file="$3" allow_pr="$4" volumes="$5"
    request GET "/repos/lookup/${OWNER}/${REPO}${query}"
    if [ "$DRY_RUN" -eq 1 ]; then
        ok "${name}: DRY-RUN lookup"
        return 0
    fi
    if [ "$RESPONSE_STATUS" != "200" ]; then
        bad "${name}: lookup returned ${RESPONSE_STATUS} (project missing?)"
        return 0
    fi
    local id got_config got_pr got_vol
    id="$(json_field id)"
    got_config="$(json_field config_file)"
    got_pr="$(printf '%s' "$(json_field allow_pr)" | tr '[:upper:]' '[:lower:]')"
    got_vol="$(printf '%s' "$(json_field trusted.volumes)" | tr '[:upper:]' '[:lower:]')"
    [ "$got_config" = "$config_file" ] ||
        bad "${name} (id=${id}): config_file='${got_config}' != '${config_file}'"
    [ "$got_pr" = "$allow_pr" ] ||
        bad "${name} (id=${id}): allow_pr=${got_pr} != ${allow_pr}"
    [ "$got_vol" = "$volumes" ] ||
        bad "${name} (id=${id}): trusted.volumes=${got_vol} != ${volumes}"
    case "$name" in
    untrusted) UNTRUSTED_ID="$id" ;;
    trusted) TRUSTED_ID="$id" ;;
    esac
    [ "$got_config" = "$config_file" ] && [ "$got_pr" = "$allow_pr" ] && [ "$got_vol" = "$volumes" ] &&
        ok "${name} (id=${id}): config_file=${config_file} allow_pr=${allow_pr} trusted.volumes=${volumes}"
}

UNTRUSTED_ID=""
TRUSTED_ID=""
check_project untrusted "" "$UNTRUSTED_CONFIG_FILE" "true" "false"
check_project trusted "?project=trusted" "$TRUSTED_CONFIG_FILE" "false" "true"

if [ "$DRY_RUN" -eq 0 ] && [ -n "$UNTRUSTED_ID" ] && [ -n "$TRUSTED_ID" ]; then
    request GET "/repos/${TRUSTED_ID}/cron"
    trusted_cron="$(json_field 0.name '')"
    [ -n "$trusted_cron" ] || bad "trusted project ${TRUSTED_ID} has no cron job (expected nightly)"
    request GET "/repos/${UNTRUSTED_ID}/cron"
    untrusted_cron="$(json_field 0.name '')"
    [ -z "$untrusted_cron" ] ||
        bad "untrusted project ${UNTRUSTED_ID} has a cron job (${untrusted_cron}); cron belongs to the trusted project only"
    [ -z "$untrusted_cron" ] && [ -n "$trusted_cron" ] &&
        ok "cron: nightly on trusted only (untrusted has none)"
fi

if [ "$FAIL" -ne 0 ]; then
    echo "verify-boundary: FAIL" >&2
    exit 1
fi
echo "verify-boundary: PASS (layout guards + server-side project settings)"

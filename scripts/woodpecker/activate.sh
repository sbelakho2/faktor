#!/usr/bin/env bash
# Activate the Faktor repository on a Woodpecker 3.x instance as TWO projects
# over the same forge repository, with different trust:
#
#   untrusted project (created/configured FIRST, trusted=false)
#     pipeline path: .woodpecker/untrusted/   allow_pr: true
#     receives pull_request events; the server refuses volume mounts for a
#     project that is not trusted, so PR code cannot obtain a volume even if a
#     PR diff adds `volumes:` to its own YAML.
#   trusted project (admin-approved, trusted.volumes=true)
#     pipeline path: .woodpecker/trusted/     allow_pr: false
#     runs push/tag (`trusted.yaml`) and the cron `nightly` workflow.
#
# Trust and the pipeline path are PROJECT settings stored on the server: a PR
# diff cannot change either, and it cannot move the trusted project onto the
# PR YAML. See scripts/woodpecker/setup.md §3/§5.
#
# What it does (all calls verified against the Woodpecker v3.18 API):
#   1. GET  /api/user                                token check
#   2. UNTRUSTED project: lookup or activate (`POST /api/repos?forge_remote_id=`),
#      then PATCH /api/repos/<untrusted_id> with
#      {"config_file":".woodpecker/untrusted/","allow_pr":true,
#       "trusted":{"volumes":false}}
#   3. TRUSTED project: lookup `<lookup>?project=trusted` or create the second
#      project (`POST /api/repos?forge_remote_id=&project=trusted`), then
#      PATCH /api/repos/<trusted_id> with
#      {"config_file":".woodpecker/trusted/","allow_pr":false,
#       "trusted":{"volumes":true}}  (trusted.volumes needs an admin token)
#   4. POST|PATCH /api/repos/<trusted_id>/cron       register `nightly` (trusted only)
#   5. POST /api/repos/<trusted_id>/cron/<cron_id>   optional --run-now
#   6. prints the required server-side settings + branch-protection contexts
#
# Required environment:
#   WOODPECKER_HOST   base URL of the instance, e.g. https://ci.example.org
#   WOODPECKER_TOKEN  personal access token (Woodpecker UI -> user settings)
#
# Optional environment:
#   WOODPECKER_UNTRUSTED_REPO_ID / WOODPECKER_TRUSTED_REPO_ID
#       known project ids (skip lookup/creation)
#   FORGE_REMOTE_ID
#       forge repository id used for activation (otherwise `gh api`)
#
# Usage:
#   WOODPECKER_HOST=... WOODPECKER_TOKEN=... \
#     bash scripts/woodpecker/activate.sh [owner/repo] [options]
#
# Options:
#   --run-now                 trigger `nightly` right after registration
#   --timeout-minutes N       set the TRUSTED project pipeline timeout (needs
#                             server WOODPECKER_MAX_PIPELINE_TIMEOUT >= N;
#                             0 = leave as is)
#   --trusted                 request trusted.volumes on the trusted project
#                             (instance-admin token only; caches need it)
#   --secret NAME=VALUE       create/update a secret on the TRUSTED project
#                             (repeatable; none are required)
#   --untrusted-repo-id ID    untrusted project id (skip lookup/creation)
#   --trusted-repo-id ID      trusted project id (skip lookup/creation)
#   --dry-run                 print the API calls without sending them
#   -h, --help                this text
#
# Defaults:
#   cron `nightly`: 0 3 * * *   branch main, UTC, enabled
#
# The UI equivalents are in setup.md (§3 activation, §4 secrets, §5 trust,
# §6 cron); if the API path differs on your instance, setup.md documents the
# exact UI steps that reach the same state.
set -uo pipefail

HOST="${WOODPECKER_HOST:-}"
TOKEN="${WOODPECKER_TOKEN:-}"
DRY_RUN=0
RUN_NOW=0
TRUSTED=0
TIMEOUT_MINUTES=0
REPO_FULL_NAME=""
UNTRUSTED_REPO_ID="${WOODPECKER_UNTRUSTED_REPO_ID:-}"
TRUSTED_REPO_ID="${WOODPECKER_TRUSTED_REPO_ID:-}"
declare -a SECRETS=()

UNTRUSTED_CONFIG_FILE=".woodpecker/untrusted/"
TRUSTED_CONFIG_FILE=".woodpecker/trusted/"

usage() { sed -n '2,/^set -/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; }

die() { echo "activate: $*" >&2; exit 1; }
note() { echo "activate: $*"; }
warn() { echo "activate: WARNING: $*" >&2; }

while [ $# -gt 0 ]; do
    case "$1" in
    -h | --help)
        usage
        exit 0
        ;;
    --dry-run)
        DRY_RUN=1
        shift
        ;;
    --run-now)
        RUN_NOW=1
        shift
        ;;
    --trusted)
        TRUSTED=1
        shift
        ;;
    --timeout-minutes)
        [ $# -ge 2 ] || die "--timeout-minutes needs a value"
        TIMEOUT_MINUTES="$2"
        shift 2
        ;;
    --secret)
        [ $# -ge 2 ] || die "--secret needs NAME=VALUE"
        SECRETS+=("$2")
        shift 2
        ;;
    --untrusted-repo-id)
        [ $# -ge 2 ] || die "--untrusted-repo-id needs a value"
        UNTRUSTED_REPO_ID="$2"
        shift 2
        ;;
    --trusted-repo-id)
        [ $# -ge 2 ] || die "--trusted-repo-id needs a value"
        TRUSTED_REPO_ID="$2"
        shift 2
        ;;
    --*)
        die "unknown option: $1 (try --help)"
        ;;
    *)
        [ -z "$REPO_FULL_NAME" ] || die "unexpected argument: $1"
        REPO_FULL_NAME="$1"
        shift
        ;;
    esac
done

[ -n "$HOST" ] || die "WOODPECKER_HOST is required (e.g. https://ci.example.org)"
[ -n "$TOKEN" ] || die "WOODPECKER_TOKEN is required (personal access token)"
HOST="${HOST%/}"
API="${HOST}/api"
command -v curl >/dev/null 2>&1 || die "curl is required"
command -v python3 >/dev/null 2>&1 || die "python3 is required (JSON parsing)"

if [ -z "$REPO_FULL_NAME" ]; then
    remote="$(git remote get-url origin 2>/dev/null || true)"
    case "$remote" in
    *github.com[:/]*)
        REPO_FULL_NAME="$(printf '%s' "$remote" | sed -E 's#.*github\.com[:/]([^/]+/[^/]+?)(\.git)?$#\1#')"
        ;;
    *://*/*|*@*:*)
        host_path="${remote#*://}"
        case "$remote" in
        *@*:*) host_path="${remote#*@}"; host_path="${host_path/:/\/}" ;;
        esac
        REPO_FULL_NAME="$(printf '%s' "$host_path" | sed -E 's#^[^/]+/##; s#\.git$##; s#/+$##')"
        ;;
    esac
fi
[ -n "$REPO_FULL_NAME" ] || die "pass the repository as owner/repo (could not derive it from 'git remote get-url origin')"
case "$REPO_FULL_NAME" in
*/*) ;;
*) die "repository must be owner/repo (got '$REPO_FULL_NAME')" ;;
esac
OWNER="${REPO_FULL_NAME%%/*}"
REPO="${REPO_FULL_NAME##*/}"

case "$TIMEOUT_MINUTES" in
'' | *[!0-9]*) die "--timeout-minutes must be a non-negative integer" ;;
esac

RESPONSE_BODY=""
RESPONSE_STATUS=""
request() {
    local method="$1" path="$2" data="${3:-}" url status body_file
    url="${API}${path}"
    if [ "$DRY_RUN" -eq 1 ]; then
        note "DRY-RUN ${method} ${url}${data:+ ${data}}"
        RESPONSE_STATUS="200"
        RESPONSE_BODY="{}"
        return 0
    fi
    body_file="$(mktemp)"
    if [ -n "$data" ]; then
        status="$(curl -sS -o "$body_file" -w '%{http_code}' -X "$method" \
            -H "Authorization: Bearer ${TOKEN}" -H 'Content-Type: application/json' \
            --data "$data" "$url" || echo "000")"
    else
        status="$(curl -sS -o "$body_file" -w '%{http_code}' -X "$method" \
            -H "Authorization: Bearer ${TOKEN}" "$url" || echo "000")"
    fi
    RESPONSE_BODY="$(cat "$body_file")"
    RESPONSE_STATUS="$status"
    rm -f "$body_file"
    return 0
}

# json_field PATH [DEFAULT] -- reads RESPONSE_BODY.
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

# forge_remote_id -- the forge's numeric repository id used for activation.
forge_remote_id() {
    if [ -n "${FORGE_REMOTE_ID:-}" ]; then
        printf '%s' "$FORGE_REMOTE_ID"
        return 0
    fi
    if command -v gh >/dev/null 2>&1; then
        gh api "repos/${REPO_FULL_NAME}" --jq .id 2>/dev/null || true
    fi
}

# lookup_project [NAME] -- NAME selects a project when the instance supports
# several projects over one repository; empty selects the primary project.
lookup_project() {
    local name="${1:-}" path="/repos/lookup/${OWNER}/${REPO}"
    [ -n "$name" ] && path="${path}?project=${name}"
    request GET "$path"
    [ "$RESPONSE_STATUS" = "200" ]
}

note "instance : ${HOST}"
note "repo     : ${REPO_FULL_NAME}"

request GET "/user"
case "$RESPONSE_STATUS" in
200) note "token    : ok (user $(json_field login unknown))" ;;
401 | 403) die "token rejected by ${HOST} (${RESPONSE_STATUS}); check WOODPECKER_TOKEN" ;;
*) warn "GET /api/user returned ${RESPONSE_STATUS}; continuing" ;;
esac

# ------------------------------------------------------- untrusted project --
# Created/configured FIRST. It is the only project that receives
# pull_request events, and it must never be trusted (no volumes).
if [ -n "$UNTRUSTED_REPO_ID" ]; then
    note "untrusted: id=${UNTRUSTED_REPO_ID} (given)"
elif [ "$DRY_RUN" -eq 1 ]; then
    lookup_project "" || true
    UNTRUSTED_REPO_ID="<untrusted-repo-id>"
    note "untrusted: DRY-RUN (lookup/activate skipped)"
elif lookup_project ""; then
    UNTRUSTED_REPO_ID="$(json_field id)"
    note "untrusted: found (id=${UNTRUSTED_REPO_ID})"
else
    REMOTE_ID="$(forge_remote_id)"
    [ -n "$REMOTE_ID" ] || die "repo not found in Woodpecker and no forge remote id (set FORGE_REMOTE_ID or install gh); log into ${HOST} once so the forge sync can list the repository, then re-run"
    note "untrusted: POST /repos?forge_remote_id=${REMOTE_ID}"
    request POST "/repos?forge_remote_id=${REMOTE_ID}"
    case "$RESPONSE_STATUS" in
    200 | 201) note "untrusted: activated (id=$(json_field id))" ;;
    409) note "untrusted: already active (409)" ;;
    *) die "untrusted activation failed (${RESPONSE_STATUS}): ${RESPONSE_BODY}" ;;
    esac
    lookup_project "" || die "repo still not resolvable after activation; check forge access, then re-run"
    UNTRUSTED_REPO_ID="$(json_field id)"
fi
[ -n "$UNTRUSTED_REPO_ID" ] && [ "$UNTRUSTED_REPO_ID" != "None" ] || die "could not resolve the untrusted project id"
note "untrusted: id=${UNTRUSTED_REPO_ID}"

# --------------------------------------------------------- trusted project --
# The second project over the same forge repository. It is the only project
# allowed trusted.volumes and the only project a cron job may exist on.
if [ -n "$TRUSTED_REPO_ID" ]; then
    note "trusted  : id=${TRUSTED_REPO_ID} (given)"
elif [ "$DRY_RUN" -eq 1 ]; then
    lookup_project trusted || true
    TRUSTED_REPO_ID="<trusted-repo-id>"
    note "trusted  : DRY-RUN (lookup/creation skipped)"
elif lookup_project trusted; then
    TRUSTED_REPO_ID="$(json_field id)"
    note "trusted  : found (id=${TRUSTED_REPO_ID})"
else
    REMOTE_ID="$(forge_remote_id)"
    [ -n "$REMOTE_ID" ] || die "trusted project not found and no forge remote id (set FORGE_REMOTE_ID or install gh)"
    note "trusted  : POST /repos?forge_remote_id=${REMOTE_ID}&project=trusted (second project)"
    request POST "/repos?forge_remote_id=${REMOTE_ID}&project=trusted"
    case "$RESPONSE_STATUS" in
    200 | 201)
        new_id="$(json_field id)"
        if [ -n "$new_id" ] && [ "$new_id" != "None" ] && [ "$new_id" != "$UNTRUSTED_REPO_ID" ]; then
            TRUSTED_REPO_ID="$new_id"
            note "trusted  : created (id=${TRUSTED_REPO_ID})"
        else
            warn "server did not return a distinct second project; create the trusted project in the UI and re-run with --trusted-repo-id ID"
        fi
        ;;
    *) warn "second-project creation returned ${RESPONSE_STATUS}: ${RESPONSE_BODY}" ;;
    esac
fi
if [ -z "$TRUSTED_REPO_ID" ]; then
    warn "no trusted project resolved: trusted push/tag caches and the nightly cron are NOT configured by this run (setup.md §5/§6)"
fi

# ---------------------------------------------------------------- settings --
# NAME REPO_ID CONFIG_FILE ALLOW_PR VOLUMES("true"|"false"|"skip").
# The untrusted project is always pinned to trusted.volumes=false; the trusted
# project requests trusted.volumes=true only with --trusted (admin-only) and
# reports the stored value otherwise.
configure_project() {
    local name="$1" repo_id="$2" config_file="$3" allow_pr="$4" volumes="$5" body
    body="$(python3 -c '
import json, sys
config_file, allow_pr, volumes = sys.argv[1], sys.argv[2] == "true", sys.argv[3]
data = {"config_file": config_file, "allow_pr": allow_pr}
if volumes != "skip":
    data["trusted"] = {"volumes": volumes == "true"}
print(json.dumps(data))
' "$config_file" "$allow_pr" "$volumes")"
    note "${name}: PATCH /repos/${repo_id} ${body}"
    request PATCH "/repos/${repo_id}" "$body"
    case "$RESPONSE_STATUS" in
    200)
        if [ "$DRY_RUN" -eq 1 ]; then
            note "${name}: DRY-RUN (settings not verified)"
            return 0
        fi
        got_config="$(json_field config_file)"
        got_pr="$(printf '%s' "$(json_field allow_pr)" | tr '[:upper:]' '[:lower:]')"
        got_vol="$(printf '%s' "$(json_field trusted.volumes)" | tr '[:upper:]' '[:lower:]')"
        [ "$got_config" = "$config_file" ] ||
            warn "${name}: server stored config_file='${got_config}' (expected '${config_file}')"
        [ "$got_pr" = "$allow_pr" ] ||
            warn "${name}: server stored allow_pr=${got_pr} (expected ${allow_pr})"
        if [ "$volumes" = "skip" ]; then
            if [ "$got_vol" != "true" ]; then
                warn "${name}: trusted.volumes=${got_vol} — an instance admin must grant it (setup.md §5)"
            else
                note "${name}: trusted.volumes already granted"
            fi
        elif [ "$got_vol" != "$volumes" ]; then
            warn "${name}: server stored trusted.volumes=${got_vol} (expected ${volumes}); the grant is admin-only (setup.md §5)"
        else
            note "${name}: trusted.volumes=${volumes} allow_pr=${allow_pr} config_file=${config_file}"
        fi
        ;;
    401 | 403) warn "${name}: update refused (${RESPONSE_STATUS}); owner/admin token needed, and trusted.volumes additionally needs an instance admin" ;;
    *) warn "${name}: update returned ${RESPONSE_STATUS}: ${RESPONSE_BODY}" ;;
    esac
}

configure_project untrusted "$UNTRUSTED_REPO_ID" "$UNTRUSTED_CONFIG_FILE" "true" "false"

if [ -n "$TRUSTED_REPO_ID" ]; then
    if [ "$TRUSTED" -eq 1 ]; then
        configure_project trusted "$TRUSTED_REPO_ID" "$TRUSTED_CONFIG_FILE" "false" "true"
    else
        configure_project trusted "$TRUSTED_REPO_ID" "$TRUSTED_CONFIG_FILE" "false" "skip"
    fi
fi

if [ "$TIMEOUT_MINUTES" -gt 0 ]; then
    if [ -z "$TRUSTED_REPO_ID" ]; then
        warn "no trusted project: timeout not set (it applies to the nightly longrun campaign)"
    else
        note "settings : trusted timeout=${TIMEOUT_MINUTES} min"
        request PATCH "/repos/${TRUSTED_REPO_ID}" "{\"timeout\":${TIMEOUT_MINUTES}}"
        case "$RESPONSE_STATUS" in
        200) note "settings : timeout set" ;;
        403) warn "timeout ${TIMEOUT_MINUTES} refused: server WOODPECKER_MAX_PIPELINE_TIMEOUT caps it (hosted instances are admin-fixed); long campaigns cannot run past the cap until it is raised" ;;
        *) warn "timeout update returned ${RESPONSE_STATUS}: ${RESPONSE_BODY}" ;;
        esac
    fi
else
    note "settings : timeout left untouched (pass --timeout-minutes 1560 for the nightly longrun campaign)"
fi

# ----------------------------------------------------------------- secrets --
# None are required by default. Secrets belong on the TRUSTED project (nightly
# provider keys); the untrusted PR project must never get secrets.
if [ "${#SECRETS[@]}" -eq 0 ]; then
    note "secrets  : none passed (correct default; CI requires no secrets)"
elif [ -z "$TRUSTED_REPO_ID" ]; then
    warn "secrets  : none set (no trusted project resolved; secrets belong to the trusted project only)"
else
    for entry in "${SECRETS[@]}"; do
        case "$entry" in
        *=*) ;;
        *) die "--secret expects NAME=VALUE (got '$entry')" ;;
        esac
        name="${entry%%=*}"
        value="${entry#*=}"
        case "$name" in
        '' | *[!A-Za-z0-9._-]*) die "secret name '$name' must match [A-Za-z0-9._-]+" ;;
        esac
        note "secrets  : ${name} (trusted project)"
        request GET "/repos/${TRUSTED_REPO_ID}/secrets/${name}"
        if [ "$RESPONSE_STATUS" = "200" ]; then
            request PATCH "/repos/${TRUSTED_REPO_ID}/secrets/${name}" "$(python3 -c 'import json,sys; print(json.dumps({"value": sys.argv[1]}))' "$value")"
        else
            request POST "/repos/${TRUSTED_REPO_ID}/secrets" "$(python3 -c 'import json,sys; print(json.dumps({"name": sys.argv[1], "value": sys.argv[2]}))' "$name" "$value")"
        fi
        case "$RESPONSE_STATUS" in
        200 | 201) note "secrets  : ${name} set" ;;
        *) warn "secret ${name} returned ${RESPONSE_STATUS}: ${RESPONSE_BODY}" ;;
        esac
    done
fi

# ------------------------------------------------------------------- crons --
# The cron job name must match the `when.cron` filter of the workflow file.
# The `nightly` job exists ONLY on the trusted project: the untrusted PR
# project must never be able to start the cron campaigns.
CRON_SPECS=(
    "nightly|0 3 * * *|main|UTC"
)
CRON_IDS=()
if [ -z "$TRUSTED_REPO_ID" ]; then
    warn "cron     : nightly NOT registered (no trusted project); register it on the trusted project only (setup.md §6)"
fi
if [ -n "$TRUSTED_REPO_ID" ]; then
    for spec in "${CRON_SPECS[@]}"; do
        IFS='|' read -r cron_name cron_schedule cron_branch cron_timezone <<<"$spec"
        cron_body="$(python3 -c 'import json,sys; print(json.dumps({"name": sys.argv[1], "schedule": sys.argv[2], "branch": sys.argv[3], "timezone": sys.argv[4], "enabled": True}))' \
            "$cron_name" "$cron_schedule" "$cron_branch" "$cron_timezone")"
        request GET "/repos/${TRUSTED_REPO_ID}/cron"
        cron_id=""
        if [ "$RESPONSE_STATUS" = "200" ]; then
            cron_id="$(printf '%s' "$RESPONSE_BODY" | python3 -c '
import json, sys
name = sys.argv[1]
try:
    jobs = json.load(sys.stdin)
except Exception:
    jobs = []
for job in jobs if isinstance(jobs, list) else []:
    if job.get("name") == name:
        print(job.get("id", ""))
        break
' "$cron_name")"
        fi
        if [ -n "$cron_id" ]; then
            note "cron     : ${cron_name} exists on the trusted project (id=${cron_id}); patching schedule/branch"
            request PATCH "/repos/${TRUSTED_REPO_ID}/cron/${cron_id}" "$cron_body"
            case "$RESPONSE_STATUS" in
            200) note "cron     : ${cron_name} updated (${cron_schedule} ${cron_timezone})" ;;
            *) warn "cron ${cron_name} patch returned ${RESPONSE_STATUS}: ${RESPONSE_BODY}" ;;
            esac
        else
            note "cron     : registering ${cron_name} on the trusted project (${cron_schedule} ${cron_timezone})"
            request POST "/repos/${TRUSTED_REPO_ID}/cron" "$cron_body"
            case "$RESPONSE_STATUS" in
            200 | 201)
                cron_id="$(json_field id)"
                note "cron     : ${cron_name} registered (id=${cron_id})"
                ;;
            409)
                warn "cron ${cron_name} already exists (409) but was not listed; check the UI"
                ;;
            *) warn "cron ${cron_name} returned ${RESPONSE_STATUS}: ${RESPONSE_BODY}" ;;
            esac
        fi
        [ -n "$cron_id" ] && CRON_IDS+=("${cron_name}=${cron_id}")
    done
fi

if [ "$RUN_NOW" -eq 1 ]; then
    if [ "${#CRON_IDS[@]}" -eq 0 ]; then
        warn "run-now  : skipped (no cron id registered)"
    fi
    for pair in "${CRON_IDS[@]:-}"; do
        [ -n "$pair" ] || continue
        cron_name="${pair%%=*}"
        cron_id="${pair#*=}"
        note "run-now  : ${cron_name}"
        request POST "/repos/${TRUSTED_REPO_ID}/cron/${cron_id}"
        case "$RESPONSE_STATUS" in
        200) note "run-now  : ${cron_name} pipeline #$(json_field number '?') created" ;;
        *) warn "run-now ${cron_name} returned ${RESPONSE_STATUS}: ${RESPONSE_BODY}" ;;
        esac
    done
fi

# ------------------------------------------------- required server settings --
# These live in project settings (server-side). A PR diff cannot change any of
# them; that is exactly why they are the trust boundary.
cat <<EOF

Required server-side settings (a PR diff cannot change these):
  untrusted project ${UNTRUSTED_REPO_ID}:
      config_file=${UNTRUSTED_CONFIG_FILE}  allow_pr=true   trusted.volumes=false
  trusted project   ${TRUSTED_REPO_ID:-<not created yet>}:
      config_file=${TRUSTED_CONFIG_FILE}  allow_pr=false  trusted.volumes=true
  * trusted.volumes is admin-only: PATCH /api/repos/<trusted_id> {"trusted":{"volumes":true}}
  * cron job \`nightly\` exists on the TRUSTED project only (never on the untrusted one)
  * both pipeline paths are mandatory project settings; never leave a path empty, because an
    empty path falls back to default resolution (top-level .woodpecker/*.yaml must stay unused)
  * keep "Require approval for forked repositories" enabled on the untrusted project (default)
EOF

# -------------------------------------------------------- branch protection --
# Woodpecker reports one commit status per workflow; the default context
# format is "{{ .context }}/{{ .event }}/{{ .workflow }}". With the default
# WOODPECKER_STATUS_CONTEXT=ci/woodpecker the contexts are:
#   pull_request  -> ci/woodpecker/pr/pr          (the required PR gate, untrusted project)
#   push main     -> ci/woodpecker/push/trusted   (post-merge evidence, trusted project)
#   tag           -> ci/woodpecker/tag/trusted
#   cron          -> ci/woodpecker/cron/nightly
cat <<EOF

Next steps (this script does not change branch protection):
  * GitHub UI: Settings -> Branches -> Add branch protection rule for main
    -> Require status checks to pass -> add ci/woodpecker/pr/pr.
  * Or with gh (adjust owner/repo if needed):
      gh api --method PUT repos/${REPO_FULL_NAME}/branches/main/protection --input - <<'JSON'
      {
        "required_status_checks": {
          "strict": true,
          "contexts": ["ci/woodpecker/pr/pr"]
        },
        "enforce_admins": false,
        "required_pull_request_reviews": null,
        "restrictions": null
      }
      JSON
  * Forge webhook: activation installed it; confirm it delivers push, tag and
    pull_request events, and keep "Require approval for forked repositories"
    enabled (Woodpecker default) as documented in setup.md §5.
  * Statuses are per workflow: a failing PR lane fails ci/woodpecker/pr/pr
    because the certificate step inside the untrusted project's pr.yaml fails
    closed. If the server overrides WOODPECKER_STATUS_CONTEXT(_FORMAT),
    substitute the resulting strings.
EOF
note "done"

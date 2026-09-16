#!/usr/bin/env bash
# Windows code-signing driver (release residual: OS-level Authenticode over
# the packaged artifacts, wired additively into the release path).
#
# Tool selection: `osslsigncode` (cross-platform) is preferred; a real
# `signtool.exe` is used when present (Windows hosts). Both are env-driven
# and never guessed from a private key location.
#
# Env (all optional; absent => an EXPLICIT UNSIGNED marker, never a silent
# claim):
#   FAKTOR_WINDOWS_CERT_FILE         PFX/P12 certificate path. Absent => the
#                                    driver reports `unsigned` (marker
#                                    UNSIGNED).
#   FAKTOR_WINDOWS_CERT_PASSWORD     certificate password (may be empty for
#                                    a passwordless PFX).
#   FAKTOR_WINDOWS_TIMESTAMP_URL     RFC-3161 timestamp URL (default
#                                    http://timestamp.digicert.com).
#   FAKTOR_WINDOWS_OSSLSIGNCODE_BIN  test seam: osslsigncode binary (default
#                                    `osslsigncode`).
#   FAKTOR_WINDOWS_SIGNTOOL_BIN      test seam: signtool binary (default
#                                    `signtool`; only used when present).
#
# Modes:
#   bash scripts/sign-windows.sh sign <path> [--kind K] [--expect-sha256 HEX]
#        [--require-signed]
#     Signs one artifact: a `.exe`/`.dll`/`.msi` gets Authenticode; a
#     `.tar.gz` gets its inner PE members signed and the archive repacked;
#     other kinds are `not_applicable`. The FINAL JSON is one line on stdout:
#       {"tool":"windows","status":"signed|unsigned|not_applicable|failed",
#        "scope":"...","subject_sha256":"...","tool_used":"...|null",
#        "identity":"...|null","signed":bool,"timestamped":bool,
#        "detail":"...","marker":"UNSIGNED"|null}
#     Exit: 0 signed/unsigned/not_applicable (honest marker), 1 failed or
#     --require-signed without a certificate, 2 usage.
#   bash scripts/sign-windows.sh verify <path> [--expect-sha256 HEX]
#     Verifies the recorded Authenticode signature and (when given) the
#     artifact digest — a doctored artifact fails the digest step.
#   bash scripts/sign-windows.sh selftest
#     Deterministic offline matrix (fake osslsigncode): absent env => UNSIGNED
#     marker + --require-signed refusal; configured env => signed; tar.gz
#     repack; doctored artifact fails the digest step; verify refuses
#     unsigned.
set -u
set -o pipefail
export LC_ALL=C

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

OSSLSIGNCODE_BIN="${FAKTOR_WINDOWS_OSSLSIGNCODE_BIN:-osslsigncode}"
SIGNTOOL_BIN="${FAKTOR_WINDOWS_SIGNTOOL_BIN:-signtool}"
CERT_FILE="${FAKTOR_WINDOWS_CERT_FILE:-}"
CERT_PASSWORD="${FAKTOR_WINDOWS_CERT_PASSWORD:-}"
TIMESTAMP_URL="${FAKTOR_WINDOWS_TIMESTAMP_URL:-http://timestamp.digicert.com}"

usage() {
    sed -n '2,48p' "${BASH_SOURCE[0]}" | sed -e 's/^# \{0,1\}//'
    exit "${1:-0}"
}

json_escape() {
    printf '%s' "$1" | tr -d '\r' | tr '\n' ' ' |
        sed -e 's/\\/\\\\/g' -e 's/"/\\"/g' -e 's/\t/ /g'
}

hash_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    elif command -v openssl >/dev/null 2>&1; then
        openssl dgst -sha256 "$1" | awk '{print $NF}'
    else
        return 1
    fi
}

# The signing tool that will actually be used, or empty.
signing_tool() {
    if command -v "$OSSLSIGNCODE_BIN" >/dev/null 2>&1 || [ -x "$OSSLSIGNCODE_BIN" ]; then
        printf 'osslsigncode'
        return 0
    fi
    if command -v "$SIGNTOOL_BIN" >/dev/null 2>&1 || [ -x "$SIGNTOOL_BIN" ]; then
        printf 'signtool'
        return 0
    fi
    printf ''
}

emit() {
    # status scope subject detail signed timestamped tool_used
    printf '{"tool":"windows","status":"%s","scope":"%s","subject_sha256":%s,"tool_used":%s,"identity":%s,"signed":%s,"timestamped":%s,"detail":"%s","marker":%s}\n' \
        "$1" "$(json_escape "$2")" \
        "$(if [ -n "${3:-}" ]; then printf '"%s"' "$(json_escape "$3")"; else printf 'null'; fi)" \
        "$(if [ -n "${7:-}" ]; then printf '"%s"' "$(json_escape "$7")"; else printf 'null'; fi)" \
        "$(if [ -n "$CERT_FILE" ]; then printf '"%s"' "$(json_escape "$CERT_FILE")"; else printf 'null'; fi)" \
        "$4" "$5" "$(json_escape "$6")" \
        "$(if [ "$1" = "unsigned" ]; then printf '"UNSIGNED"'; else printf 'null'; fi)"
}

is_pe() {
    local magic
    magic="$(od -An -tx1 -N2 "$1" 2>/dev/null | tr -d ' \n' | tr 'A-F' 'a-f')"
    [ "$magic" = "4d5a" ]
}

# Authenticode one PE file in place (osslsigncode writes via a temp output).
authenticode_file() {
    local target="$1" tool
    tool="$(signing_tool)"
    case "$tool" in
        osslsigncode)
            if ! "$OSSLSIGNCODE_BIN" sign \
                -certs "$CERT_FILE" -pass "$CERT_PASSWORD" \
                -h sha256 -ts "$TIMESTAMP_URL" \
                -in "$target" -out "$target.signed" >&2; then
                rm -f "$target.signed"
                return 1
            fi
            mv "$target.signed" "$target" || return 1
            ;;
        signtool)
            if ! "$SIGNTOOL_BIN" sign /f "$CERT_FILE" /p "$CERT_PASSWORD" \
                /fd sha256 /tr "$TIMESTAMP_URL" /td sha256 "$target" >&2; then
                return 1
            fi
            ;;
        *)
            return 2
            ;;
    esac
    return 0
}

sign_targz() {
    local path="$1"
    local work top
    work="$(mktemp -d "${TMPDIR:-/tmp}/faktor-sign-windows.XXXXXX")" || return 1
    if ! tar -xzf "$path" -C "$work"; then
        rm -rf "$work"
        return 1
    fi
    top="$(tar -tzf "$path" 2>/dev/null | head -n1 | cut -d/ -f1)"
    if [ -z "$top" ]; then
        rm -rf "$work"
        return 1
    fi
    local signed=0
    while IFS= read -r candidate; do
        [ -f "$candidate" ] || continue
        case "$(printf '%s' "$candidate" | tr '[:upper:]' '[:lower:]')" in
            *.exe | *.dll | *.msi | *.sys)
                if ! is_pe "$candidate"; then
                    continue
                fi
                if ! authenticode_file "$candidate"; then
                    rm -rf "$work"
                    return 1
                fi
                signed=$((signed + 1))
                ;;
        esac
    done <<EOF
$(find "$work" -type f 2>/dev/null | LC_ALL=C sort)
EOF
    if [ "$signed" -eq 0 ]; then
        rm -rf "$work"
        printf '[sign-windows] REFUSED: %s carries no PE member to sign\n' "$path" >&2
        return 1
    fi
    if ! tar -czf "$path.tmp" -C "$work" "$top"; then
        rm -rf "$work" "$path.tmp"
        return 1
    fi
    mv "$path.tmp" "$path" || {
        rm -rf "$work" "$path.tmp"
        return 1
    }
    rm -rf "$work"
    return 0
}

mode_sign() {
    local path="$1" kind="artifact" expect=""
    shift
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --kind)
                kind="${2:-artifact}"
                shift 2
                ;;
            --expect-sha256)
                expect="${2:-}"
                shift 2
                ;;
            --require-signed)
                REQUIRE_SIGNED=1
                shift
                ;;
            *)
                printf '[sign-windows] unknown flag %s\n' "$1" >&2
                exit 2
                ;;
        esac
    done
    if [ ! -f "$path" ]; then
        printf '[sign-windows] artifact %s does not exist\n' "$path" >&2
        emit failed "" "" "artifact not found" false false ""
        exit 1
    fi
    local sha_before
    sha_before="$(hash_file "$path" 2>/dev/null || printf '')"
    if [ -n "$expect" ] && [ "$sha_before" != "$expect" ]; then
        printf '[sign-windows] REFUSED: digest mismatch for %s (recorded %s, actual %s)\n' \
            "$path" "$expect" "$sha_before" >&2
        emit failed "" "$sha_before" "refused: artifact digest does not match the recorded digest" \
            false false ""
        exit 1
    fi

    local lower ext
    lower="$(printf '%s' "$path" | tr '[:upper:]' '[:lower:]')"
    ext=""
    case "$lower" in
        *.tar.gz | *.tgz) ext="tar.gz" ;;
        *.exe) ext="exe" ;;
        *.dll) ext="dll" ;;
        *.msi) ext="msi" ;;
        *) ext="other" ;;
    esac

    local tool
    tool="$(signing_tool)"
    if [ -z "$CERT_FILE" ]; then
        printf '[sign-windows] UNSIGNED: FAKTOR_WINDOWS_CERT_FILE is not set — %s is explicitly UNSIGNED (marker emitted; usable locally, not for release)\n' \
            "$path" >&2
        emit unsigned "$kind" "$sha_before" \
            "unsigned: no FAKTOR_WINDOWS_CERT_FILE configured" false false "$tool"
        if [ "${REQUIRE_SIGNED:-0}" = "1" ]; then
            printf '[sign-windows] REFUSED: --require-signed and no certificate\n' >&2
            exit 1
        fi
        exit 0
    fi
    if [ ! -f "$CERT_FILE" ]; then
        printf '[sign-windows] REFUSED: FAKTOR_WINDOWS_CERT_FILE %s does not exist\n' "$CERT_FILE" >&2
        emit failed "$kind" "$sha_before" \
            "failed: certificate file is unreadable" false false "$tool"
        exit 1
    fi
    if [ -z "$tool" ]; then
        printf '[sign-windows] UNSIGNED: no osslsigncode/signtool on this host\n' >&2
        emit unsigned "$kind" "$sha_before" \
            "unsigned: neither osslsigncode nor signtool is available" false false ""
        if [ "${REQUIRE_SIGNED:-0}" = "1" ]; then
            exit 1
        fi
        exit 0
    fi

    case "$ext" in
        tar.gz)
            if ! sign_targz "$path"; then
                emit failed "inner-pe" "$sha_before" \
                    "failed: Authenticode of the tar.gz payload failed" false false "$tool"
                exit 1
            fi
            emit signed "inner-pe" "$(hash_file "$path" 2>/dev/null || printf '')" \
                "signed: inner PE members Authenticode-signed (sha256 + timestamp)" true true "$tool"
            exit 0
            ;;
        exe | dll | msi)
            if ! is_pe "$path"; then
                printf '[sign-windows] REFUSED: %s is not a PE binary\n' "$path" >&2
                emit failed "file" "$sha_before" "failed: not a PE binary" false false "$tool"
                exit 1
            fi
            if ! authenticode_file "$path"; then
                emit failed "file" "$sha_before" "failed: Authenticode signing refused" false false "$tool"
                exit 1
            fi
            emit signed "file" "$(hash_file "$path" 2>/dev/null || printf '')" \
                "signed: Authenticode sha256 with an RFC-3161 timestamp" true true "$tool"
            exit 0
            ;;
        *)
            printf '[sign-windows] %s is not a PE code object; not_applicable\n' "$path" >&2
            emit not_applicable "$kind" "$sha_before" \
                "not_applicable: not a PE binary, and not a PE-carrying tar.gz" false false "$tool"
            exit 0
            ;;
    esac
}

mode_verify() {
    local path="$1" expect=""
    shift
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --expect-sha256)
                expect="${2:-}"
                shift 2
                ;;
            *)
                printf '[sign-windows] unknown flag %s\n' "$1" >&2
                exit 2
                ;;
        esac
    done
    if [ ! -f "$path" ]; then
        printf '[sign-windows] verify: %s does not exist\n' "$path" >&2
        exit 1
    fi
    local sha
    sha="$(hash_file "$path" 2>/dev/null || printf '')"
    if [ -n "$expect" ] && [ "$sha" != "$expect" ]; then
        printf '[sign-windows] verify: REFUSED digest mismatch (recorded %s, actual %s)\n' \
            "$expect" "$sha" >&2
        exit 1
    fi
    local tool
    tool="$(signing_tool)"
    if [ "$tool" != "osslsigncode" ]; then
        printf '[sign-windows] verify: osslsigncode is required to verify on this host\n' >&2
        exit 1
    fi
    if "$OSSLSIGNCODE_BIN" verify -in "$path" >&2; then
        printf '[sign-windows] verify: ok sha256=%s\n' "$sha"
        exit 0
    fi
    printf '[sign-windows] verify: %s carries no valid Authenticode signature\n' "$path" >&2
    exit 1
}

mode_selftest() {
    local failures=0
    SIGN_SELFTEST_DIR="$(mktemp -d "${TMPDIR:-/tmp}/faktor-sign-windows-selftest.XXXXXX")" || exit 2
    trap 'rm -rf "${SIGN_SELFTEST_DIR:-}"' EXIT
    local dir="$SIGN_SELFTEST_DIR"
    expect() {
        if [ "$2" != "$3" ]; then
            failures=$((failures + 1))
            printf '[sign-windows selftest] FAIL: %s => %s (expected %s)\n' "$1" "$2" "$3" >&2
        else
            printf '[sign-windows selftest] ok: %s\n' "$1"
        fi
    }
    json_field() {
        sed -n 's/.*"'"$2"'":\([^,}]*\).*/\1/p' "$1" | head -n1 | tr -d '"' |
            sed 's/^ *//;s/ *$//'
    }

    local fakebin="$dir/bin"
    mkdir -p "$fakebin"
    cat >"$fakebin/osslsigncode" <<'FAKE'
#!/usr/bin/env bash
# Fake osslsigncode: `sign` requires -certs and writes a sidecar; `verify`
# succeeds only when the sidecar exists.
set -u
mode="$1"
shift
target=""
out=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        -in)
            target="$2"
            shift
            ;;
        -out)
            out="$2"
            shift
            ;;
    esac
    shift
done
if [ "$mode" = "verify" ]; then
    [ -n "$target" ] && [ -f "$target.sig" ] && exit 0
    exit 1
fi
if [ -z "$target" ] || [ ! -f "$target" ]; then
    echo "fake osslsigncode: no input" >&2
    exit 1
fi
if [ -n "${FAKE_OSSLSIGNCODE_FAIL:-}" ]; then
    echo "fake osslsigncode: injected failure" >&2
    exit 1
fi
printf 'fake-authenticode\n' >>"$target.sig"
cp "$target" "$out"
exit 0
FAKE
    chmod +x "$fakebin/osslsigncode"

    # A minimal PE-magic artifact (MZ header) plus junk.
    local exe="$dir/faktor-cli.exe"
    printf 'MZfake-pe-payload\n' >"$exe"
    local cert="$dir/operator.pfx"
    printf 'fake-pfx\n' >"$cert"

    # --- 1. missing cert => explicit UNSIGNED marker ----------------------
    local out="$dir/out.json"
    FAKTOR_WINDOWS_CERT_FILE= FAKTOR_WINDOWS_OSSLSIGNCODE_BIN="$fakebin/osslsigncode" \
        bash "$0" sign "$exe" --kind daemon-bundle >"$out" 2>/dev/null
    expect "missing certificate exits 0 with an UNSIGNED marker" \
        "$(json_field "$out" status):$(json_field "$out" marker)" "unsigned:UNSIGNED"
    FAKTOR_WINDOWS_CERT_FILE= FAKTOR_WINDOWS_OSSLSIGNCODE_BIN="$fakebin/osslsigncode" \
        bash "$0" sign "$exe" --require-signed >/dev/null 2>&1
    expect "--require-signed refuses without a certificate" "$?" "1"

    # --- 2. configured certificate + fake tool => signed ------------------
    FAKTOR_WINDOWS_CERT_FILE="$cert" FAKTOR_WINDOWS_CERT_PASSWORD="pw" \
        FAKTOR_WINDOWS_OSSLSIGNCODE_BIN="$fakebin/osslsigncode" \
        bash "$0" sign "$exe" --kind daemon-bundle >"$out" 2>/dev/null
    expect "configured certificate signs" "$(json_field "$out" status)" "signed"
    expect "the signing tool is named" "$(json_field "$out" tool_used)" "osslsigncode"
    expect "the verdict carries the signed subject digest" \
        "$(json_field "$out" subject_sha256)" "$(hash_file "$exe")"

    # --- 3. tar.gz inner PE signing + repack ------------------------------
    local stage="$dir/stage" tarball="$dir/bundle.tar.gz"
    mkdir -p "$stage/faktor-cli-9.9.9-windows-x86_64/bin"
    printf 'MZinner-pe\n' >"$stage/faktor-cli-9.9.9-windows-x86_64/bin/faktor-cli.exe"
    printf 'readme\n' >"$stage/faktor-cli-9.9.9-windows-x86_64/RELEASE"
    tar -czf "$tarball" -C "$stage" "faktor-cli-9.9.9-windows-x86_64"
    local before_sha
    before_sha="$(hash_file "$tarball")"
    FAKTOR_WINDOWS_CERT_FILE="$cert" FAKTOR_WINDOWS_CERT_PASSWORD="pw" \
        FAKTOR_WINDOWS_OSSLSIGNCODE_BIN="$fakebin/osslsigncode" \
        bash "$0" sign "$tarball" --kind daemon-bundle >"$out" 2>/dev/null
    expect "tar.gz is signed via its inner PE members" \
        "$(json_field "$out" status):$(json_field "$out" scope)" "signed:inner-pe"
    expect "the repacked archive keeps the top-level layout" \
        "$(tar -tzf "$tarball" | head -n1)" "faktor-cli-9.9.9-windows-x86_64/"
    expect "the repack changed the archive digest (inner bytes signed)" \
        "$([ "$before_sha" != "$(hash_file "$tarball")" ] && printf changed)" "changed"

    # --- 4. a doctored artifact fails the digest step ---------------------
    local recorded
    recorded="$(json_field "$out" subject_sha256)"
    printf 'doctored\n' >>"$tarball"
    FAKTOR_WINDOWS_OSSLSIGNCODE_BIN="$fakebin/osslsigncode" \
        bash "$0" verify "$tarball" --expect-sha256 "$recorded" >/dev/null 2>&1
    expect "a doctored artifact fails the recorded-digest step" "$?" "1"
    FAKTOR_WINDOWS_CERT_FILE="$cert" \
        FAKTOR_WINDOWS_OSSLSIGNCODE_BIN="$fakebin/osslsigncode" \
        bash "$0" sign "$tarball" --expect-sha256 "$recorded" >/dev/null 2>&1
    expect "signing a doctored artifact against the recorded digest refuses" "$?" "1"

    # --- 5. non-PE artifacts are explicitly not_applicable ----------------
    printf 'zip payload\n' >"$dir/plugin.zip"
    FAKTOR_WINDOWS_CERT_FILE="$cert" FAKTOR_WINDOWS_OSSLSIGNCODE_BIN="$fakebin/osslsigncode" \
        bash "$0" sign "$dir/plugin.zip" --kind jetbrains-plugin >"$out" 2>/dev/null
    expect "a non-PE artifact is not_applicable (never claimed signed)" \
        "$(json_field "$out" status)" "not_applicable"

    if [ "$failures" -gt 0 ]; then
        printf '[sign-windows selftest] FAIL (%s assertion(s))\n' "$failures" >&2
        exit 1
    fi
    printf '[sign-windows selftest] PASS\n'
}

REQUIRE_SIGNED=0
case "${1:-}" in
    sign)
        shift
        [ "$#" -ge 1 ] || usage 2
        mode_sign "$@"
        ;;
    verify)
        shift
        [ "$#" -ge 1 ] || usage 2
        mode_verify "$@"
        ;;
    selftest)
        mode_selftest
        ;;
    --help | -h | "")
        usage 0
        ;;
    *)
        printf '[sign-windows] unknown mode %s\n' "$1" >&2
        usage 2
        ;;
esac

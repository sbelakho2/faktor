#!/usr/bin/env bash
# macOS code-signing + notarization driver (release residual: the artifacts
# themselves were signed only by the update manifest — this signs the OS
# level: `codesign --options runtime`, `notarytool submit --wait`, `stapler
# staple`).
#
# Env (all optional; absent => an EXPLICIT UNSIGNED marker, never a silent
# claim):
#   FAKTOR_MACOS_SIGNING_IDENTITY  Developer ID identity ("Developer ID
#                                  Application: ..."). Absent => the driver
#                                  reports `unsigned` with marker UNSIGNED.
#   FAKTOR_MACOS_NOTARY_PROFILE    notarytool keychain profile. Absent =>
#                                  codesign only (`notarized:false`).
#   FAKTOR_MACOS_CODESIGN_BIN      test seam: codesign binary (default
#                                  `codesign`; selftest injects a fake).
#   FAKTOR_MACOS_XCRUN_BIN         test seam: xcrun binary for
#                                  notarytool/stapler (default `xcrun`).
#
# Modes:
#   bash scripts/sign-macos.sh sign <path> [--kind K] [--expect-sha256 HEX]
#        [--require-signed]
#     Signs one artifact. `.app`/`.dmg`/`.pkg` get codesign + notarize +
#     staple; a bare Mach-O executable gets codesign (a lone executable has
#     no notarizable container); a `.tar.gz` gets its inner Mach-O members
#     codesigned and the archive repacked (the archive layout is preserved),
#     then the payload is submitted to notarytool as a zip — tar.gz itself
#     cannot be stapled, which is reported honestly. `.zip`/unknown kinds
#     are `not_applicable` (an extension zip is not an OS code object).
#     The FINAL JSON is one line on stdout:
#       {"tool":"macos","status":"signed|unsigned|not_applicable|failed",
#        "scope":"...","subject_sha256":"...","identity":"...|null",
#        "codesigned":bool,"notarized":bool,"stapled":bool,
#        "detail":"...","marker":"UNSIGNED"|null}
#     Exit: 0 signed/unsigned/not_applicable (honest marker), 1 failed or
#     --require-signed without a signing identity, 2 usage.
#   bash scripts/sign-macos.sh verify <path> [--expect-sha256 HEX]
#     Verifies the recorded signature (codesign --verify --deep --strict)
#     and, when --expect-sha256 is given, that the artifact still hashes to
#     the recorded digest (a doctored artifact fails here).
#   bash scripts/sign-macos.sh selftest
#     Deterministic offline matrix: absent env => UNSIGNED marker (and a
#     --require-signed refusal); fake tools + env => signed; tar.gz repack;
#     doctored artifact fails the digest step; verify refuses unsigned.
set -u
set -o pipefail
export LC_ALL=C

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

CODESIGN_BIN="${FAKTOR_MACOS_CODESIGN_BIN:-codesign}"
XCRUN_BIN="${FAKTOR_MACOS_XCRUN_BIN:-xcrun}"
IDENTITY="${FAKTOR_MACOS_SIGNING_IDENTITY:-}"
NOTARY_PROFILE="${FAKTOR_MACOS_NOTARY_PROFILE:-}"

usage() {
    sed -n '2,52p' "${BASH_SOURCE[0]}" | sed -e 's/^# \{0,1\}//'
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

# Emit the driver's one-line verdict. Every mode ends with exactly one call.
emit() {
    # status scope subject detail codesigned notarized stapled marker
    printf '{"tool":"macos","status":"%s","scope":"%s","subject_sha256":%s,"identity":%s,"codesigned":%s,"notarized":%s,"stapled":%s,"detail":"%s","marker":%s}\n' \
        "$1" "$(json_escape "$2")" \
        "$(if [ -n "${3:-}" ]; then printf '"%s"' "$(json_escape "$3")"; else printf 'null'; fi)" \
        "$(if [ -n "$IDENTITY" ]; then printf '"%s"' "$(json_escape "$IDENTITY")"; else printf 'null'; fi)" \
        "$4" "$5" "$6" "$(json_escape "$7")" \
        "$(if [ "$1" = "unsigned" ]; then printf '"UNSIGNED"'; else printf 'null'; fi)"
}

is_macho() {
    if command -v file >/dev/null 2>&1; then
        file -b "$1" 2>/dev/null | grep -q 'Mach-O'
        return $?
    fi
    # Fallback: the Mach-O magic numbers (32/64/fat, both endiannesses).
    local magic
    magic="$(od -An -tx1 -N4 "$1" 2>/dev/null | tr -d ' \n' | tr 'A-F' 'a-f')"
    case "$magic" in
        feedface | feedfacf | cafebabe | cefaedfe | cffaedfe | bebafeca) return 0 ;;
        *) return 1 ;;
    esac
}

# codesign one file; a refusal is a hard failure (never a silent marker).
codesign_file() {
    local target="$1"
    if ! "$CODESIGN_BIN" --force --options runtime --timestamp --sign "$IDENTITY" "$target" \
        >&2; then
        return 1
    fi
    return 0
}

# Notarize one container (zip/dmg/pkg) and optionally staple it.
# Returns 0 only on an Accepted notarization verdict.
notarize_container() {
    local container="$1" staple_target="${2:-}"
    [ -n "$NOTARY_PROFILE" ] || return 3
    if ! command -v "$XCRUN_BIN" >/dev/null 2>&1 && [ ! -x "$XCRUN_BIN" ]; then
        return 4
    fi
    local out
    out="$("$XCRUN_BIN" notarytool submit "$container" \
        --keychain-profile "$NOTARY_PROFILE" --output-format json --wait 2>&2)" || return 1
    printf '%s' "$out" | grep -q '"status"[[:space:]]*:[[:space:]]*"Accepted"' || return 1
    if [ -n "$staple_target" ]; then
        "$XCRUN_BIN" stapler staple "$staple_target" >&2 || return 2
    fi
    return 0
}

# Code-sign every Mach-O member of one extracted tree and repack the tar.gz
# at its original path with its original top-level layout.
sign_targz() {
    local path="$1"
    local work top name unzipped
    work="$(mktemp -d "${TMPDIR:-/tmp}/faktor-sign-macos.XXXXXX")" || return 1
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
        if is_macho "$candidate"; then
            if ! codesign_file "$candidate"; then
                rm -rf "$work"
                return 1
            fi
            signed=$((signed + 1))
            name="${candidate#"$work"/}"
            printf '[sign-macos] codesigned %s\n' "$name" >&2
        fi
    done <<EOF
$(find "$work" -type f 2>/dev/null | LC_ALL=C sort)
EOF
    if [ "$signed" -eq 0 ]; then
        rm -rf "$work"
        printf '[sign-macos] REFUSED: %s carries no Mach-O member to codesign\n' "$path" >&2
        return 1
    fi
    # Repack the SAME top-level layout (checksums.txt inside the bundle was
    # hashed before signing, so the bundle payload is re-derived here).
    if ! tar -czf "$path.tmp" -C "$work" "$top"; then
        rm -rf "$work" "$path.tmp"
        return 1
    fi
    mv "$path.tmp" "$path" || {
        rm -rf "$work" "$path.tmp"
        return 1
    }
    # Notarize the payload as a zip (notarytool accepts zip/pkg/dmg). A tar.gz
    # cannot be stapled; the online ticket still covers the signed members.
    if [ -n "$NOTARY_PROFILE" ]; then
        unzipped="$(mktemp -d "${TMPDIR:-/tmp}/faktor-sign-notary.XXXXXX")" || {
            rm -rf "$work"
            return 1
        }
        if (cd "$work" && zip -qry "$unzipped/payload.zip" .); then
            notarize_container "$unzipped/payload.zip" ""
            local verdict=$?
            rm -rf "$work" "$unzipped"
            return "$verdict"
        fi
        rm -rf "$work" "$unzipped"
        return 1
    fi
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
                printf '[sign-macos] unknown flag %s\n' "$1" >&2
                exit 2
                ;;
        esac
    done
    if [ ! -f "$path" ]; then
        printf '[sign-macos] artifact %s does not exist\n' "$path" >&2
        emit failed "" "" "" false false false "artifact not found"
        exit 1
    fi
    local sha_before
    sha_before="$(hash_file "$path" 2>/dev/null || printf '')"
    if [ -n "$expect" ] && [ "$sha_before" != "$expect" ]; then
        printf '[sign-macos] REFUSED: digest mismatch for %s (recorded %s, actual %s)\n' \
            "$path" "$expect" "$sha_before" >&2
        emit failed "" "$sha_before" false false false \
            "refused: artifact digest $sha_before does not match the recorded $expect"
        exit 1
    fi

    local lower ext
    lower="$(printf '%s' "$path" | tr '[:upper:]' '[:lower:]')"
    ext=""
    case "$lower" in
        *.tar.gz | *.tgz) ext="tar.gz" ;;
        *.app) ext="app" ;;
        *.dmg) ext="dmg" ;;
        *.pkg) ext="pkg" ;;
        *.zip) ext="zip" ;;
    esac

    if [ -z "$IDENTITY" ]; then
        printf '[sign-macos] UNSIGNED: FAKTOR_MACOS_SIGNING_IDENTITY is not set — %s is explicitly UNSIGNED (marker emitted; usable locally, not for release)\n' \
            "$path" >&2
        emit unsigned "$kind" "$sha_before" false false false \
            "unsigned: no FAKTOR_MACOS_SIGNING_IDENTITY configured"
        if [ "${REQUIRE_SIGNED:-0}" = "1" ]; then
            printf '[sign-macos] REFUSED: --require-signed and no signing identity\n' >&2
            exit 1
        fi
        exit 0
    fi
    if ! command -v "$CODESIGN_BIN" >/dev/null 2>&1 && [ ! -x "$CODESIGN_BIN" ]; then
        printf '[sign-macos] UNSIGNED: codesign is unavailable on this host\n' >&2
        emit unsigned "$kind" "$sha_before" false false false \
            "unsigned: codesign binary '$CODESIGN_BIN' is unavailable"
        if [ "${REQUIRE_SIGNED:-0}" = "1" ]; then
            exit 1
        fi
        exit 0
    fi

    case "$ext" in
        zip)
            printf '[sign-macos] %s is a distribution archive, not an OS code object; not_applicable\n' \
                "$path" >&2
            emit not_applicable "$kind" "$sha_before" false false false \
                "not_applicable: an extension zip is not codesigned/notarized"
            exit 0
            ;;
        tar.gz)
            if ! sign_targz "$path"; then
                emit failed "inner-macho" "$sha_before" false false false \
                    "failed: codesign/notarization of the tar.gz payload failed"
                exit 1
            fi
            local sha_after
            sha_after="$(hash_file "$path" 2>/dev/null || printf '')"
            local notarized=false stapled=false detail
            if [ -n "$NOTARY_PROFILE" ]; then
                notarized=true
                stapled=false
                detail="signed: inner Mach-O members codesigned with runtime options; payload notarized (tar.gz cannot be stapled; the ticket is checked online)"
            else
                detail="signed: inner Mach-O members codesigned with runtime options; notarization skipped (no FAKTOR_MACOS_NOTARY_PROFILE)"
            fi
            emit signed "inner-macho" "$sha_after" true "$notarized" "$stapled" "$detail"
            exit 0
            ;;
        app | dmg | pkg)
            if ! codesign_file "$path"; then
                emit failed "$scope" "$sha_before" false false false "failed: codesign refused $path"
                exit 1
            fi
            ;;
        *)
            if ! codesign_file "$path"; then
                emit failed "file" "$sha_before" false false false "failed: codesign refused $path"
                exit 1
            fi
            ;;
    esac

    # Containers that support a staple: notarize then staple in place.
    if [ "$ext" = "app" ] || [ "$ext" = "dmg" ] || [ "$ext" = "pkg" ]; then
        if [ -n "$NOTARY_PROFILE" ]; then
            if ! notarize_container "$path" "$path"; then
                emit failed "file" "$(hash_file "$path" 2>/dev/null || printf '')" true true false \
                    "failed: notarytool did not accept $path or stapling failed"
                exit 1
            fi
            emit signed "file" "$(hash_file "$path" 2>/dev/null || printf '')" true true true \
                "signed: codesign (runtime options) + notarytool Accepted + stapled"
            exit 0
        fi
        emit signed "file" "$(hash_file "$path" 2>/dev/null || printf '')" true false false \
            "signed: codesign (runtime options); notarization skipped (no FAKTOR_MACOS_NOTARY_PROFILE)"
        exit 0
    fi
    # A bare Mach-O executable: codesign only (there is no notarizable
    # container; notarytool requires a zip/pkg/dmg).
    emit signed "file" "$(hash_file "$path" 2>/dev/null || printf '')" true false false \
        "signed: codesign (runtime options); a lone executable has no notarizable container"
    exit 0
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
                printf '[sign-macos] unknown flag %s\n' "$1" >&2
                exit 2
                ;;
        esac
    done
    if [ ! -f "$path" ]; then
        printf '[sign-macos] verify: %s does not exist\n' "$path" >&2
        exit 1
    fi
    local sha
    sha="$(hash_file "$path" 2>/dev/null || printf '')"
    if [ -n "$expect" ] && [ "$sha" != "$expect" ]; then
        printf '[sign-macos] verify: REFUSED digest mismatch (recorded %s, actual %s)\n' \
            "$expect" "$sha" >&2
        exit 1
    fi
    if ! command -v "$CODESIGN_BIN" >/dev/null 2>&1 && [ ! -x "$CODESIGN_BIN" ]; then
        printf '[sign-macos] verify: codesign unavailable; cannot verify a signature\n' >&2
        exit 1
    fi
    if "$CODESIGN_BIN" --verify --deep --strict "$path" >&2; then
        printf '[sign-macos] verify: ok sha256=%s\n' "$sha"
        exit 0
    fi
    printf '[sign-macos] verify: %s carries no valid signature\n' "$path" >&2
    exit 1
}

mode_selftest() {
    local failures=0
    SIGN_SELFTEST_DIR="$(mktemp -d "${TMPDIR:-/tmp}/faktor-sign-macos-selftest.XXXXXX")" || exit 2
    trap 'rm -rf "${SIGN_SELFTEST_DIR:-}"' EXIT
    local dir="$SIGN_SELFTEST_DIR"
    expect() {
        if [ "$2" != "$3" ]; then
            failures=$((failures + 1))
            printf '[sign-macos selftest] FAIL: %s => %s (expected %s)\n' "$1" "$2" "$3" >&2
        else
            printf '[sign-macos selftest] ok: %s\n' "$1"
        fi
    }
    json_field() {
        # json_field <file> <key> — flat string/bool/null field of the verdict
        sed -n 's/.*"'"$2"'":\([^,}]*\).*/\1/p' "$1" | head -n1 | tr -d '"' |
            sed 's/^ *//;s/ *$//'
    }

    # --- fake tools -------------------------------------------------------
    local fakebin="$dir/bin"
    mkdir -p "$fakebin"
    cat >"$fakebin/codesign" <<'FAKE'
#!/usr/bin/env bash
# Fake codesign: records the sign call and creates a sidecar signature.
set -u
mode=""
target=""
verify=0
while [ "$#" -gt 0 ]; do
    case "$1" in
        --force | --timestamp) ;;
        --verify)
            verify=1
            ;;
        --deep | --strict) ;;
        --options)
            mode="$2"
            shift
            ;;
        --sign)
            shift
            ;;
        *)
            target="$1"
            ;;
    esac
    shift
done
if [ -z "$target" ] || [ ! -e "$target" ]; then
    echo "fake codesign: no target" >&2
    exit 1
fi
if [ "$verify" = "1" ]; then
    [ -f "$target.sig" ] || exit 1
    exit 0
fi
printf 'fake-signature options=%s\n' "$mode" >>"$target.sig"
if [ -n "${FAKE_CODESIGN_FAIL:-}" ]; then
    echo "fake codesign: injected failure" >&2
    exit 1
fi
exit 0
FAKE
    chmod +x "$fakebin/codesign"
    cat >"$fakebin/xcrun" <<'FAKE'
#!/usr/bin/env bash
# Fake xcrun: accepts notarytool submissions and staples.
set -u
if [ "${1:-}" = "notarytool" ]; then
    if [ -n "${FAKE_NOTARY_FAIL:-}" ]; then
        printf '{"status":"Invalid"}\n'
        exit 1
    fi
    for arg in "$@"; do :; done
    printf '{"id":"fake","status":"Accepted","message":"Successfully uploaded"}\n'
    exit 0
fi
if [ "${1:-}" = "stapler" ] && [ "${2:-}" = "staple" ]; then
    printf 'The staple and validate action worked!\n'
    exit 0
fi
echo "fake xcrun: unsupported $*" >&2
exit 1
FAKE
    chmod +x "$fakebin/xcrun"

    # A plain Mach-O-magic executable artifact.
    local exe="$dir/faktor-cli"
    printf '\317\372\355\376fake-macho-payload\n' >"$exe"
    chmod +x "$exe"

    # --- 1. missing env => explicit UNSIGNED marker ------------------------
    local out="$dir/out.json"
    FAKTOR_MACOS_SIGNING_IDENTITY= FAKTOR_MACOS_NOTARY_PROFILE= \
        bash "$0" sign "$exe" --kind daemon-bundle >"$out" 2>/dev/null
    expect "missing identity exits 0 with an UNSIGNED marker" \
        "$(json_field "$out" status):$(json_field "$out" marker)" "unsigned:UNSIGNED"
    expect "missing identity is never codesigned" \
        "$(json_field "$out" codesigned)" "false"
    # --require-signed turns the same absence into a loud refusal.
    FAKTOR_MACOS_SIGNING_IDENTITY= FAKTOR_MACOS_NOTARY_PROFILE= \
        bash "$0" sign "$exe" --require-signed >/dev/null 2>&1
    expect "--require-signed refuses without an identity" "$?" "1"
    # verify refuses an unsigned artifact.
    FAKTOR_MACOS_CODESIGN_BIN="$fakebin/codesign" \
        bash "$0" verify "$exe" >/dev/null 2>&1
    expect "verify refuses an unsigned artifact" "$?" "1"

    # --- 2. identity + fake tools => signed -------------------------------
    FAKTOR_MACOS_SIGNING_IDENTITY="Developer ID Application: Test (TEAMID)" \
        FAKTOR_MACOS_NOTARY_PROFILE="notary-profile" \
        FAKTOR_MACOS_CODESIGN_BIN="$fakebin/codesign" \
        FAKTOR_MACOS_XCRUN_BIN="$fakebin/xcrun" \
        bash "$0" sign "$exe" --kind daemon-bundle >"$out" 2>/dev/null
    expect "configured identity signs" "$(json_field "$out" status)" "signed"
    expect "runtime options are passed to codesign" \
        "$(grep -c 'options=runtime' "$exe.sig" 2>/dev/null || printf 0)" "1"
    expect "the verdict carries the signed subject digest" \
        "$(json_field "$out" subject_sha256)" "$(hash_file "$exe")"

    # --- 3. tar.gz inner signing + repack + notarization ------------------
    local stage="$dir/stage" tarball="$dir/bundle.tar.gz"
    mkdir -p "$stage/faktor-cli-9.9.9-darwin-arm64/bin"
    printf '\317\372\355\376inner-macho\n' >"$stage/faktor-cli-9.9.9-darwin-arm64/bin/faktor-cli"
    printf 'not macho\n' >"$stage/faktor-cli-9.9.9-darwin-arm64/RELEASE"
    tar -czf "$tarball" -C "$stage" "faktor-cli-9.9.9-darwin-arm64"
    local before_sha
    before_sha="$(hash_file "$tarball")"
    FAKTOR_MACOS_SIGNING_IDENTITY="Developer ID Application: Test (TEAMID)" \
        FAKTOR_MACOS_NOTARY_PROFILE="notary-profile" \
        FAKTOR_MACOS_CODESIGN_BIN="$fakebin/codesign" \
        FAKTOR_MACOS_XCRUN_BIN="$fakebin/xcrun" \
        bash "$0" sign "$tarball" --kind daemon-bundle >"$out" 2>/dev/null
    expect "tar.gz is signed via its inner Mach-O members" \
        "$(json_field "$out" status):$(json_field "$out" scope)" "signed:inner-macho"
    expect "tar.gz notarization is attempted" "$(json_field "$out" notarized)" "true"
    expect "tar.gz is explicitly not stapled" "$(json_field "$out" stapled)" "false"
    expect "the repacked archive keeps the top-level layout" \
        "$(tar -tzf "$tarball" | head -n1)" "faktor-cli-9.9.9-darwin-arm64/"
    expect "the repack changed the archive digest (inner bytes signed)" \
        "$([ "$before_sha" != "$(hash_file "$tarball")" ] && printf changed)" "changed"
    expect "the inner binary carries the fake signature" \
        "$(tar -xzOf "$tarball" faktor-cli-9.9.9-darwin-arm64/bin/faktor-cli.sig >/dev/null 2>&1 && printf yes || printf no)" "yes"

    # --- 4. a doctored artifact fails the digest step ---------------------
    local recorded
    recorded="$(json_field "$out" subject_sha256)"
    printf 'doctored\n' >>"$tarball"
    FAKTOR_MACOS_CODESIGN_BIN="$fakebin/codesign" \
        bash "$0" verify "$tarball" --expect-sha256 "$recorded" >/dev/null 2>&1
    expect "a doctored artifact fails the recorded-digest step" "$?" "1"
    FAKTOR_MACOS_SIGNING_IDENTITY="Developer ID Application: Test (TEAMID)" \
        FAKTOR_MACOS_CODESIGN_BIN="$fakebin/codesign" \
        bash "$0" sign "$tarball" --expect-sha256 "$recorded" --require-signed >/dev/null 2>&1
    expect "signing a doctored artifact against the recorded digest refuses" "$?" "1"

    # --- 5. zip artifacts are explicitly not_applicable -------------------
    printf 'zip payload\n' >"$dir/plugin.zip"
    FAKTOR_MACOS_SIGNING_IDENTITY="Developer ID Application: Test (TEAMID)" \
        FAKTOR_MACOS_CODESIGN_BIN="$fakebin/codesign" \
        bash "$0" sign "$dir/plugin.zip" --kind jetbrains-plugin >"$out" 2>/dev/null
    expect "an extension zip is not_applicable (never claimed signed)" \
        "$(json_field "$out" status)" "not_applicable"

    if [ "$failures" -gt 0 ]; then
        printf '[sign-macos selftest] FAIL (%s assertion(s))\n' "$failures" >&2
        exit 1
    fi
    printf '[sign-macos selftest] PASS\n'
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
        printf '[sign-macos] unknown mode %s\n' "$1" >&2
        usage 2
        ;;
esac

#!/bin/sh
# CI image pin gate (Phase E item 12).
#
# Every container image referenced by a Woodpecker workflow MUST be pinned by
# a multi-arch index digest: `image:tag@sha256:<64 lowercase hex>` (the
# trailing comment records the original tag and the recording date). A bare
# tag is mutable, so a compromised or retagged upstream image would silently
# change what CI runs.
#
# The one documented exemption is the Windows self-hosted shell image
# (`image: powershell`): Woodpecker's Windows backend does not pull it as a
# container and no `docker.io` library image exists to record a digest from.
# Such a line MUST carry an explicit inline annotation:
#
#   image: powershell # digest-exempt: <reason>
#
# Anything else missing `@sha256:` is a violation.
#
# Usage:
#   sh scripts/check-ci-image-pins.sh [--dir DIR]
#   sh scripts/check-ci-image-pins.sh --selftest
#
# Exit codes: 0 pass; 1 violation; 2 usage error.
set -u

SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
ROOT="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)"
DIR="$ROOT/.woodpecker"
SELFTEST=0

usage() {
    sed -n '2,/^set -/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'
}

while [ "$#" -gt 0 ]; do
    case "$1" in
    --dir)
        [ "$#" -ge 2 ] || { echo "check-ci-image-pins: --dir needs a value" >&2; exit 2; }
        DIR="$2"
        shift 2
        ;;
    --selftest)
        SELFTEST=1
        shift
        ;;
    -h | --help)
        usage
        exit 0
        ;;
    *)
        echo "check-ci-image-pins: unknown argument: $1" >&2
        exit 2
        ;;
    esac
done

FAIL=0

scan_file() {
    # stdin: `lineno:image value` lines for one file, $1 = display path
    file="$1"
    grep -nE '^[[:space:]]*image:' "$file" | while IFS= read -r match; do
        lineno="${match%%:*}"
        raw="${match#*:}"
        value="${raw#*image:}"
        value="$(printf '%s' "$value" | sed 's/#.*$//' | sed 's/[[:space:]]*$//' | sed 's/^[[:space:]]*//')"
        if printf '%s' "$value" | grep -qE '@sha256:[0-9a-f]{64}$'; then
            continue
        fi
        if printf '%s' "$raw" | grep -q 'digest-exempt:'; then
            reason="$(printf '%s' "$raw" | sed -n 's/.*digest-exempt:[[:space:]]*//p' | sed 's/[[:space:]]*$//')"
            if [ -z "$reason" ]; then
                echo "check-ci-image-pins: VIOLATION: $file:$lineno: digest-exempt needs a reason" >&2
                echo "x" >>"$FAILLOG"
                continue
            fi
            echo "check-ci-image-pins: exempt: $file:$lineno: '$value' ($reason)"
            continue
        fi
        echo "check-ci-image-pins: VIOLATION: $file:$lineno: image '$value' lacks @sha256:<64 hex> (pin the multi-arch index digest; use 'digest-exempt: <reason>' only for the Windows self-hosted shell image)" >&2
        echo "x" >>"$FAILLOG"
    done
}

scan_dir() {
    dir="$1"
    [ -d "$dir" ] || { echo "check-ci-image-pins: $dir does not exist" >&2; return 2; }
    files="$(find "$dir" -type f \( -name '*.yaml' -o -name '*.yml' \) | sort)"
    if [ -z "$files" ]; then
        echo "check-ci-image-pins: no workflow YAML under $dir" >&2
        return 2
    fi
    FAILLOG="$(mktemp)"
    export FAILLOG
    for f in $files; do
        scan_file "$f"
    done
    violations="$(wc -l <"$FAILLOG" | tr -d ' ')"
    rm -f "$FAILLOG"
    if [ "$violations" -gt 0 ]; then
        echo "check-ci-image-pins: FAIL ($violations unpinned image line(s))" >&2
        return 1
    fi
    echo "check-ci-image-pins: PASS (every CI image is digest-pinned or explicitly digest-exempt)"
    return 0
}

selftest() {
    fixtures="$SCRIPT_DIR/certification/fixtures/ci-images"
    rc=0
    if scan_dir "$fixtures/pinned" >/dev/null 2>&1; then
        echo "selftest ok: pinned fixture passes"
    else
        echo "selftest FAIL: pinned fixture was rejected" >&2
        rc=1
    fi
    if scan_dir "$fixtures/unpinned" >/dev/null 2>&1; then
        echo "selftest FAIL: unpinned fixture was accepted" >&2
        rc=1
    else
        code=$?
        if [ "$code" -eq 1 ]; then
            echo "selftest ok: unpinned fixture is rejected"
        else
            echo "selftest FAIL: unpinned fixture errored ($code) instead of failing on a violation" >&2
            rc=1
        fi
    fi
    if scan_dir "$fixtures/empty" >/dev/null 2>&1; then
        echo "selftest FAIL: empty fixture with no workflows was accepted" >&2
        rc=1
    else
        code=$?
        if [ "$code" -eq 2 ]; then
            echo "selftest ok: empty scan is a usage error"
        else
            echo "selftest FAIL: empty scan exited $code (want 2)" >&2
            rc=1
        fi
    fi
    if [ "$rc" -eq 0 ]; then
        echo "check-ci-image-pins selftest: PASS (planted unpinned image is rejected)"
    else
        echo "check-ci-image-pins selftest: FAIL" >&2
    fi
    return "$rc"
}

if [ "$SELFTEST" -eq 1 ]; then
    selftest
    exit $?
fi

scan_dir "$DIR"
exit $?

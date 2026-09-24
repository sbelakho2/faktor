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
# P2-D (additive): apt usage against live Debian repositories is audited too.
# A Faktor CI image (or a Debian snapshot + exact-version pin) does not exist
# yet, so every `- apt-get ...` / `- apt install ...` command item must carry
# ONE of these annotations on its own comment line inside the same step
# (before the command):
#
#   # apt-pinned: <image-ref@sha256:...>       (no apt usage against live repos)
#   # apt-residual: <reason>                   (known residual, flagged, loud)
#
# An apt command item with neither annotation is a violation; an `apt-pinned:`
# annotation without `@sha256:` or an `apt-residual:` without a reason is a
# violation. Residual lines are summarized on every run and documented in
# `docs/certification.md` §2.14.
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

# P2-D: apt usage may only survive with an explicit apt-pinned/apt-residual
# annotation in the same step; anything else is unpinned live-repo usage.
apt_audit_file() {
    file="$1"
    awk '
        /^[[:space:]]*-[[:space:]]*name:/ { ann = "" }
        /apt-residual:|apt-pinned:/ { ann = $0 }
        /^[[:space:]]*-[[:space:]]*(apt-get|apt)[[:space:]]/ {
            kind = "VIOLATION"
            if (ann != "") kind = "RESIDUAL"
            printf "%s\t%d\t%s\t%s\n", kind, NR, ann, $0
        }
    ' "$file" >"$APTREPORT"
    if [ ! -s "$APTREPORT" ]; then
        return 0
    fi
    while IFS="$(printf '\t')" read -r kind lineno ann cmd; do
        if [ "$kind" = "RESIDUAL" ]; then
            case "$ann" in
            *apt-pinned:*)
                if printf '%s' "$ann" | grep -qE '@sha256:[0-9a-f]{64}'; then
                    echo "check-ci-image-pins: apt-pinned: $file:$lineno ($ann)"
                    echo "p" >>"$APTLOG"
                    continue
                fi
                echo "check-ci-image-pins: VIOLATION: $file:$lineno: apt-pinned annotation needs an @sha256:<64 hex> image reference" >&2
                echo "x" >>"$FAILLOG"
                continue
                ;;
            *apt-residual:*)
                reason="$(printf '%s' "$ann" | sed -n 's/.*apt-residual:[[:space:]]*//p' | sed 's/[[:space:]]*$//')"
                if [ -z "$reason" ]; then
                    echo "check-ci-image-pins: VIOLATION: $file:$lineno: apt-residual annotation needs a reason" >&2
                    echo "x" >>"$FAILLOG"
                    continue
                fi
                echo "check-ci-image-pins: residual: $file:$lineno: unpinned apt usage ($reason)"
                echo "r" >>"$APTLOG"
                continue
                ;;
            esac
        fi
        echo "check-ci-image-pins: VIOLATION: $file:$lineno: unpinned apt usage against live Debian repositories without an 'apt-residual: <reason>' or 'apt-pinned: <image@sha256:...>' annotation: $cmd" >&2
        echo "x" >>"$FAILLOG"
    done <"$APTREPORT"
    return 0
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
    APTLOG="$(mktemp)"
    APTREPORT="$(mktemp)"
    export FAILLOG APTLOG APTREPORT
    for f in $files; do
        scan_file "$f"
        apt_audit_file "$f"
    done
    violations="$(wc -l <"$FAILLOG" | tr -d ' ')"
    residuals="$(grep -c '^r$' "$APTLOG" 2>/dev/null || true)"
    pinned="$(grep -c '^p$' "$APTLOG" 2>/dev/null || true)"
    rm -f "$FAILLOG" "$APTLOG" "$APTREPORT"
    if [ "$violations" -gt 0 ]; then
        echo "check-ci-image-pins: FAIL ($violations violation line(s): unpinned image or unannotated apt usage)" >&2
        return 1
    fi
    if [ "${residuals:-0}" -gt 0 ]; then
        echo "check-ci-image-pins: NOTE: $residuals apt usage line(s) carry 'apt-residual' (no Faktor CI image yet; see docs/certification.md §2.14)"
    fi
    if [ "${pinned:-0}" -gt 0 ]; then
        echo "check-ci-image-pins: $pinned apt-pinned line(s)"
    fi
    echo "check-ci-image-pins: PASS (every CI image is digest-pinned or explicitly digest-exempt; apt usage is annotated)"
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
    if scan_dir "$fixtures/apt-annotated" >/dev/null 2>&1; then
        echo "selftest ok: annotated apt usage passes (residual flagged, not fatal)"
    else
        echo "selftest FAIL: annotated apt usage was rejected" >&2
        rc=1
    fi
    if scan_dir "$fixtures/apt-unannotated" >/dev/null 2>&1; then
        echo "selftest FAIL: unannotated apt usage was accepted" >&2
        rc=1
    else
        code=$?
        if [ "$code" -eq 1 ]; then
            echo "selftest ok: unannotated apt usage is rejected"
        else
            echo "selftest FAIL: unannotated apt fixture errored ($code) instead of failing on a violation" >&2
            rc=1
        fi
    fi
    if scan_dir "$fixtures/apt-bare-reason" >/dev/null 2>&1; then
        echo "selftest FAIL: apt-residual without a reason was accepted" >&2
        rc=1
    else
        code=$?
        if [ "$code" -eq 1 ]; then
            echo "selftest ok: apt-residual without a reason is rejected"
        else
            echo "selftest FAIL: bare apt-residual exited $code (want 1)" >&2
            rc=1
        fi
    fi
    if [ "$rc" -eq 0 ]; then
        echo "check-ci-image-pins selftest: PASS (planted unpinned image/apt usage is rejected)"
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

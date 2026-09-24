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
# P2-D (closed): apt usage must be reproducible. Every apt-bearing step must
# carry ONE of these annotations on its own comment line inside the step
# (before the command):
#
#   # apt-pinned: <image-ref@sha256:...>   the step runs on the digest-pinned
#                                           Faktor CI image; no live-repo apt
#   # apt-snapshot: <YYYYMMDDTHHMMSSZ> <reason>
#                                           apt is pinned to a Debian/Ubuntu
#                                           snapshot at that exact timestamp
#
# An `apt-pinned:` annotation without `@sha256:` is a violation. An
# `apt-snapshot:` annotation is accepted only when the SAME step contains the
# matching snapshot URL (`snapshot.debian.org/archive/debian/<ts>`,
# `snapshot.debian.org/archive/debian-security/<ts>` or
# `snapshot.ubuntu.com/ubuntu/<ts>`) AND every `apt-get install` argument in
# that step is an exact `pkg=version` pin (shell variables are allowed).
# The legacy `apt-residual:` escape is rejected outright — there is no
# justified residual left. An apt command with no annotation is a violation.
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

# P2-D: apt usage is reproducible only through a fixed snapshot + exact
# versions, or a digest-pinned Faktor CI image. Report lines:
#   VIOLATION<TAB>file:line<TAB>message
#   SNAPSHOT<TAB>file:line<TAB>timestamp<TAB>reason
#   PINNED<TAB>file:line
apt_audit_file() {
    file="$1"
    awk -v file="$file" '
        function has_apt(s) {
            return s ~ /^[[:space:]]*(-[[:space:]]+)?(apt-get|apt)([[:space:]]|$)/
        }
        function note_url(s,    t, ts) {
            t = s
            if (t ~ /debian-security\//) {
                sub(/^.*debian-security\//, "", t)
                ts = substr(t, 1, 16)
                if (length(ts) == 16 && ts ~ /^[0-9]*T[0-9]*Z$/) url_ts[step "," ts] = 1
                return
            }
            if (t ~ /snapshot\.debian\.org\/archive\/debian\//) {
                sub(/^.*archive\/debian\//, "", t)
                ts = substr(t, 1, 16)
                if (length(ts) == 16 && ts ~ /^[0-9]*T[0-9]*Z$/) url_ts[step "," ts] = 1
                return
            }
            if (t ~ /snapshot\.ubuntu\.com\/ubuntu\//) {
                sub(/^.*ubuntu\.com\/ubuntu\//, "", t)
                ts = substr(t, 1, 16)
                if (length(ts) == 16 && ts ~ /^[0-9]*T[0-9]*Z$/) url_ts[step "," ts] = 1
                return
            }
        }
        function note_annotation(s,    t, n, parts, first, rest) {
            snap_ann[step] = 1
            t = s
            sub(/^.*apt-snapshot:[[:space:]]*/, "", t)
            n = split(t, parts, /[[:space:]]+/)
            if (n < 1) return
            first = parts[1]
            if (length(first) == 16 && first ~ /^[0-9]*T[0-9]*Z$/) ann_ts[step] = first
            rest = t
            sub(/^[^[:space:]]+/, "", rest)
            sub(/^[[:space:]]+/, "", rest)
            sub(/[[:space:]]+$/, "", rest)
            ann_reason[step] = rest
        }
        function unpinned(s,    rest, n, parts, i, tok, bad) {
            rest = s
            if (!sub(/^.*apt(-get)?[[:space:]]+install[[:space:]]+/, "", rest)) return ""
            n = split(rest, parts, /[[:space:]]+/)
            bad = ""
            for (i = 1; i <= n; i++) {
                tok = parts[i]
                if (tok == "") continue
                if (tok ~ /^-/) continue
                if (tok ~ /=/) continue
                if (tok ~ /\$/) continue
                bad = bad (bad == "" ? "" : " ") tok
            }
            return bad
        }
        {
            lines[NR] = $0
            if ($0 ~ /^[[:space:]]*-[[:space:]]*name:/) step++
            if ($0 ~ /apt-residual:/) residual[step] = 1
            if ($0 ~ /apt-pinned:/) pinned[step] = $0
            if ($0 ~ /apt-snapshot:/) note_annotation($0)
            if ($0 ~ /snapshot\.debian\.org\/archive\/debian/ || $0 ~ /snapshot\.ubuntu\.com\/ubuntu\//) note_url($0)
            if (has_apt($0)) line_step[NR] = step
        }
        END {
            for (i = 1; i <= NR; i++) {
                if (!(i in line_step)) continue
                s = line_step[i]
                loc = file ":" i
                cmd = lines[i]
                if (s in residual) {
                    printf "VIOLATION\t%s\tapt-residual is rejected (P2-D closed): pin the Debian/Ubuntu snapshot with exact versions (`# apt-snapshot: <YYYYMMDDTHHMMSSZ> <reason>`) or run on a digest-pinned CI image (`# apt-pinned: <image@sha256:...>`)\n", loc
                    continue
                }
                if (s in snap_ann) {
                    if (!(s in ann_ts)) {
                        printf "VIOLATION\t%s\tapt-snapshot annotation needs a <YYYYMMDDTHHMMSSZ> timestamp followed by a reason\n", loc
                        continue
                    }
                    if (ann_reason[s] == "") {
                        printf "VIOLATION\t%s\tapt-snapshot annotation needs a reason after the timestamp\n", loc
                        continue
                    }
                    if (!((s "," ann_ts[s]) in url_ts)) {
                        printf "VIOLATION\t%s\tapt-snapshot timestamp %s has no matching snapshot URL in the same step\n", loc, ann_ts[s]
                        continue
                    }
                    bad = unpinned(cmd)
                    if (bad != "") {
                        printf "VIOLATION\t%s\tunpinned apt-get install argument(s) (%s): a snapshot-pinned step needs exact pkg=version pins\n", loc, bad
                        continue
                    }
                    printf "SNAPSHOT\t%s\t%s %s\n", loc, ann_ts[s], ann_reason[s]
                    continue
                }
                if (s in pinned) {
                    t = pinned[s]
                    sub(/^.*@sha256:/, "", t)
                    sub(/[^0-9a-f].*$/, "", t)
                    if (t ~ /^[0-9a-f]+$/ && length(t) == 64) {
                        printf "PINNED\t%s\n", loc
                    } else {
                        printf "VIOLATION\t%s\tapt-pinned annotation needs an @sha256:<64 hex> image reference\n", loc
                    }
                    continue
                }
                printf "VIOLATION\t%s\tapt usage without an annotation: add `# apt-snapshot: <YYYYMMDDTHHMMSSZ> <reason>` (fixed snapshot + exact versions) or `# apt-pinned: <image@sha256:...>` (digest-pinned CI image)\n", loc
            }
        }
    ' "$file" >>"$APTREPORT"
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
    if [ -s "$APTREPORT" ]; then
        while IFS="$(printf '\t')" read -r kind loc msg; do
            case "$kind" in
            VIOLATION)
                echo "check-ci-image-pins: VIOLATION: $loc: $msg" >&2
                echo "x" >>"$FAILLOG"
                ;;
            SNAPSHOT)
                echo "check-ci-image-pins: apt-snapshot: $loc ($msg)"
                echo "s" >>"$APTLOG"
                ;;
            PINNED)
                echo "check-ci-image-pins: apt-pinned: $loc (digest-pinned CI image)"
                echo "p" >>"$APTLOG"
                ;;
            esac
        done <"$APTREPORT"
    fi
    violations="$(wc -l <"$FAILLOG" | tr -d ' ')"
    snapshots="$(grep -c '^s$' "$APTLOG" 2>/dev/null || true)"
    pinned="$(grep -c '^p$' "$APTLOG" 2>/dev/null || true)"
    rm -f "$FAILLOG" "$APTLOG" "$APTREPORT"
    if [ "$violations" -gt 0 ]; then
        echo "check-ci-image-pins: FAIL ($violations violation line(s): unpinned image or non-reproducible apt usage)" >&2
        return 1
    fi
    if [ "${snapshots:-0}" -gt 0 ]; then
        echo "check-ci-image-pins: $snapshots apt line(s) pinned to a fixed snapshot with exact versions"
    fi
    if [ "${pinned:-0}" -gt 0 ]; then
        echo "check-ci-image-pins: $pinned apt line(s) on a digest-pinned CI image"
    fi
    echo "check-ci-image-pins: PASS (every CI image is digest-pinned or explicitly digest-exempt; every apt line is snapshot-pinned with exact versions or on a pinned image)"
    return 0
}

selftest() {
    fixtures="$SCRIPT_DIR/certification/fixtures/ci-images"
    rc=0
    expect_pass() { # label dir
        if scan_dir "$fixtures/$2" >/dev/null 2>&1; then
            echo "selftest ok: $1"
        else
            echo "selftest FAIL: $1 (fixture rejected)" >&2
            rc=1
        fi
    }
    expect_fail() { # label dir
        if scan_dir "$fixtures/$2" >/dev/null 2>&1; then
            echo "selftest FAIL: $1 (fixture accepted)" >&2
            rc=1
        else
            code=$?
            if [ "$code" -eq 1 ]; then
                echo "selftest ok: $1"
            else
                echo "selftest FAIL: $1 (exit $code, want 1)" >&2
                rc=1
            fi
        fi
    }
    expect_pass "pinned fixture passes" pinned
    expect_fail "unpinned fixture is rejected" unpinned
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
    expect_pass "apt-pinned fixture passes (digest-pinned CI image)" apt-pinned
    expect_pass "apt-snapshot fixture passes (fixed snapshot + exact versions)" apt-snapshot
    expect_fail "unannotated apt usage is rejected" apt-unannotated
    expect_fail "apt-snapshot without timestamp/reason is rejected" apt-snapshot-bare
    expect_fail "apt-snapshot timestamp with no matching URL is rejected" apt-snapshot-mismatch
    expect_fail "apt-snapshot step with an unpinned package is rejected" apt-snapshot-unpinned
    expect_fail "legacy apt-residual escape is rejected" apt-residual
    if [ "$rc" -eq 0 ]; then
        echo "check-ci-image-pins selftest: PASS (planted unpinned image/apt usage is rejected; snapshot pins are required and verified)"
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

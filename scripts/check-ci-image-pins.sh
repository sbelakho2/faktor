#!/bin/sh
# CI image pin gate (Phase E item 12; P1 hermetic-CI closure).
#
# Every container image referenced by a Woodpecker workflow MUST be pinned by
# a multi-arch index digest: `image:tag@sha256:<64 lowercase hex>` (the
# trailing comment records the original tag and the recording date). A bare
# tag is mutable, so a compromised or retagged upstream image would silently
# change what CI runs.
#
# The documented exemptions are the self-hosted shell pseudo-images: the
# Windows backend runs `image: powershell` natively (no container, no
# `docker.io` library image to record a digest from) and the local-backend
# darwin agent runs `image: bash` as the host shell (no container image is
# pulled). Such a line MUST carry an explicit inline annotation:
#
#   image: bash # digest-exempt: <reason>
#   image: powershell # digest-exempt: <reason>
#
# Anything else missing `@sha256:` is a violation.
#
# Faktor CI image (P1 hermetic CI): `docker/faktor-ci/Dockerfile` is the
# dedicated toolchain image for the lanes that used to install apt packages at
# runtime. It is built by `scripts/build-ci-image.sh` and its digest is
# recorded in `docker/faktor-ci/image-digest.txt` (`--record-digest`):
#
#   * a reference to the Faktor CI image MUST use `@sha256:<64 hex>`; a tag
#     reference (`faktor-ci:latest`, `registry/faktor-ci:<tag>`) is rejected
#     even when annotated `digest-exempt:` (that exemption is for the
#     local-backend shell pseudo-images only);
#   * the referenced digest MUST match the recorded digest (when the digest
#     record exists / is supplied via `--digest-file`).
#
# P2-D apt rules, trust-aware:
#
#   * TRUSTED lanes (any file not under a `/untrusted/` path segment) must
#     not run unpinned apt. `# apt-residual:` is rejected outright there;
#     apt must be on the digest-pinned Faktor CI image or pinned to a fixed
#     Debian/Ubuntu snapshot with exact versions.
#   * UNTRUSTED lanes (path contains `/untrusted/`) may keep `apt-residual`
#     ONLY with a justification: `# apt-residual: <reason>`. Bare
#     `apt-residual:` remains a violation.
#
# The accepted apt annotations are unchanged:
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
# An apt command with no annotation is a violation.
#
# Usage:
#   sh scripts/check-ci-image-pins.sh [--dir DIR] [--digest-file FILE]
#   sh scripts/check-ci-image-pins.sh --selftest
#
# `--digest-file -` disables the recorded-digest comparison (the selftest
# fixtures do this); the default is `docker/faktor-ci/image-digest.txt`.
#
# Exit codes: 0 pass; 1 violation; 2 usage error.
set -u

SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
ROOT="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)"
DIR="$ROOT/.woodpecker"
DIGEST_FILE="$ROOT/docker/faktor-ci/image-digest.txt"
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
    --digest-file)
        [ "$#" -ge 2 ] || { echo "check-ci-image-pins: --digest-file needs a value (or - to disable)" >&2; exit 2; }
        if [ "$2" = "-" ]; then DIGEST_FILE=""; else DIGEST_FILE="$2"; fi
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
    # $1 = display path, $2 = digest record file ("" disables the comparison)
    file="$1"
    digest_file="$2"
    grep -nE '^[[:space:]]*image:' "$file" | while IFS= read -r match; do
        lineno="${match%%:*}"
        raw="${match#*:}"
        value="${raw#*image:}"
        value="$(printf '%s' "$value" | sed 's/#.*$//' | sed 's/[[:space:]]*$//' | sed 's/^[[:space:]]*//')"
        # The Faktor CI image is only valid by digest, and the digest must be
        # the one recorded by scripts/build-ci-image.sh. Tag references are
        # rejected even with digest-exempt: that exemption exists only for the
        # self-hosted shell pseudo-images (Windows powershell, darwin bash).
        if printf '%s' "$value" | grep -qE '(^|[/:])faktor-ci([:@]|$)'; then
            if ! printf '%s' "$value" | grep -qE '@sha256:[0-9a-f]{64}$'; then
                echo "check-ci-image-pins: VIOLATION: $file:$lineno: Faktor CI image '$value' must be pinned by digest (image: <ref>@sha256:<64 hex>); tag references are rejected" >&2
                echo "x" >>"$FAILLOG"
                continue
            fi
            if [ -n "$digest_file" ]; then
                ref_digest="$(printf '%s' "$value" | sed -n 's/.*@\(sha256:[0-9a-f]\{64\}\)$/\1/p')"
                recorded=""
                if [ -f "$digest_file" ]; then
                    recorded="$(grep -oE 'sha256:[0-9a-f]{64}' "$digest_file" | head -n 1)"
                fi
                if [ -z "$recorded" ]; then
                    echo "check-ci-image-pins: VIOLATION: $file:$lineno: Faktor CI image references '$ref_digest' but $digest_file records no digest (build it with scripts/build-ci-image.sh --record-digest $digest_file)" >&2
                    echo "x" >>"$FAILLOG"
                    continue
                fi
                if [ "$ref_digest" != "$recorded" ]; then
                    echo "check-ci-image-pins: VIOLATION: $file:$lineno: Faktor CI image digest $ref_digest does not match the recorded digest $recorded in $digest_file" >&2
                    echo "x" >>"$FAILLOG"
                    continue
                fi
            fi
            continue
        fi
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
        echo "check-ci-image-pins: VIOLATION: $file:$lineno: image '$value' lacks @sha256:<64 hex> (pin the multi-arch index digest; use 'digest-exempt: <reason>' only for the self-hosted shell pseudo-images)" >&2
        echo "x" >>"$FAILLOG"
    done
}

# Apt audit: trusted lanes must be on a pinned image or a fixed snapshot with
# exact versions; apt-residual is only tolerated in untrusted lanes and only
# with a justification. Report lines:
#   VIOLATION<TAB>file:line<TAB>message
#   SNAPSHOT<TAB>file:line<TAB>timestamp<TAB>reason
#   PINNED<TAB>file:line
#   RESIDUAL<TAB>file:line<TAB>justification
apt_audit_file() {
    # $1 = display path, $2 = "trusted" or "untrusted"
    file="$1"
    trust="$2"
    awk -v file="$file" -v trusted="$trust" '
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
        function note_residual(s,    t) {
            residual[step] = 1
            t = s
            sub(/^.*apt-residual:[[:space:]]*/, "", t)
            sub(/[[:space:]]+$/, "", t)
            residual_reason[step] = t
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
            if ($0 ~ /apt-residual:/) note_residual($0)
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
                    if (trusted == "trusted") {
                        printf "VIOLATION\t%s\tapt-residual is rejected in trusted lanes (P1 hermetic CI): the lane must run no apt at all or use the digest-pinned Faktor CI image (`# apt-pinned: <image@sha256:...>`)\n", loc
                    } else if (residual_reason[s] == "") {
                        printf "VIOLATION\t%s\tapt-residual in an untrusted lane needs a justification (`# apt-residual: <reason>`)\n", loc
                    } else {
                        printf "RESIDUAL\t%s\t%s\n", loc, residual_reason[s]
                    }
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
                printf "VIOLATION\t%s\tapt usage without an annotation: add `# apt-pinned: <image@sha256:...>` (digest-pinned CI image) or `# apt-snapshot: <YYYYMMDDTHHMMSSZ> <reason>` (fixed snapshot + exact versions)\n", loc
            }
        }
    ' "$file" >>"$APTREPORT"
}

trust_class() { # path
    case "$1" in
    */untrusted/*) printf 'untrusted' ;;
    *) printf 'trusted' ;;
    esac
}

scan_dir() {
    # $1 = dir, $2 = digest record file ("" disables the comparison)
    dir="$1"
    digest_file="${2:-}"
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
        scan_file "$f" "$digest_file"
        apt_audit_file "$f" "$(trust_class "$f")"
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
            RESIDUAL)
                echo "check-ci-image-pins: apt-residual (untrusted lane, justified): $loc ($msg)"
                echo "r" >>"$APTLOG"
                ;;
            esac
        done <"$APTREPORT"
    fi
    violations="$(wc -l <"$FAILLOG" | tr -d ' ')"
    snapshots="$(grep -c '^s$' "$APTLOG" 2>/dev/null || true)"
    pinned="$(grep -c '^p$' "$APTLOG" 2>/dev/null || true)"
    rm -f "$FAILLOG" "$APTLOG" "$APTREPORT"
    if [ "$violations" -gt 0 ]; then
        echo "check-ci-image-pins: FAIL ($violations violation line(s): unpinned image, Faktor tag reference or non-reproducible apt usage)" >&2
        return 1
    fi
    if [ "${snapshots:-0}" -gt 0 ]; then
        echo "check-ci-image-pins: $snapshots apt line(s) pinned to a fixed snapshot with exact versions"
    fi
    if [ "${pinned:-0}" -gt 0 ]; then
        echo "check-ci-image-pins: $pinned apt line(s) on a digest-pinned CI image"
    fi
    echo "check-ci-image-pins: PASS (every CI image is digest-pinned or explicitly digest-exempt; the Faktor CI image is referenced by its recorded digest; trusted lanes run no unpinned apt; untrusted apt-residual is justified)"
    return 0
}

selftest() {
    fixtures="$SCRIPT_DIR/certification/fixtures/ci-images"
    rc=0
    expect_pass() { # label dir
        if scan_dir "$fixtures/$2" "" >/dev/null 2>&1; then
            echo "selftest ok: $1"
        else
            echo "selftest FAIL: $1 (fixture rejected)" >&2
            rc=1
        fi
    }
    expect_fail() { # label dir
        if scan_dir "$fixtures/$2" "" >/dev/null 2>&1; then
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
    if scan_dir "$fixtures/empty" "" >/dev/null 2>&1; then
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
    expect_fail "legacy apt-residual escape is rejected (trusted by default)" apt-residual
    expect_pass "Faktor CI image pinned by digest passes" faktor-pinned
    expect_fail "Faktor CI image referenced by tag is rejected" faktor-tag
    expect_fail "Faktor tag cannot claim digest-exempt" faktor-tag-exempt
    expect_pass "untrusted apt-residual with justification passes" apt-residual-untrusted
    expect_fail "trusted apt-residual is rejected" apt-residual-trusted

    # The recorded digest is the single source of truth for the Faktor image.
    tmpd="$(mktemp -d)"
    trap 'rm -rf "$tmpd"' EXIT INT TERM
    dig="$(grep -oE 'sha256:[0-9a-f]{64}' "$fixtures/faktor-pinned/ci.yaml" | head -n 1)"
    printf '%s\n' "$dig" >"$tmpd/record.txt"
    if scan_dir "$fixtures/faktor-pinned" "$tmpd/record.txt" >/dev/null 2>&1; then
        echo "selftest ok: Faktor digest matching the recorded digest passes"
    else
        echo "selftest FAIL: Faktor digest matching the recorded digest was rejected" >&2
        rc=1
    fi
    printf 'sha256:a%063d\n' 0 >"$tmpd/record.txt"
    if scan_dir "$fixtures/faktor-pinned" "$tmpd/record.txt" >/dev/null 2>&1; then
        echo "selftest FAIL: Faktor digest drift was accepted" >&2
        rc=1
    else
        echo "selftest ok: Faktor digest drift against the record is rejected"
    fi
    if scan_dir "$fixtures/faktor-pinned" "$tmpd/missing.txt" >/dev/null 2>&1; then
        echo "selftest FAIL: missing digest record for a Faktor image was accepted" >&2
        rc=1
    else
        echo "selftest ok: missing digest record for a Faktor image is rejected"
    fi

    if [ "$rc" -eq 0 ]; then
        echo "check-ci-image-pins selftest: PASS (unpinned/tag Faktor images and trusted-lane apt-residual are rejected; digest-record drift and unjustified untrusted residual are rejected)"
    else
        echo "check-ci-image-pins selftest: FAIL" >&2
    fi
    return "$rc"
}

if [ "$SELFTEST" -eq 1 ]; then
    selftest
    exit $?
fi

scan_dir "$DIR" "$DIGEST_FILE"
exit $?

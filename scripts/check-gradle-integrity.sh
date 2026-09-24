#!/usr/bin/env bash
# Gradle integrity gate (Phase E item 13).
#
# Verifies the committed Gradle wrapper and dependency-integrity surface for
# apps/jetbrains before CI builds the plugin:
#
#   1. `distributionSha256Sum` in gradle-wrapper.properties is present and
#      equals the checksum pinned in gradle/wrapper/wrapper-checksums.txt
#      (the published https://services.gradle.org/distributions/ checksum for
#      the distributionUrl's version — recorded with tag + date).
#   2. the committed gradle-wrapper.jar hash equals the published
#      `<version>-wrapper.jar` checksum for the same version.
#   3. gradle/verification-metadata.xml exists, verifies checksums (sha256
#      entries), and does not disable verification via `<trusted-artifacts>`
#      or `verify-metadata="false"`.
#   4. dependency locking is enabled in build.gradle.kts
#      (`lockAllConfigurations()`) and every subproject declared in
#      settings.gradle.kts has a committed, non-empty gradle.lockfile.
#
# Regenerating (operator, after deliberately bumping a version):
#   cd apps/jetbrains
#   ./gradlew --write-locks --write-verification-metadata sha256 :frontend:buildPlugin
#   # update gradle/wrapper/wrapper-checksums.txt from the published
#   # <dist>.sha256 and <dist>-wrapper.jar.sha256 files
#
# Usage:
#   bash scripts/check-gradle-integrity.sh [--root apps/jetbrains]
#   bash scripts/check-gradle-integrity.sh --selftest
#
# Exit codes: 0 pass; 1 violation; 2 usage/setup error.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_REPO="$(cd "$SCRIPT_DIR/.." && pwd)"
GRADLE_ROOT="$ROOT_REPO/apps/jetbrains"
SELFTEST=0

# P3: this script mints `faktor-*` temp dirs; stale `kp-gradle-selftest.*`
# dirs left by the pre-migration version are still swept (older than a day).
cleanup_legacy_tmpdirs() {
    local dir
    for dir in "${TMPDIR:-/tmp}"/kp-gradle-selftest.*; do
        [ -d "$dir" ] || continue
        [ -L "$dir" ] && continue
        if [ -n "$(find "$dir" -maxdepth 0 -mtime +0 2>/dev/null)" ]; then
            rm -rf "$dir"
        fi
    done
    return 0
}
cleanup_legacy_tmpdirs

while [ "$#" -gt 0 ]; do
    case "$1" in
    --root)
        [ "$#" -ge 2 ] || { echo "check-gradle-integrity: --root needs a value" >&2; exit 2; }
        GRADLE_ROOT="$2"
        shift 2
        ;;
    --selftest)
        SELFTEST=1
        shift
        ;;
    -h | --help)
        sed -n '2,/^set -/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'
        exit 0
        ;;
    *)
        echo "check-gradle-integrity: unknown argument: $1" >&2
        exit 2
        ;;
    esac
done

HASH_TOOL=()
if command -v sha256sum >/dev/null 2>&1; then
    HASH_TOOL=(sha256sum)
elif command -v shasum >/dev/null 2>&1; then
    HASH_TOOL=(shasum -a 256)
else
    echo "check-gradle-integrity: no sha256 tool available" >&2
    exit 2
fi

hash_file() {
    "${HASH_TOOL[@]}" "$1" | cut -d' ' -f1
}

FAIL=0
bad() {
    printf 'check-gradle-integrity: VIOLATION [%s] %s\n' "$1" "$2" >&2
    FAIL=1
}
ok() { printf 'check-gradle-integrity: ok: %s\n' "$*"; }

# Pinned checksums, parsed from the checked-in file of record.
pinned_checksum() { # checksums_file filename
    awk -v want="$2" '
        /^[[:space:]]*#/ { next }
        NF >= 2 && $1 == want { print $2; exit }
    ' "$1"
}

check_root() { # dir -> 0 pass, 1 violations, 2 setup error
    local root="$1"
    local props="$root/gradle/wrapper/gradle-wrapper.properties"
    local jar="$root/gradle/wrapper/gradle-wrapper.jar"
    local checksums="$root/gradle/wrapper/wrapper-checksums.txt"
    local metadata="$root/gradle/verification-metadata.xml"
    local build="$root/build.gradle.kts"
    local settings="$root/settings.gradle.kts"

    if [ ! -f "$props" ]; then
        bad wrapper-properties "$props does not exist"
        return 1
    fi
    if [ ! -f "$checksums" ]; then
        bad checksums-file "$checksums does not exist (record the published distribution and wrapper-jar checksums)"
        return 1
    fi

    local url sum version
    url="$(sed -n 's/^distributionUrl=//p' "$props" | tr -d '\\')"
    sum="$(sed -n 's/^distributionSha256Sum=//p' "$props" | tr -d '[:space:]')"
    case "$url" in
    *gradle-[0-9]*-bin.zip) ;;
    *)
        bad wrapper-properties "distributionUrl '$url' is not a gradle-<version>-bin.zip"
        return 1
        ;;
    esac
    version="$(printf '%s' "$url" | sed -n 's/.*gradle-\([0-9][0-9.]*\)-bin\.zip$/\1/p')"
    if [ -z "$version" ]; then
        bad wrapper-properties "cannot parse the Gradle version from '$url'"
        return 1
    fi

    # 1. distribution checksum.
    local expected_dist
    expected_dist="$(pinned_checksum "$checksums" "gradle-${version}-bin.zip")"
    if [ -z "$expected_dist" ]; then
        bad distribution-checksum "wrapper-checksums.txt has no gradle-${version}-bin.zip pin"
    elif [ -z "$sum" ]; then
        bad distribution-checksum "gradle-wrapper.properties has no distributionSha256Sum for gradle-${version}"
    elif ! printf '%s' "$sum" | grep -qE '^[0-9a-f]{64}$'; then
        bad distribution-checksum "distributionSha256Sum '$sum' is not 64 lowercase hex"
    elif [ "$sum" != "$expected_dist" ]; then
        bad distribution-checksum "distributionSha256Sum '$sum' != pinned published checksum '$expected_dist' (gradle-${version}-bin.zip)"
    else
        ok "distributionSha256Sum matches the published gradle-${version}-bin.zip checksum"
    fi

    # 2. committed wrapper jar.
    local expected_jar actual_jar
    expected_jar="$(pinned_checksum "$checksums" "gradle-${version}-wrapper.jar")"
    if [ ! -f "$jar" ]; then
        bad wrapper-jar "$jar does not exist"
    elif [ -z "$expected_jar" ]; then
        bad wrapper-jar "wrapper-checksums.txt has no gradle-${version}-wrapper.jar pin"
    else
        actual_jar="$(hash_file "$jar")"
        if [ "$actual_jar" != "$expected_jar" ]; then
            bad wrapper-jar "committed gradle-wrapper.jar hashes $actual_jar, pinned published checksum is $expected_jar (gradle-${version}-wrapper.jar)"
        else
            ok "committed gradle-wrapper.jar matches the published gradle-${version}-wrapper.jar checksum"
        fi
    fi

    # 3. verification metadata.
    if [ ! -f "$metadata" ]; then
        bad verification-metadata "$metadata does not exist (run: ./gradlew --write-verification-metadata sha256 :frontend:buildPlugin)"
    else
        local components sha_entries
        components="$(grep -c '<component ' "$metadata" || true)"
        sha_entries="$(grep -c '<sha256 value=' "$metadata" || true)"
        if grep -q '<trusted-artifacts>' "$metadata"; then
            bad verification-metadata "$metadata declares <trusted-artifacts>, which disables checksum verification for matched artifacts"
        fi
        if grep -q 'verify-metadata="false"' "$metadata"; then
            bad verification-metadata "$metadata sets verify-metadata=\"false\""
        fi
        if [ "$components" -lt 1 ] || [ "$sha_entries" -lt 1 ]; then
            bad verification-metadata "$metadata has no checksummed components (components=$components sha256=$sha_entries)"
        else
            ok "verification metadata present ($components component(s), $sha_entries sha256 checksum(s), no bypass)"
        fi
    fi

    # 4. dependency locking.
    if [ ! -f "$build" ]; then
        bad dependency-lock "$build does not exist"
        return 1
    fi
    if ! grep -q 'lockAllConfigurations()' "$build"; then
        bad dependency-lock "build.gradle.kts does not enable dependencyLocking { lockAllConfigurations() }"
    fi
    if [ ! -f "$settings" ]; then
        bad dependency-lock "$settings does not exist"
        return 1
    fi
    local subproject lock found_lock=0
    while IFS= read -r subproject; do
        [ -n "$subproject" ] || continue
        lock="$root/${subproject#:}/gradle.lockfile"
        if [ ! -f "$lock" ]; then
            bad dependency-lock "subproject ':$subproject' has no committed $lock"
            continue
        fi
        found_lock=1
        if ! grep -qE '^[^#].*=' "$lock"; then
            bad dependency-lock "$lock is empty (no locked modules)"
        fi
    done < <(sed -n 's/^include(":\([A-Za-z0-9_-]*\)").*/\1/p' "$settings")
    if [ "$found_lock" -eq 1 ]; then
        ok "dependency locks present for every declared subproject"
    fi

    if [ "$FAIL" -ne 0 ]; then
        return 1
    fi
    return 0
}

copy_into() { # src_root dest_root
    local src="$1" dst="$2" f rel
    mkdir -p "$dst"
    (cd "$src" && find . -type f \( -name 'gradle-wrapper.properties' -o -name 'gradle-wrapper.jar' -o -name 'wrapper-checksums.txt' -o -name 'verification-metadata.xml' -o -name 'gradle.lockfile' -o -name 'build.gradle.kts' -o -name 'settings.gradle.kts' \) -print) |
        while IFS= read -r f; do
            rel="${f#./}"
            mkdir -p "$dst/$(dirname "$rel")"
            cp "$src/$rel" "$dst/$rel"
        done
}

selftest() {
    local tmp base rc failures=0
    command -v mktemp >/dev/null 2>&1 || exit 2
    tmp="$(mktemp -d "${TMPDIR:-/tmp}/faktor-gradle-selftest.XXXXXX")"
    base="$tmp/root"
    copy_into "$GRADLE_ROOT" "$base"

    if FAIL=0; check_root "$base" >/dev/null 2>&1; then
        echo "selftest ok: pristine copy passes"
    else
        echo "selftest FAIL: pristine copy was rejected" >&2
        failures=$((failures + 1))
    fi

    # planted violation: tampered wrapper jar
    printf 'tampered\n' >>"$base/gradle/wrapper/gradle-wrapper.jar"
    FAIL=0
    if check_root "$base" >/dev/null 2>&1; then
        echo "selftest FAIL: tampered wrapper jar was accepted" >&2
        failures=$((failures + 1))
    else
        echo "selftest ok: tampered wrapper jar is rejected"
    fi
    rm -rf "$base" && copy_into "$GRADLE_ROOT" "$base"

    # planted violation: checksum mismatch
    sed -i.bak 's/^distributionSha256Sum=.*/distributionSha256Sum=0000000000000000000000000000000000000000000000000000000000000000/' "$base/gradle/wrapper/gradle-wrapper.properties"
    rm -f "$base/gradle/wrapper/gradle-wrapper.properties.bak"
    FAIL=0
    if check_root "$base" >/dev/null 2>&1; then
        echo "selftest FAIL: mismatched distributionSha256Sum was accepted" >&2
        failures=$((failures + 1))
    else
        echo "selftest ok: mismatched distributionSha256Sum is rejected"
    fi
    rm -rf "$base" && copy_into "$GRADLE_ROOT" "$base"

    # planted violation: verification metadata removed
    rm -f "$base/gradle/verification-metadata.xml"
    FAIL=0
    if check_root "$base" >/dev/null 2>&1; then
        echo "selftest FAIL: missing verification metadata was accepted" >&2
        failures=$((failures + 1))
    else
        echo "selftest ok: missing verification metadata is rejected"
    fi
    rm -rf "$base" && copy_into "$GRADLE_ROOT" "$base"

    # planted violation: one lockfile removed
    local lock_to_remove
    lock_to_remove="$(find "$base" -name gradle.lockfile -print -quit)"
    rm -f "$lock_to_remove"
    FAIL=0
    if check_root "$base" >/dev/null 2>&1; then
        echo "selftest FAIL: missing subproject lockfile was accepted" >&2
        failures=$((failures + 1))
    else
        echo "selftest ok: missing subproject lockfile is rejected"
    fi

    # P3 temp-name migration: no kp-* minting, legacy dirs still swept.
    local legacy
    if grep -qE 'mktemp -d "[^"]*kp-' "$0"; then
        echo "selftest FAIL: script still mints a kp-* temp name" >&2
        failures=$((failures + 1))
    fi
    if grep -qF 'faktor-gradle-selftest.XXXXXX' "$0"; then
        :
    else
        echo "selftest FAIL: script does not mint a faktor-* temp name" >&2
        failures=$((failures + 1))
    fi
    legacy="$tmp/legacy-tmp"
    mkdir -p "$legacy/kp-gradle-selftest.dead" "$legacy/faktor-gradle.keep"
    touch -t 202001010000 "$legacy/kp-gradle-selftest.dead" "$legacy/faktor-gradle.keep"
    (
        TMPDIR="$legacy"
        cleanup_legacy_tmpdirs
    )
    if [ ! -e "$legacy/kp-gradle-selftest.dead" ] && [ -d "$legacy/faktor-gradle.keep" ]; then
        echo "selftest ok: legacy kp-* temp dirs are swept, faktor-* dirs survive"
    else
        echo "selftest FAIL: legacy kp-* sweep did not behave" >&2
        failures=$((failures + 1))
    fi

    rm -rf "$tmp"
    if [ "$failures" -ne 0 ]; then
        echo "check-gradle-integrity selftest: FAIL ($failures case(s))" >&2
        return 1
    fi
    echo "check-gradle-integrity selftest: PASS (planted tampering is rejected)"
    return 0
}

if [ "$SELFTEST" -eq 1 ]; then
    selftest
    exit $?
fi

check_root "$GRADLE_ROOT"
rc=$?
if [ "$rc" -eq 2 ]; then
    echo "check-gradle-integrity: setup error" >&2
    exit 2
fi
if [ "$rc" -ne 0 ]; then
    echo "check-gradle-integrity: FAIL" >&2
    exit 1
fi
echo "check-gradle-integrity: PASS (wrapper, distribution sha256, verification metadata, dependency locks)"
exit 0

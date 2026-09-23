#!/usr/bin/env bash
# Installable artifact packaging (audit 90).
#
# Builds the release daemon, the VS Code extension VSIX and a copy of the
# JetBrains plugin zip when each is available, then writes
# target/certification/artifacts.json with {name, path, sha256, size,
# commit, status, detail} per artifact. Nothing is silently claimed: an
# artifact that could not be produced is recorded with its exact error and
# a runnable retry command.
#
# Artifacts (under $PACKAGE_ARTIFACT_DIR, default target/certification/artifacts):
#   faktor-cli-<version>-<os>-<arch>.tar.gz   daemon bundle: bin/faktor-cli,
#                                             checksums.txt, RELEASE
#   faktor-cli-<version>-<os>-<arch>          the RAW daemon binary (additive):
#                                             the artifact the immutable
#                                             release layout installs as
#                                             versions/<release-id>/faktor
#   faktor-<version>.vsix                     VS Code extension (vsce package)
#   faktor-jetbrains-plugin-<version>.zip     copy of the Gradle plugin zip
#
# Env:
#   PACKAGE_OUT_DIR           output dir (default target/certification)
#   PACKAGE_ARTIFACT_DIR      artifact dir (default <out>/artifacts)
#   PACKAGE_SKIP_BUILD=1      do not rebuild; reuse target/release/faktor-cli
#   PACKAGE_SKIP_VSIX=1       record the VSIX as skipped without attempting it
#   PACKAGE_SKIP_JETBRAINS=1  record the JetBrains zip as skipped
#   PACKAGE_REQUIRE_VSIX=1    a missing/broken VSIX becomes fatal
#   PACKAGE_SKIP_UPDATE_MANIFEST=1  do not assemble the signed update manifest
#   FAKTOR_UPDATE_SIGNING_KEY       operator ed25519 key for the update
#                                   manifest (absent => explicitly UNSIGNED,
#                                   which `apply` refuses); FAKTOR_UPDATE_KEY_ID
#                                   names the allowlisted identity
#   FAKTOR_UPDATE_URL_BASE          distribution URL base for artifact urls
#   PACKAGE_SIGN=0                  do not attempt OS-level signing (the
#                                   artifacts are recorded with an explicit
#                                   `skipped` signature marker, never a claim)
#   PACKAGE_REQUIRE_SIGNED=1        OS signing is mandatory: a missing
#                                   identity/certificate or an unsigned
#                                   artifact is fatal (the drivers' loud
#                                   UNSIGNED marker becomes a refusal)
#
# Additively (step 4.5) every built artifact is passed through the host's OS
# signing driver (scripts/sign-macos.sh on darwin: codesign runtime options +
# notarytool + staple; scripts/sign-windows.sh elsewhere when a PFX/identity
# is configured: Authenticode). The daemon bundle's INNER bin/faktor-cli is
# signed BEFORE checksums.txt is generated, so the bundle's recorded digest
# covers the signed bytes. Per-artifact signature metadata (tool, status,
# scope, identity, subject digest, marker) is recorded into artifacts.json
# AND into target/certification/artifact-signatures.json; absent
# env/keys produce an explicit UNSIGNED marker (never a silent claim) and the
# artifacts stay usable locally. `--require-signed` on the drivers is wired
# from PACKAGE_REQUIRE_SIGNED=1.
#
# Additively (step 5) the script assembles the SIGNED update manifest
# (`faktor-update/v1`) from THIS run's artifacts.json + certification
# evidence via scripts/update-manifest.mjs: with FAKTOR_UPDATE_SIGNING_KEY
# set the manifest is signed (and --require-signed fails the packaging run
# if signing breaks); without it the manifest is written explicitly UNSIGNED.
# FAKTOR_UPDATE_RELEASE_GENERATION (env, passed through to
# update-manifest.mjs) records the signed anti-rollback counter
# `release_generation`; absent it the manifest is a legacy generation-0
# manifest the updater only admits via [updater]
# allow_legacy_manifests_once. Node absent => the step is recorded as
# skipped, never silently claimed.
#
# Additively (step 6) the immutable release layout tree is staged under
# $OUT_DIR/install/versions/<release-id>/{faktor,manifest}, with
# release-id = <version>-<raw binary sha256[..12]> (the exact derivation of
# crates/updater::release::release_id_for). The signed update manifest MUST
# carry the raw binary's digest or the step refuses: the bootstrap launcher
# re-verifies that binding at every launch, so a doctored staging tree can
# never be activated.
#
# Exit non-zero when the daemon bundle cannot be produced, when a VSIX
# packaging attempt fails for a non-environmental reason, when
# PACKAGE_REQUIRE_VSIX=1 and no VSIX was produced, or when the signed update
# manifest cannot be assembled while a signing key is configured.
set -u
set -o pipefail
export LC_ALL=C

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT" || exit 2

OUT_DIR="${PACKAGE_OUT_DIR:-target/certification}"
ART_DIR="${PACKAGE_ARTIFACT_DIR:-$OUT_DIR/artifacts}"
SKIP_BUILD="${PACKAGE_SKIP_BUILD:-0}"
SKIP_VSIX="${PACKAGE_SKIP_VSIX:-0}"
SKIP_JETBRAINS="${PACKAGE_SKIP_JETBRAINS:-0}"
REQUIRE_VSIX="${PACKAGE_REQUIRE_VSIX:-0}"
SIGN_ARTIFACTS="${PACKAGE_SIGN:-1}"
REQUIRE_SIGNED="${PACKAGE_REQUIRE_SIGNED:-0}"

case "$OUT_DIR" in /*) ;; *) OUT_DIR="$ROOT/$OUT_DIR" ;; esac
case "$ART_DIR" in /*) ;; *) ART_DIR="$ROOT/$ART_DIR" ;; esac
MANIFEST="$OUT_DIR/artifacts.json"
LOG_DIR="$OUT_DIR/logs"
mkdir -p "$ART_DIR" "$LOG_DIR" || exit 2
TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/faktor-package.XXXXXX")" || exit 2
trap 'rm -rf "$TMP_DIR"' EXIT

FATAL=0

now_iso() {
    date -u +%Y-%m-%dT%H:%M:%SZ
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

file_size() {
    if stat -f %z "$1" >/dev/null 2>&1; then
        stat -f %z "$1"
    else
        stat -c %s "$1"
    fi
}

rel_path() {
    case "$1" in
        "$ROOT"/*) printf '%s' "${1#"$ROOT"/}" ;;
        *) printf '%s' "$1" ;;
    esac
}

first_error_line() {
    grep -a -m1 -E '(^error|error TS|npm error|ENOENT|EACCES|not found|Not found)' "$1" 2>/dev/null |
        head -n1 | tr -d '\r' | cut -c1-240
}

network_like_error() {
    grep -qaiE 'ENOTFOUND|EAI_AGAIN|ETIMEDOUT|ECONNREFUSED|ECONNRESET|ENETUNREACH|network|registry|fetch failed|getaddrinfo|npm error code E40[0-9]|npm error 404|404 Not Found' "$1" 2>/dev/null
}

# ---------------------------------------------------------------------------
# Artifact bookkeeping (bash 3.2 compatible indexed arrays).
# ---------------------------------------------------------------------------
A_NAME=()
A_KIND=()
A_PATH=()
A_SHA=()
A_SIZE=()
A_COMMIT=()
A_STATUS=()
A_DETAIL=()
S_NAME=()
S_REASON=()

add_artifact() {
    # name kind path status detail
    local name="$1" kind="$2" path="$3" status="$4" detail="$5" sha size
    sha=""
    size=""
    if [ -f "$path" ]; then
        sha="$(hash_file "$path" 2>/dev/null || printf '')"
        size="$(file_size "$path" 2>/dev/null || printf '')"
    fi
    A_NAME+=("$name")
    A_KIND+=("$kind")
    A_PATH+=("$(rel_path "$path")")
    A_SHA+=("$sha")
    A_SIZE+=("$size")
    A_COMMIT+=("$COMMIT")
    A_STATUS+=("$status")
    A_DETAIL+=("$detail")
    if [ "$status" = "built" ]; then
        printf '[package] %-34s %-8s %10s bytes  sha256=%.12s  %s\n' \
            "$name" "$status" "${size:-?}" "${sha:-none}" "$detail"
    else
        printf '[package] %-34s %-8s %s\n' "$name" "$status" "$detail"
    fi
}

add_skip() {
    S_NAME+=("$1")
    S_REASON+=("$2")
    printf '[package] %-34s %-8s %s\n' "$1" "skip" "$2"
}

# ---------------------------------------------------------------------------
# OS-level signing (step 4.5, additive). Every record here is either the
# driver's own verdict verbatim or an explicit synthesized marker; nothing is
# ever claimed signed without the driver saying so.
# ---------------------------------------------------------------------------
SIG_NAME=()
SIG_JSON=()

sig_json() {
    # status scope detail tool
    printf '{"tool":"%s","status":"%s","scope":"%s","subject_sha256":null,"identity":null,"detail":"%s","marker":%s}' \
        "$(json_escape "$4")" "$(json_escape "$1")" "$(json_escape "$2")" \
        "$(json_escape "$3")" \
        "$(if [ "$1" = "unsigned" ] || [ "$1" = "skipped" ]; then printf '"UNSIGNED"'; else printf 'null'; fi)"
}

driver_for_host() {
    case "$OS" in
        darwin) printf '%s' "$ROOT/scripts/sign-macos.sh" ;;
        mingw* | msys* | cygwin*) printf '%s' "$ROOT/scripts/sign-windows.sh" ;;
        *) printf '' ;;
    esac
}

# Sign one file (in place) through the host driver and record the verdict
# under the artifact `name`. Returns non-zero only when the driver refused
# AND PACKAGE_REQUIRE_SIGNED=1 (a mandatory-signing release); an honest
# UNSIGNED marker is a success with FATAL untouched.
sign_artifact() {
    # name path kind scope
    local name="$1" path="$2" kind="$3" scope="$4"
    local driver out rc
    if [ "$SIGN_ARTIFACTS" != "1" ]; then
        SIG_NAME+=("$name")
        SIG_JSON+=("$(sig_json skipped "$scope" "signing disabled (PACKAGE_SIGN=0)" "none")")
        printf '[package] %-34s %-8s %s\n' "$name" "unsigned" "PACKAGE_SIGN=0"
        if [ "$REQUIRE_SIGNED" = "1" ]; then
            printf '[package] PACKAGE_REQUIRE_SIGNED=1 but OS signing is disabled (PACKAGE_SIGN=0)\n' >&2
            FATAL=1
        fi
        return 0
    fi
    driver="$(driver_for_host)"
    if [ -z "$driver" ] || [ ! -f "$driver" ]; then
        SIG_NAME+=("$name")
        SIG_JSON+=("$(sig_json unsigned "$scope" "no OS signing driver for host $OS" "none")")
        printf '[package] %-34s %-8s UNSIGNED (no driver for host %s)\n' "$name" "unsigned" "$OS"
        if [ "$REQUIRE_SIGNED" = "1" ]; then
            FATAL=1
        fi
        return 0
    fi
    mkdir -p "$LOG_DIR"
    if [ "$REQUIRE_SIGNED" = "1" ]; then
        out="$(bash "$driver" sign "$path" --kind "$kind" --require-signed \
            2>>"$LOG_DIR/package-signing.log")"
    else
        out="$(bash "$driver" sign "$path" --kind "$kind" 2>>"$LOG_DIR/package-signing.log")"
    fi
    rc=$?
    if [ "$rc" -ne 0 ]; then
        local detail
        detail="$(printf '%s' "$out" | sed -n 's/.*"detail":"\([^"]*\)".*/\1/p' | head -n1)"
        [ -n "$detail" ] || detail="signing driver exited $rc; see $(rel_path "$LOG_DIR/package-signing.log")"
        SIG_NAME+=("$name")
        SIG_JSON+=("$(sig_json failed "$scope" "$detail" "$(basename "$driver")")")
        printf '[package] %-34s %-8s %s\n' "$name" "failed" "OS signing refused: $detail"
        FATAL=1
        return 1
    fi
    if [ -z "$out" ]; then
        SIG_NAME+=("$name")
        SIG_JSON+=("$(sig_json failed "$scope" "signing driver produced no verdict" "$(basename "$driver")")")
        printf '[package] %-34s %-8s driver produced no verdict\n' "$name" "failed"
        FATAL=1
        return 1
    fi
    SIG_NAME+=("$name")
    # The caller knows the semantic scope (e.g. `inner:bin/faktor-cli` for a
    # bundle whose inner executable was signed before archiving); keep it in
    # the recorded metadata instead of the driver's generic file scope.
    if [ "$scope" != "file" ] && [ "$scope" != "$kind" ]; then
        out="$(printf '%s' "$out" | sed "s/\"scope\":\"[^\"]*\"/\"scope\":\"$(printf '%s' "$scope" | sed 's/[\/&]/\\&/g')\"/")"
    fi
    SIG_JSON+=("$out")
    local status detail
    status="$(printf '%s' "$out" | sed -n 's/.*"status":"\([^"]*\)".*/\1/p' | head -n1)"
    detail="$(printf '%s' "$out" | sed -n 's/.*"detail":"\([^"]*\)".*/\1/p' | head -n1)"
    printf '[package] %-34s %-8s %s\n' "$name" "os-sign" "${detail:-${status:-?}}"
    if [ "$REQUIRE_SIGNED" = "1" ] && [ "$status" != "signed" ]; then
        printf '[package] PACKAGE_REQUIRE_SIGNED=1 but %s is not signed (status=%s)\n' \
            "$name" "$status" >&2
        FATAL=1
        return 1
    fi
    return 0
}

sig_for() {
    local name="$1" i
    for i in "${!SIG_NAME[@]}"; do
        if [ "${SIG_NAME[$i]}" = "$name" ]; then
            printf '%s' "${SIG_JSON[$i]}"
            return 0
        fi
    done
    return 1
}

COMMIT="$(git rev-parse HEAD 2>/dev/null || printf unknown)"
DIRTY="$(git status --porcelain 2>/dev/null | wc -l | tr -d ' ')"
VERSION="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml 2>/dev/null | head -n1)"
[ -n "$VERSION" ] || VERSION="0.0.0"
OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
ARCH="$(uname -m)"
BUNDLE_NAME="faktor-cli-$VERSION-$OS-$ARCH.tar.gz"
RAW_NAME="faktor-cli-$VERSION-$OS-$ARCH"
RAW_SHA=""

printf '[package] commit=%s dirty=%s version=%s host=%s/%s\n' \
    "$COMMIT" "$DIRTY" "$VERSION" "$OS" "$ARCH"

# ---------------------------------------------------------------------------
# 1. Release daemon + bundle (required).
# ---------------------------------------------------------------------------
DAEMON_BIN="$ROOT/target/release/faktor-cli"
BUNDLE_PATH="$ART_DIR/$BUNDLE_NAME"
CARGO_OK=0
if [ "$SKIP_BUILD" = "1" ]; then
    if [ -x "$DAEMON_BIN" ]; then
        CARGO_OK=1
        printf '[package] build skipped (PACKAGE_SKIP_BUILD=1); reusing %s\n' "$(rel_path "$DAEMON_BIN")"
    else
        add_artifact "$BUNDLE_NAME" daemon-bundle "$BUNDLE_PATH" "failed" \
            "PACKAGE_SKIP_BUILD=1 but target/release/faktor-cli is absent; run: cargo build --release -p faktor-cli"
        FATAL=1
    fi
else
    if cargo build --release -p faktor-cli >"$LOG_DIR/package-cargo-build.log" 2>&1; then
        CARGO_OK=1
        printf '[package] cargo build --release -p faktor-cli: ok\n'
    else
        detail="$(first_error_line "$LOG_DIR/package-cargo-build.log")"
        [ -n "$detail" ] || detail="cargo exited non-zero"
        add_artifact "$BUNDLE_NAME" daemon-bundle "$BUNDLE_PATH" "failed" \
            "cargo build --release -p faktor-cli failed: $detail; see $(rel_path "$LOG_DIR/package-cargo-build.log")"
        FATAL=1
    fi
fi

if [ "$CARGO_OK" = "1" ] && [ ! -x "$DAEMON_BIN" ]; then
    add_artifact "$BUNDLE_NAME" daemon-bundle "$BUNDLE_PATH" "failed" \
        "target/release/faktor-cli missing after build; see $(rel_path "$LOG_DIR/package-cargo-build.log")"
    FATAL=1
    CARGO_OK=0
fi

if [ "$CARGO_OK" = "1" ]; then
    STAGE="$TMP_DIR/stage/faktor-cli-$VERSION-$OS-$ARCH"
    mkdir -p "$STAGE/bin" || FATAL=1
    cp "$DAEMON_BIN" "$STAGE/bin/faktor-cli" || FATAL=1
    chmod +x "$STAGE/bin/faktor-cli" 2>/dev/null || FATAL=1
    # OS signing happens BEFORE checksums.txt is generated so the recorded
    # inner digest (and therefore the bundle digest) covers the SIGNED bytes.
    sign_artifact "$BUNDLE_NAME" "$STAGE/bin/faktor-cli" daemon-bundle "inner:bin/faktor-cli"
    inner_sha="$(hash_file "$STAGE/bin/faktor-cli" 2>/dev/null || printf unknown)"
    printf '%s  bin/faktor-cli\n' "$inner_sha" >"$STAGE/checksums.txt"
    {
        printf 'faktor-cli %s (%s/%s)\n' "$VERSION" "$OS" "$ARCH"
        printf 'commit: %s\n' "$COMMIT"
        printf 'install: extract this archive and run bin/faktor-cli doctor --data-dir <dir>\n'
    } >"$STAGE/RELEASE"
    if tar -czf "$BUNDLE_PATH" -C "$TMP_DIR/stage" "faktor-cli-$VERSION-$OS-$ARCH"; then
        add_artifact "$BUNDLE_NAME" daemon-bundle "$BUNDLE_PATH" "built" \
            "bin/faktor-cli + checksums.txt + RELEASE; inner sha256=$inner_sha"
    else
        add_artifact "$BUNDLE_NAME" daemon-bundle "$BUNDLE_PATH" "failed" \
            "tar -czf failed while bundling target/release/faktor-cli"
        FATAL=1
    fi
    # Additive RAW daemon binary artifact (the immutable release layout
    # installs exactly these bytes as versions/<release-id>/faktor). Identical
    # to the bundle's signed inner binary; the signed update manifest must
    # carry this digest for the release layout to be launchable.
    RAW_PATH="$ART_DIR/$RAW_NAME"
    if cp "$STAGE/bin/faktor-cli" "$RAW_PATH" && chmod +x "$RAW_PATH" 2>/dev/null; then
        sign_artifact "$RAW_NAME" "$RAW_PATH" daemon-binary "file"
        RAW_SHA="$(hash_file "$RAW_PATH" 2>/dev/null || printf unknown)"
        add_artifact "$RAW_NAME" daemon-binary "$RAW_PATH" "built" \
            "raw daemon binary for the immutable release layout; sha256=$RAW_SHA"
    else
        add_artifact "$RAW_NAME" daemon-binary "$RAW_PATH" "failed" \
            "copy of the signed inner daemon binary failed"
        FATAL=1
    fi
fi

# ---------------------------------------------------------------------------
# 2. VS Code extension VSIX (best effort, recorded).
# ---------------------------------------------------------------------------
VSIX_NAME="faktor-$VERSION.vsix"
VSIX_PATH="$ART_DIR/$VSIX_NAME"
VSIX_APP="$ROOT/apps/vscode"
VSIX_RETRY="cd apps/vscode && npm ci && npm run build && npx --no-install vsce package --out $(rel_path "$VSIX_PATH")"

if [ "$SKIP_VSIX" = "1" ]; then
    add_skip "$VSIX_NAME" "PACKAGE_SKIP_VSIX=1: VSIX packaging not attempted"
elif [ ! -d "$VSIX_APP" ]; then
    add_skip "$VSIX_NAME" "apps/vscode not present in this workspace"
elif ! command -v node >/dev/null 2>&1 || ! command -v npm >/dev/null 2>&1; then
    add_skip "$VSIX_NAME" "node/npm not on PATH; retry: $VSIX_RETRY"
elif ! command -v npx >/dev/null 2>&1; then
    add_skip "$VSIX_NAME" "npx not on PATH; retry: $VSIX_RETRY"
else
    VSIX_OK=1
    if [ ! -d "$VSIX_APP/node_modules" ] || [ ! -x "$VSIX_APP/node_modules/.bin/vsce" ]; then
        if (cd "$VSIX_APP" && npm ci) >"$LOG_DIR/package-vsix-npm-ci.log" 2>&1; then
            printf '[package] npm ci (apps/vscode): ok\n'
        else
            detail="$(first_error_line "$LOG_DIR/package-vsix-npm-ci.log")"
            [ -n "$detail" ] || detail="npm ci exited non-zero"
            if network_like_error "$LOG_DIR/package-vsix-npm-ci.log"; then
                add_skip "$VSIX_NAME" "npm ci could not reach the registry: $detail; retry: $VSIX_RETRY"
            else
                add_artifact "$VSIX_NAME" vsix "$VSIX_PATH" "failed" \
                    "npm ci failed: $detail; see $(rel_path "$LOG_DIR/package-vsix-npm-ci.log")"
                FATAL=1
            fi
            VSIX_OK=0
        fi
    fi
    BUILD_STATUS="pass"
    BUILD_DETAIL="npm run build (tsc) ok"
    if [ "$VSIX_OK" = "1" ]; then
        if (cd "$VSIX_APP" && npm run build) >"$LOG_DIR/package-vsix-build.log" 2>&1; then
            printf '[package] npm run build (apps/vscode): ok\n'
        else
            detail="$(first_error_line "$LOG_DIR/package-vsix-build.log")"
            [ -n "$detail" ] || detail="tsc exited non-zero"
            BUILD_STATUS="failed"
            BUILD_DETAIL="npm run build exited non-zero: $detail"
            if [ -f "$VSIX_APP/out/extension.js" ]; then
                printf '[package] npm run build (apps/vscode): FAILED (emitted out/ anyway): %s\n' "$detail"
            else
                add_artifact "$VSIX_NAME" vsix "$VSIX_PATH" "failed" \
                    "npm run build produced no out/extension.js: $detail; see $(rel_path "$LOG_DIR/package-vsix-build.log")"
                FATAL=1
                VSIX_OK=0
            fi
        fi
    fi
    if [ "$VSIX_OK" = "1" ]; then
        if (cd "$VSIX_APP" && npx --no-install vsce package --out "$VSIX_PATH") \
            >"$LOG_DIR/package-vsix-vsce.log" 2>&1; then
            add_artifact "$VSIX_NAME" vsix "$VSIX_PATH" "built" \
                "vsce package ok (build: $BUILD_DETAIL)"
            sign_artifact "$VSIX_NAME" "$VSIX_PATH" vsix "file"
            if [ "$BUILD_STATUS" != "pass" ]; then
                printf '[package] WARNING: %s is installable but was built from a tree with a failing npm run build\n' "$VSIX_NAME"
            fi
        else
            detail="$(first_error_line "$LOG_DIR/package-vsix-vsce.log")"
            [ -n "$detail" ] || detail="vsce exited non-zero"
            if network_like_error "$LOG_DIR/package-vsix-vsce.log"; then
                add_skip "$VSIX_NAME" "vsce could not reach the registry/tool: $detail; retry: $VSIX_RETRY"
            else
                add_artifact "$VSIX_NAME" vsix "$VSIX_PATH" "failed" \
                    "vsce package failed: $detail; see $(rel_path "$LOG_DIR/package-vsix-vsce.log")"
                FATAL=1
            fi
        fi
    fi
fi

# ---------------------------------------------------------------------------
# 3. JetBrains plugin zip (copied when present, recorded when absent).
# ---------------------------------------------------------------------------
JB_DIST="$ROOT/apps/jetbrains/frontend/build/distributions"
JB_SRC=""
if [ -d "$JB_DIST" ]; then
    for candidate in "$JB_DIST"/*.zip; do
        [ -f "$candidate" ] || continue
        JB_SRC="$candidate"
        break
    done
fi
JB_NAME="faktor-jetbrains-plugin-$VERSION.zip"
JB_PATH="$ART_DIR/$JB_NAME"
if [ "$SKIP_JETBRAINS" = "1" ]; then
    add_skip "$JB_NAME" "PACKAGE_SKIP_JETBRAINS=1: JetBrains zip copy not attempted"
elif [ -z "$JB_SRC" ]; then
    add_skip "$JB_NAME" \
        "apps/jetbrains/frontend/build/distributions/*.zip absent; build it first: bash apps/jetbrains/compile-and-smoke.sh"
else
    if cp "$JB_SRC" "$JB_PATH" && [ -f "$JB_PATH" ]; then
        add_artifact "$JB_NAME" jetbrains-plugin "$JB_PATH" "built" \
            "copied from $(rel_path "$JB_SRC")"
        sign_artifact "$JB_NAME" "$JB_PATH" jetbrains-plugin "file"
    else
        add_artifact "$JB_NAME" jetbrains-plugin "$JB_PATH" "failed" \
            "copying $(rel_path "$JB_SRC") failed"
        FATAL=1
    fi
fi

if [ "$REQUIRE_VSIX" = "1" ]; then
    found=0
    for i in "${!A_NAME[@]}"; do
        if [ "${A_KIND[$i]}" = "vsix" ] && [ "${A_STATUS[$i]}" = "built" ]; then
            found=1
        fi
    done
    if [ "$found" != "1" ]; then
        printf '[package] PACKAGE_REQUIRE_VSIX=1 and no VSIX was produced\n' >&2
        FATAL=1
    fi
fi

# ---------------------------------------------------------------------------
# 4. artifacts.json (always written, pass or fail).
# ---------------------------------------------------------------------------
if [ "$FATAL" -eq 0 ]; then
    STATUS="pass"
else
    STATUS="fail"
fi

emit_manifest() {
    local i first sig
    printf '{\n'
    printf '  "schema": "faktor-artifacts/v1",\n'
    printf '  "status": "%s",\n' "$STATUS"
    printf '  "commit": "%s",\n' "$(json_escape "$COMMIT")"
    printf '  "dirty_count": %s,\n' "${DIRTY:-0}"
    printf '  "version": "%s",\n' "$(json_escape "$VERSION")"
    printf '  "os": "%s",\n' "$(json_escape "$OS")"
    printf '  "arch": "%s",\n' "$(json_escape "$ARCH")"
    printf '  "timestamp": "%s",\n' "$(now_iso)"
    printf '  "artifacts": ['
    first=1
    for i in "${!A_NAME[@]}"; do
        if [ "$first" -eq 0 ]; then printf ','; fi
        first=0
        printf '\n    {'
        printf '"name":"%s",' "$(json_escape "${A_NAME[$i]}")"
        printf '"kind":"%s",' "$(json_escape "${A_KIND[$i]}")"
        printf '"path":"%s",' "$(json_escape "${A_PATH[$i]}")"
        printf '"sha256":%s,' "$(if [ -n "${A_SHA[$i]}" ]; then printf '"%s"' "$(json_escape "${A_SHA[$i]}")"; else printf 'null'; fi)"
        printf '"size":%s,' "${A_SIZE[$i]:-null}"
        printf '"commit":"%s",' "$(json_escape "${A_COMMIT[$i]}")"
        printf '"status":"%s",' "$(json_escape "${A_STATUS[$i]}")"
        printf '"detail":"%s"' "$(json_escape "${A_DETAIL[$i]}")"
        if sig="$(sig_for "${A_NAME[$i]}")"; then
            printf ',"signature":%s' "$sig"
        fi
        printf '}'
    done
    if [ "${#A_NAME[@]}" -gt 0 ]; then printf '\n  '; fi
    printf '],\n'
    printf '  "skipped": ['
    first=1
    for i in "${!S_NAME[@]}"; do
        if [ "$first" -eq 0 ]; then printf ','; fi
        first=0
        printf '\n    {"name":"%s","reason":"%s"}' \
            "$(json_escape "${S_NAME[$i]}")" "$(json_escape "${S_REASON[$i]}")"
    done
    if [ "${#S_NAME[@]}" -gt 0 ]; then printf '\n  '; fi
    printf ']\n'
    printf '}\n'
}

mkdir -p "$OUT_DIR" || exit 2
emit_manifest >"$MANIFEST.tmp" && mv "$MANIFEST.tmp" "$MANIFEST" || exit 2

# ---------------------------------------------------------------------------
# 4.5 Per-artifact OS-signature records (additive; digest-bound step 5).
# ---------------------------------------------------------------------------
SIGNATURES="$OUT_DIR/artifact-signatures.json"
emit_signatures() {
    local i first sig
    printf '{\n'
    printf '  "schema": "faktor-artifact-signatures/v1",\n'
    printf '  "commit": "%s",\n' "$(json_escape "$COMMIT")"
    printf '  "os": "%s",\n' "$(json_escape "$OS")"
    printf '  "arch": "%s",\n' "$(json_escape "$ARCH")"
    printf '  "timestamp": "%s",\n' "$(now_iso)"
    printf '  "artifacts": ['
    first=1
    for i in "${!A_NAME[@]}"; do
        sig="$(sig_for "${A_NAME[$i]}")" || continue
        if [ "$first" -eq 0 ]; then printf ','; fi
        first=0
        printf '\n    {'
        printf '"name":"%s",' "$(json_escape "${A_NAME[$i]}")"
        printf '"sha256":%s,' "$(if [ -n "${A_SHA[$i]}" ]; then printf '"%s"' "$(json_escape "${A_SHA[$i]}")"; else printf 'null'; fi)"
        printf '"status":"%s",' "$(json_escape "${A_STATUS[$i]}")"
        printf '"signature":%s' "$sig"
        printf '}'
    done
    if [ "${#A_NAME[@]}" -gt 0 ]; then printf '\n  '; fi
    printf ']\n'
    printf '}\n'
}
emit_signatures >"$SIGNATURES.tmp" && mv "$SIGNATURES.tmp" "$SIGNATURES" || exit 2

# ---------------------------------------------------------------------------
# 5. Signed update manifest (additive; assembled from this run).
# ---------------------------------------------------------------------------
UPDATE_MANIFEST="$OUT_DIR/update-manifest.json"
UPDATE_MANIFEST_OK=0
if [ "${PACKAGE_SKIP_UPDATE_MANIFEST:-0}" = "1" ]; then
    printf '[package] update-manifest: skipped (PACKAGE_SKIP_UPDATE_MANIFEST=1)\n'
elif ! command -v node >/dev/null 2>&1; then
    printf '[package] update-manifest: skipped (node unavailable; manifest not assembled)\n'
else
    if [ -n "${FAKTOR_UPDATE_SIGNING_KEY:-}" ]; then
        um() {
            node "$ROOT/scripts/update-manifest.mjs" --artifacts "$MANIFEST" \
                --certification-optional "$OUT_DIR/manifest.json" \
                --signatures "$SIGNATURES" \
                --out "$UPDATE_MANIFEST" --require-signed \
                ${REQUIRE_SIGNED_ARGS:-}
        }
    else
        um() {
            node "$ROOT/scripts/update-manifest.mjs" --artifacts "$MANIFEST" \
                --certification-optional "$OUT_DIR/manifest.json" \
                --signatures "$SIGNATURES" \
                --out "$UPDATE_MANIFEST" \
                ${REQUIRE_SIGNED_ARGS:-}
        }
    fi
    if [ "$REQUIRE_SIGNED" = "1" ]; then
        REQUIRE_SIGNED_ARGS="--require-signed-artifacts"
    else
        REQUIRE_SIGNED_ARGS=""
    fi
    if node "$ROOT/scripts/update-manifest.mjs" selftest \
        >"$LOG_DIR/package-update-manifest-selftest.log" 2>&1 &&
        um >>"$LOG_DIR/package-update-manifest.log" 2>&1; then
        UPDATE_MANIFEST_OK=1
        printf '[package] update-manifest: %s\n' "$(rel_path "$UPDATE_MANIFEST")"
    else
        detail="$(first_error_line "$LOG_DIR/package-update-manifest.log")"
        [ -n "$detail" ] || detail="update-manifest assembly failed"
        printf '[package] update-manifest: FAILED %s\n' "$detail" >&2
        FATAL=1
    fi
fi

# ---------------------------------------------------------------------------
# 6. Immutable release layout staging (additive): the tree the bootstrap
#    launcher authenticates. versions/<release-id>/faktor is byte-identical
#    to the raw daemon artifact and manifest is the signed update manifest;
#    the step refuses when the manifest does not carry the raw digest
#    (release-id = <version>-<raw sha256[..12]>, the exact Rust derivation).
# ---------------------------------------------------------------------------
if [ -z "$RAW_SHA" ] || [ "$RAW_SHA" = "unknown" ]; then
    printf '[package] release-layout: skipped (no raw daemon binary this run)\n'
elif [ "$UPDATE_MANIFEST_OK" != "1" ]; then
    printf '[package] release-layout: skipped (no update manifest this run)\n'
elif ! command -v node >/dev/null 2>&1; then
    printf '[package] release-layout: skipped (node unavailable; cannot bind the manifest)\n'
else
    RELEASE_ID="$VERSION-$(printf '%s' "$RAW_SHA" | cut -c1-12)"
    LAYOUT_DIR="$OUT_DIR/install/versions/$RELEASE_ID"
    if node -e '
        const fs = require("node:fs");
        const [file, digest] = process.argv.slice(1);
        const manifest = JSON.parse(fs.readFileSync(file, "utf8"));
        if (manifest.schema !== "faktor-update/v1") process.exit(1);
        if (!Array.isArray(manifest.artifacts)) process.exit(1);
        process.exit(manifest.artifacts.some((a) => a && a.sha256 === digest) ? 0 : 1);
    ' "$UPDATE_MANIFEST" "$RAW_SHA" 2>>"$LOG_DIR/package-release-layout.log"; then
        mkdir -p "$LAYOUT_DIR" || FATAL=1
        cp "$RAW_PATH" "$LAYOUT_DIR/faktor" && chmod +x "$LAYOUT_DIR/faktor" || FATAL=1
        cp "$UPDATE_MANIFEST" "$LAYOUT_DIR/manifest" || FATAL=1
        printf '[package] release-layout: %s (release-id %s)\n' \
            "$(rel_path "$LAYOUT_DIR")" "$RELEASE_ID"
    else
        printf '[package] release-layout: FAILED the manifest does not carry the raw digest %s\n' \
            "$RAW_SHA" >&2
        FATAL=1
    fi
fi

printf '\n[package] status=%s artifacts=%s recorded_skips=%s signature_records=%s manifest=%s\n' \
    "$STATUS" "${#A_NAME[@]}" "${#S_NAME[@]}" "${#SIG_NAME[@]}" "$(rel_path "$MANIFEST")"
if [ "${#A_NAME[@]}" -gt 0 ]; then
    for i in "${!A_NAME[@]}"; do
        sig_status="$(sig_for "${A_NAME[$i]}")"
        if [ -n "$sig_status" ]; then
            sig_status="$(printf '%s' "$sig_status" | sed -n 's/.*"status":"\([^"]*\)".*/\1/p' | head -n1)"
        else
            sig_status="none"
        fi
        printf '[package]   %-34s %-8s %12s bytes  %.12s  os-sig=%s\n' \
            "${A_NAME[$i]}" "${A_STATUS[$i]}" "${A_SIZE[$i]:-?}" "${A_SHA[$i]:-none}" "$sig_status"
    done
fi
if [ "$FATAL" -ne 0 ]; then
    exit 1
fi
exit 0

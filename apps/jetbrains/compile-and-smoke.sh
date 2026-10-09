#!/usr/bin/env bash
# JetBrains split-mode smoke:
#   1. build faktor-cli if missing
#   2. compile shared + backend + test + frontend (Swing panel) with kotlinc
#   3. run BackendSmoke (daemon lifecycle), NativeBridgeSmoke (native protocol
#      v1 + fake-server unit suite), FrontendSmoke (panels + canned frames)
#      and JetBrainsParitySmoke (Faktor-owned tree check + fake daemon +
#      parity families + the executable behavioral/visual parity matrix
#      artifact + real daemon restart/reconnect) against the real daemon;
#      exit 0/1
#   4. run JetBrainsHostMatrixSmoke against the BUILT plugin ZIP when one is
#      present (extract + extracted jars first on the classpath), else in
#      source-bundle mode; the trusted lane requires the ZIP
#      (FAKTOR_JETBRAINS_REQUIRE_PLUGIN_ZIP=1)
#
# The offscreen render runs under the pinned core-fonts fontconfig
# (frontend/src/test/resources/parity/fonts/core-fonts.conf) on Linux and a
# checkout-local user.home, so the pinned visual digest is comparable.
#
# When kotlinc is absent (dev hosts), the same smokes run on the
# Gradle-managed Kotlin/IntelliJ classpath instead:
#   ./gradlew :backend:smoke :frontend:smoke -PfaktorCliBin=<bin>
#   ./gradlew :frontend:smokeHostMatrixZip          # ZIP-hosted proof
# CI images that ship kotlinc keep the self-contained, network-free path.
#
# Flags:
#   --write-baselines  re-pin the visual matrix baselines from this render
#                      (apps/jetbrains/frontend/src/test/resources/parity/
#                      visual-baselines.json); normal runs compare against the
#                      pinned file and fail on drift
set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
JETBRAINS="$ROOT/apps/jetbrains"

# Pinned core-fonts environment for the offscreen Swing render: the visual
# digest derives component bounds from resolved font metrics, so the pinned
# linux baseline is only comparable under this environment (the CI image
# installs exactly fontconfig + fonts-dejavu-core). macOS/Windows render and
# record their own environment fingerprints.
CORE_FONTS_CONF="$JETBRAINS/frontend/src/test/resources/parity/fonts/core-fonts.conf"
if [ -f "$CORE_FONTS_CONF" ] && [ "$(uname -s)" = "Linux" ]; then
  export FONTCONFIG_FILE="$CORE_FONTS_CONF"
  echo "[compile-and-smoke] pinned core-fonts fontconfig: $CORE_FONTS_CONF"
fi

EXTRA_JVM_ARGS=""
for arg in "$@"; do
  case "$arg" in
    --write-baselines)
      EXTRA_JVM_ARGS="-Dfaktor.parity.writeBaselines=true"
      echo "[compile-and-smoke] writing pinned visual baselines from this render"
      ;;
    *)
      echo "FAIL: unknown flag: $arg" >&2
      exit 1
      ;;
  esac
done

SHARED_SRC="$JETBRAINS/shared/src/main/kotlin/dev/faktor/shared/Protocol.kt
$JETBRAINS/shared/src/main/kotlin/dev/faktor/shared/NativeProtocol.kt
$JETBRAINS/shared/src/main/kotlin/dev/faktor/shared/GeneratedProtocolDto.kt"
BACKEND_SRC="$JETBRAINS/backend/src/main/kotlin/dev/faktor/backend/BackendProcessManager.kt
$JETBRAINS/backend/src/main/kotlin/dev/faktor/backend/NativeClient.kt
$JETBRAINS/backend/src/main/kotlin/dev/faktor/backend/NativeEventStream.kt"
TEST_SRC="$JETBRAINS/backend/src/test/kotlin/dev/faktor/backend/BackendProcessManagerTest.kt
$JETBRAINS/backend/src/test/kotlin/dev/faktor/backend/NativeClientTest.kt
$JETBRAINS/frontend/src/test/kotlin/dev/faktor/frontend/FrontendTestSupport.kt
$JETBRAINS/frontend/src/test/kotlin/dev/faktor/frontend/JetBrainsParityMatrix.kt
$JETBRAINS/frontend/src/test/kotlin/dev/faktor/frontend/FrontendSmoke.kt
$JETBRAINS/frontend/src/test/kotlin/dev/faktor/frontend/ControlPlaneCredentialSmoke.kt
$JETBRAINS/frontend/src/test/kotlin/dev/faktor/frontend/JetBrainsHostMatrixSmoke.kt
$JETBRAINS/frontend/src/test/kotlin/dev/faktor/frontend/JetBrainsParitySmoke.kt"
FRONTEND_SRC="$JETBRAINS/frontend/src/main/kotlin/dev/faktor/frontend/FaktorFrontendService.kt
$JETBRAINS/frontend/src/main/kotlin/dev/faktor/frontend/ControlPlaneCredentials.kt
$JETBRAINS/frontend/src/main/kotlin/dev/faktor/frontend/FaktorChatPanel.kt
$JETBRAINS/frontend/src/main/kotlin/dev/faktor/frontend/PanelSupport.kt
$JETBRAINS/frontend/src/main/kotlin/dev/faktor/frontend/PixelAgents.kt
$JETBRAINS/frontend/src/main/kotlin/dev/faktor/frontend/TaskTreeModel.kt
$JETBRAINS/frontend/src/main/kotlin/dev/faktor/frontend/TaskTreePanel.kt
$JETBRAINS/frontend/src/main/kotlin/dev/faktor/frontend/BlockersPanel.kt
$JETBRAINS/frontend/src/main/kotlin/dev/faktor/frontend/TournamentPanel.kt
$JETBRAINS/frontend/src/main/kotlin/dev/faktor/frontend/BoardPanel.kt
$JETBRAINS/frontend/src/main/kotlin/dev/faktor/frontend/EvidenceNavigatorPanel.kt
$JETBRAINS/frontend/src/main/kotlin/dev/faktor/frontend/AttachmentsPanel.kt
$JETBRAINS/frontend/src/main/kotlin/dev/faktor/frontend/HistoryPanel.kt
$JETBRAINS/frontend/src/main/kotlin/dev/faktor/frontend/PermissionsPanel.kt
$JETBRAINS/frontend/src/main/kotlin/dev/faktor/frontend/SettingsPanel.kt
$JETBRAINS/frontend/src/main/kotlin/dev/faktor/frontend/UsagePanel.kt
$JETBRAINS/frontend/src/main/kotlin/dev/faktor/frontend/TerminalPanel.kt
$JETBRAINS/frontend/src/main/kotlin/dev/faktor/frontend/StatusPanel.kt
$JETBRAINS/frontend/src/main/kotlin/dev/faktor/frontend/AgentsPanel.kt"

BIN="${FAKTOR_CLI_BIN:-$ROOT/target/debug/faktor-cli}"

echo "[compile-and-smoke] repo root: $ROOT"

# ---- 1. the CLI binary -----------------------------------------------------
# Rebuild when the binary is missing OR any Rust source/Cargo manifest is
# newer than it: the smoke exercises additive native protocol fields (task-run
# `files`), so a stale binary would reject the current tree's requests.
needs_cli_rebuild() {
  [ ! -x "$BIN" ] && return 0
  local newer
  newer="$(find "$ROOT/crates" "$ROOT/Cargo.toml" "$ROOT/Cargo.lock" \
    -type f \( -name '*.rs' -o -name 'Cargo.toml' -o -name 'Cargo.lock' \) \
    -newer "$BIN" -print -quit 2>/dev/null)"
  [ -n "$newer" ]
}

if needs_cli_rebuild; then
  echo "[compile-and-smoke] building faktor-cli (missing or stale: $BIN)"
  (cd "$ROOT" && cargo build -p faktor-cli) || {
    echo "FAIL: cargo build -p faktor-cli" >&2
    exit 1
  }
fi
if [ ! -x "$BIN" ]; then
  echo "FAIL: $BIN missing or not executable after build" >&2
  exit 1
fi

# ---- 2. kotlinc, or the Gradle smoke tasks when it is absent ---------------
KOTLINC="${KOTLINC:-}"
if [ -z "$KOTLINC" ]; then
  KOTLINC="$(command -v kotlinc 2>/dev/null || true)"
fi

if [ -z "$KOTLINC" ]; then
  echo "[compile-and-smoke] kotlinc not found; running the Gradle smoke tasks"
  GRADLE_SMOKE_ARGS=(--console=plain --no-daemon "-PfaktorCliBin=$BIN")
  if [ -n "$EXTRA_JVM_ARGS" ]; then
    GRADLE_SMOKE_ARGS+=(-PwriteBaselines=true)
  fi
  (cd "$JETBRAINS" && ./gradlew "${GRADLE_SMOKE_ARGS[@]}" :backend:smoke :frontend:smoke) || exit $?
  # The packaged-plugin host proof on the same Gradle-managed classpath set:
  # buildPlugin + extract + run the host matrix with the ZIP jars first.
  (cd "$JETBRAINS" && ./gradlew --console=plain --no-daemon :frontend:smokeHostMatrixZip)
  exit $?
fi

# ---- 3. kotlin-stdlib.jar (bundled with the compiler distribution) ---------
# The test file is dependency-free (plain check/require, no kotlin.test),
# so only the stdlib is needed on the compile classpath. Both homebrew
# (opt/kotlin/libexec/lib) and the Ubuntu apt package
# (/usr/share/java, /usr/share/kotlin/kotlinc/lib) ship it next to kotlinc.
resolve_symlink() {
  local p="$1"
  while [ -L "$p" ]; do
    local dir
    dir="$(cd "$(dirname "$p")" && pwd -P)"
    local target
    target="$(ls -ld "$p" | sed 's/.* -> //')"
    case "$target" in
      /*) p="$target" ;;
      *) p="$dir/$target" ;;
    esac
  done
  echo "$p"
}

find_kotlin_stdlib_jar() {
  local j
  for j in \
    "${KOTLIN_STDLIB_JAR:-}" \
    "$(command -v brew >/dev/null 2>&1 && brew --prefix kotlin 2>/dev/null)/libexec/lib/kotlin-stdlib.jar" \
    "$(dirname "$(resolve_symlink "$KOTLINC")")/../lib/kotlin-stdlib.jar" \
    /usr/share/kotlin/kotlinc/lib/kotlin-stdlib.jar \
    /usr/lib/kotlin/kotlinc/lib/kotlin-stdlib.jar \
    /opt/kotlin/kotlinc/lib/kotlin-stdlib.jar; do
    if [ -n "$j" ] && [ -f "$j" ]; then
      echo "$j"
      return 0
    fi
  done
  return 1
}

STDLIB_JAR="$(find_kotlin_stdlib_jar || true)"
if [ -z "$STDLIB_JAR" ]; then
  echo "FAIL: kotlin-stdlib.jar not found next to kotlinc" >&2
  exit 1
fi
echo "[compile-and-smoke] kotlin-stdlib.jar: $STDLIB_JAR"

# ---- 4. JDK fallback for old apt kotlinc -----------------------------------
# kotlinc 1.3.31 (Ubuntu apt) cannot read class files from JDK >= 16. When
# the plain compile fails and an older JDK (<= 12) is installed, retry the
# compiler on it. The compiled jar targets 1.8 and still runs on any JVM.
jdk_major() {
  local v
  v="$("$1/bin/java" -version 2>&1 | head -1)"
  v="${v#*\"}"
  v="${v%%\"*}"
  case "$v" in
    1.*) v="${v#1.}" ;;
  esac
  v="${v%%.*}"
  echo "$v"
}

newest_jdk_at_most() {
  local limit="$1" best="" d v
  for d in /usr/lib/jvm/*/; do
    [ -x "$d/bin/java" ] || continue
    v="$(jdk_major "$d")"
    case "$v" in
      '' | *[!0-9]*) continue ;;
    esac
    if [ "$v" -le "$limit" ] && { [ -z "$best" ] || [ "$v" -gt "$(jdk_major "$best")" ]; }; then
      best="$d"
    fi
  done
  echo "$best"
}

OLD_JDK="$(newest_jdk_at_most 12)"

kotlinc_cmd() {
  local jdk="$1"
  shift
  if [ -n "$jdk" ]; then
    JAVA_HOME="$jdk" PATH="$jdk/bin:$PATH" "$KOTLINC" "$@"
  else
    "$KOTLINC" "$@"
  fi
}

# ---- 5. compile -------------------------------------------------------------
WORK="$(mktemp -d "${TMPDIR:-/tmp}/faktor-jb-smoke.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT
SMOKE_JAR="$WORK/smoke.jar"

# Source lists are newline-separated; word splitting below is intentional.
compile_kotlin() {
  local out rc
  out="$(kotlinc_cmd "" -classpath "$STDLIB_JAR" -include-runtime -d "$SMOKE_JAR" \
    $SHARED_SRC $BACKEND_SRC $TEST_SRC $FRONTEND_SRC 2>&1)"
  rc=$?
  if [ $rc -ne 0 ] && [ -n "$OLD_JDK" ]; then
    echo "[compile-and-smoke] plain kotlinc failed; retrying with $OLD_JDK" >&2
    out="$(kotlinc_cmd "$OLD_JDK" -classpath "$STDLIB_JAR" -include-runtime -d "$SMOKE_JAR" \
      $SHARED_SRC $BACKEND_SRC $TEST_SRC $FRONTEND_SRC 2>&1)"
    rc=$?
  fi
  if [ $rc -ne 0 ]; then
    echo "$out" >&2
  fi
  return $rc
}

echo "[compile-and-smoke] kotlinc: $KOTLINC"
compile_kotlin || {
  echo "FAIL: kotlinc compilation" >&2
  exit 1
}

# ---- 6. smokes against the real daemon --------------------------------------
# The smokes start the real daemon (daemon logs ride the Java process stderr).
# Capture that stderr so a failure prints a bounded daemon-log tail instead of
# only the Java-side message — a timed-out request is usually daemon-side.
run_smoke() {
  local name="$1"
  local err="$WORK/${name}.stderr"
  mkdir -p "$WORK/home"
  java -Dfaktor.repo.root="$ROOT" -Duser.home="$WORK/home" $EXTRA_JVM_ARGS -cp "$SMOKE_JAR" "$2" "$BIN" 2>"$err"
  local rc=$?
  if [ $rc -ne 0 ]; then
    echo "[compile-and-smoke] $name FAILED (rc=$rc); daemon stderr tail ($err):" >&2
    tail -n 40 "$err" >&2 || true
  fi
  return $rc
}

echo "[compile-and-smoke] running BackendSmoke (daemon lifecycle) against $BIN"
run_smoke BackendSmoke dev.faktor.backend.BackendSmoke || exit $?

echo "[compile-and-smoke] running NativeBridgeSmoke (native protocol v1) against $BIN"
run_smoke NativeBridgeSmoke dev.faktor.backend.NativeBridgeSmoke || exit $?

echo "[compile-and-smoke] running FrontendSmoke (panels + canned native JSON + real daemon) against $BIN"
run_smoke FrontendSmoke dev.faktor.frontend.FrontendSmoke || exit $?

echo "[compile-and-smoke] running ControlPlaneCredentialSmoke (fake PasswordSafe rows) against $BIN"
run_smoke ControlPlaneCredentialSmoke dev.faktor.frontend.ControlPlaneCredentialSmoke || exit $?

echo "[compile-and-smoke] running JetBrainsParitySmoke (pin hashes + fake daemon + real daemon parity) against $BIN"
run_smoke JetBrainsParitySmoke dev.faktor.frontend.JetBrainsParitySmoke || exit $?

# ---- 7. host matrix: the BUILT plugin ZIP ----------------------------------
# Extracts the built plugin ZIP and runs the host matrix with the ZIP's
# `faktor/lib/*.jar` FIRST on the classpath, so the panel classes under test
# are the shipped ones (the smoke asserts the class provenance). The trusted
# `jetbrains-smoke` lane sets FAKTOR_JETBRAINS_REQUIRE_PLUGIN_ZIP=1, which
# makes a missing ZIP a typed failure instead of a silent source-only run.
PLUGIN_ZIP="${FAKTOR_JETBRAINS_PLUGIN_ZIP:-}"
if [ -z "$PLUGIN_ZIP" ]; then
  PLUGIN_ZIP="$(ls -t "$JETBRAINS"/frontend/build/distributions/*.zip 2>/dev/null | head -n 1 || true)"
fi
HOST_MATRIX_CP="$SMOKE_JAR"
HOST_MATRIX_ARGS=""
if [ -n "$PLUGIN_ZIP" ]; then
  echo "[compile-and-smoke] host matrix: extracting the built plugin ZIP $PLUGIN_ZIP"
  mkdir -p "$WORK/plugin-zip"
  if command -v unzip >/dev/null 2>&1; then
    unzip -q "$PLUGIN_ZIP" -d "$WORK/plugin-zip"
  else
    JAR_BIN="$(dirname "$(command -v java)")/jar"
    (cd "$WORK/plugin-zip" && "$JAR_BIN" xf "$PLUGIN_ZIP") || {
      echo "FAIL: cannot extract $PLUGIN_ZIP (unzip absent and jar extraction failed)" >&2
      exit 1
    }
  fi
  HOST_MATRIX_CP="$WORK/plugin-zip/faktor/lib/*:$SMOKE_JAR"
  HOST_MATRIX_ARGS="-Dfaktor.hostMatrix.zip=$PLUGIN_ZIP -Dfaktor.hostMatrix.requireZip=true"
else
  if [ "${FAKTOR_JETBRAINS_REQUIRE_PLUGIN_ZIP:-0}" = "1" ]; then
    echo "FAIL: jetbrains-plugin-zip-missing: FAKTOR_JETBRAINS_REQUIRE_PLUGIN_ZIP=1 but no built plugin ZIP was found (run ./gradlew :frontend:buildPlugin first); the host matrix must run against the shipped artifact" >&2
    exit 1
  fi
  echo "[compile-and-smoke] host matrix: no built plugin ZIP; running source-bundle mode (build :frontend:buildPlugin for the shipped-artifact proof)"
fi
mkdir -p "$WORK/home"
echo "[compile-and-smoke] running JetBrainsHostMatrixSmoke (width/zoom/theme/keyboard-only/states)"
java -Dfaktor.repo.root="$ROOT" -Duser.home="$WORK/home" $HOST_MATRIX_ARGS -cp "$HOST_MATRIX_CP" dev.faktor.frontend.JetBrainsHostMatrixSmoke 2>"$WORK/HostMatrix.stderr"
HOST_RC=$?
if [ $HOST_RC -ne 0 ]; then
  echo "[compile-and-smoke] JetBrainsHostMatrixSmoke FAILED (rc=$HOST_RC); stderr tail:" >&2
  tail -n 40 "$WORK/HostMatrix.stderr" >&2 || true
  exit $HOST_RC
fi
exit 0

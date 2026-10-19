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
# The compiler there is the standalone kotlinc pinned to the Gradle Kotlin
# plugin version (docker/faktor-ci/Dockerfile), matching what `./gradlew`
# compiles; the sources use Kotlin 1.5+ stdlib calls (`String.lowercase`,
# `Char.code`), so the old apt kotlin 1.3.31 package cannot compile them.
#
# Toolchain heaps are BOUNDED and explicit (the jetbrains-smoke run-66 OOM
# class: under concurrent lane pressure the stock kotlinc launcher default
# -Xmx512M exhausts the JVM compiling this tree):
#   * kotlinc runs with JAVA_OPTS="-Xmx2g -Xms256m" (env override:
#     FAKTOR_JETBRAINS_KOTLINC_JAVA_OPTS); the value is printed and folded
#     into failure diagnostics.
#   * the Gradle fallback pins the Kotlin daemon heap additively in
#     gradle.properties (kotlin.daemon.jvmargs=-Xmx2g; the Gradle daemon
#     itself keeps its existing org.gradle.jvmargs=-Xmx3g).
# An OOM / heap-exhaustion failure in ANY toolchain step (kotlinc, Gradle,
# the smoke JVMs, the host matrix) is reported as a TYPED
# `environment-resource:` failure with the captured diagnostics tail and a
# signal-kill classification; it is still a hard gate failure, never a
# silent crash or a pass.
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

# ---- flags -----------------------------------------------------------------
# `--smoke <Label>` (repeatable, labels: Backend, NativeBridge, Frontend,
# ControlPlaneCredential, JetBrainsParity, HostMatrix) runs ONLY the named
# executable smoke(s) on the same compiled tree; no selection runs the full
# suite the trusted lane proves. The planted-mutation gates in
# scripts/mutations/INV-*.json use the focused form so each gate runs exactly
# its own oracle on the pinned CI image's self-contained kotlinc toolchain
# (the image ships no warm Gradle cache, so the Gradle tasks cannot run
# `--offline` there).
EXTRA_JVM_ARGS=""
SMOKE_SELECTION=()
KNOWN_SMOKES="Backend NativeBridge Frontend ControlPlaneCredential JetBrainsParity HostMatrix EdtWork ControlExercise Acceptance Edition8ControlExercise Edition8Acceptance Edition8Workflow Edition8Evidence Edition8DaemonDogfood Edition8LiveAcceptance Visual"
while [ $# -gt 0 ]; do
  case "$1" in
    --write-baselines)
      EXTRA_JVM_ARGS="-Dfaktor.parity.writeBaselines=true"
      echo "[compile-and-smoke] writing pinned visual baselines from this render"
      shift
      ;;
    --smoke)
      if [ $# -lt 2 ]; then
        echo "FAIL: --smoke needs one label from: $KNOWN_SMOKES" >&2
        exit 1
      fi
      case " $KNOWN_SMOKES " in
        *" $2 "*) SMOKE_SELECTION=(${SMOKE_SELECTION[@]+"${SMOKE_SELECTION[@]}"} "$2") ;;
        *)
          echo "FAIL: unknown smoke label '$2' (expected one of: $KNOWN_SMOKES)" >&2
          exit 1
          ;;
      esac
      shift 2
      ;;
    *)
      echo "FAIL: unknown flag: $1" >&2
      exit 1
      ;;
  esac
done

# TRUE when the label is selected (an empty selection means the full suite).
smoke_selected() { # label
  [ ${#SMOKE_SELECTION[@]} -eq 0 ] && return 0
  local label
  for label in ${SMOKE_SELECTION[@]+"${SMOKE_SELECTION[@]}"}; do
    [ "$label" = "$1" ] && return 0
  done
  return 1
}

# ---- bounded toolchain heaps + typed environment-resource failures ---------
# kotlinc's stock launcher defaults to -Xmx512M; under concurrent lane
# pressure (the jetbrains-smoke run-66 OOM) that exhausts the JVM compiling
# this tree. The bound is explicit and overridable for experiments; the
# Gradle/Kotlin-daemon equivalents live in gradle.properties
# (kotlin.daemon.jvmargs) next to the existing org.gradle.jvmargs.
KOTLINC_JAVA_OPTS="${FAKTOR_JETBRAINS_KOTLINC_JAVA_OPTS:--Xmx2g -Xms256m}"

# JVM heap-exhaustion / host-allocator markers (bounded grep, first match).
ENV_RESOURCE_PATTERN='OutOfMemoryError|Java heap space|GC overhead limit exceeded|unable to create native thread|Cannot allocate memory|Could not reserve enough space|Too small maximum heap'

# On a toolchain failure: always print the captured diagnostics tail, and
# when the failure is classified as an environment-resource failure (a JVM
# OOM marker, or a signal kill such as the host OOM killer's SIGKILL),
# additionally emit the typed `environment-resource:` reason. This is
# diagnostic classification only — the step remains a hard failure.
report_toolchain_failure() { # label log rc
  local label="$1" log="$2" rc="$3" marker klass
  echo "[compile-and-smoke] $label FAILED (rc=$rc); diagnostics tail ($log):" >&2
  tail -n 40 "$log" >&2 2>/dev/null || true
  echo "[compile-and-smoke] bounded heaps: kotlinc JAVA_OPTS=$KOTLINC_JAVA_OPTS; gradle kotlin.daemon.jvmargs (gradle.properties)" >&2
  marker="$(grep -m 1 -E "$ENV_RESOURCE_PATTERN" "$log" 2>/dev/null | head -c 400 || true)"
  klass=""
  if [ -n "$marker" ]; then
    klass="java-out-of-memory"
  elif [ "$rc" -ge 128 ]; then
    klass="process-killed-by-signal"
  fi
  if [ -n "$klass" ]; then
    echo "FAIL: environment-resource: $label classified '$klass' (rc=$rc): ${marker:-no Java marker captured; the tool ran out of host memory or was killed, not a code/compile error}" >&2
  else
    echo "FAIL: $label (rc=$rc)" >&2
  fi
}

SHARED_SRC="$JETBRAINS/shared/src/main/kotlin/dev/faktor/shared/Protocol.kt
$JETBRAINS/shared/src/main/kotlin/dev/faktor/shared/NativeProtocol.kt
$JETBRAINS/shared/src/main/kotlin/dev/faktor/shared/GeneratedProtocolDto.kt"
BACKEND_SRC="$JETBRAINS/backend/src/main/kotlin/dev/faktor/backend/BackendProcessManager.kt
$JETBRAINS/backend/src/main/kotlin/dev/faktor/backend/DaemonBinary.kt
$JETBRAINS/backend/src/main/kotlin/dev/faktor/backend/NativeClient.kt
$JETBRAINS/backend/src/main/kotlin/dev/faktor/backend/NativeEventStream.kt"
TEST_SRC="$JETBRAINS/backend/src/test/kotlin/dev/faktor/backend/BackendProcessManagerTest.kt
$JETBRAINS/backend/src/test/kotlin/dev/faktor/backend/NativeClientTest.kt
$JETBRAINS/frontend/src/test/kotlin/dev/faktor/frontend/FrontendTestSupport.kt
$JETBRAINS/frontend/src/test/kotlin/dev/faktor/frontend/JetBrainsParityMatrix.kt
$JETBRAINS/frontend/src/test/kotlin/dev/faktor/frontend/FrontendSmoke.kt
$JETBRAINS/frontend/src/test/kotlin/dev/faktor/frontend/ControlPlaneCredentialSmoke.kt
$JETBRAINS/frontend/src/test/kotlin/dev/faktor/frontend/JetBrainsHostMatrixSmoke.kt
$JETBRAINS/frontend/src/test/kotlin/dev/faktor/frontend/JetBrainsParitySmoke.kt
$JETBRAINS/frontend/src/test/kotlin/dev/faktor/frontend/EdtWorkHarness.kt
$JETBRAINS/frontend/src/test/kotlin/dev/faktor/frontend/Edition5ControlExercise.kt
$JETBRAINS/frontend/src/test/kotlin/dev/faktor/frontend/Edition5AcceptanceSmoke.kt
$JETBRAINS/frontend/src/test/kotlin/dev/faktor/frontend/Edition8ControlExercise.kt
$JETBRAINS/frontend/src/test/kotlin/dev/faktor/frontend/Edition8AcceptanceSmoke.kt
$JETBRAINS/frontend/src/test/kotlin/dev/faktor/frontend/Edition8WorkflowSmoke.kt
$JETBRAINS/frontend/src/test/kotlin/dev/faktor/frontend/Edition8Evidence.kt
$JETBRAINS/frontend/src/test/kotlin/dev/faktor/frontend/Edition8DaemonDogfood.kt
$JETBRAINS/frontend/src/test/kotlin/dev/faktor/frontend/Edition8LiveAcceptance.kt"
FRONTEND_SRC="$JETBRAINS/frontend/src/main/kotlin/dev/faktor/frontend/Edition5CatalogData.kt
$JETBRAINS/frontend/src/main/kotlin/dev/faktor/frontend/Edition5Catalog.kt
$JETBRAINS/frontend/src/main/kotlin/dev/faktor/frontend/Edition5ConfigSource.kt
$JETBRAINS/frontend/src/main/kotlin/dev/faktor/frontend/Edition5Settings.kt
$JETBRAINS/frontend/src/main/kotlin/dev/faktor/frontend/Edition5Panels.kt
$JETBRAINS/frontend/src/main/kotlin/dev/faktor/frontend/Edition5ScreenRegistry.kt
$JETBRAINS/frontend/src/main/kotlin/dev/faktor/frontend/Edition8ScreenRegistry.kt
$JETBRAINS/frontend/src/main/kotlin/dev/faktor/frontend/Edition8Daemon.kt
$JETBRAINS/frontend/src/main/kotlin/dev/faktor/frontend/Edition8Panels.kt
$JETBRAINS/frontend/src/main/kotlin/dev/faktor/frontend/ModelPickerDialog.kt
$JETBRAINS/frontend/src/main/kotlin/dev/faktor/frontend/FaktorFrontendService.kt
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
$JETBRAINS/frontend/src/main/kotlin/dev/faktor/frontend/ChangesPanel.kt
$JETBRAINS/frontend/src/main/kotlin/dev/faktor/frontend/ChangeReview.kt
$JETBRAINS/frontend/src/main/kotlin/dev/faktor/frontend/OutcomesPanel.kt
$JETBRAINS/frontend/src/main/kotlin/dev/faktor/frontend/FaktorSettingsModel.kt
$JETBRAINS/frontend/src/main/kotlin/dev/faktor/frontend/PermissionsPanel.kt
$JETBRAINS/frontend/src/main/kotlin/dev/faktor/frontend/ApprovalCards.kt
$JETBRAINS/frontend/src/main/kotlin/dev/faktor/frontend/SettingsPanel.kt
$JETBRAINS/frontend/src/main/kotlin/dev/faktor/frontend/UsagePanel.kt
$JETBRAINS/frontend/src/main/kotlin/dev/faktor/frontend/TerminalPanel.kt
$JETBRAINS/frontend/src/main/kotlin/dev/faktor/frontend/DiagnosticsPanel.kt
$JETBRAINS/frontend/src/main/kotlin/dev/faktor/frontend/AgentsPanel.kt
$JETBRAINS/frontend/src/main/kotlin/dev/faktor/frontend/OverviewPanel.kt
$JETBRAINS/frontend/src/main/kotlin/dev/faktor/frontend/VerificationPanel.kt
$JETBRAINS/frontend/src/main/kotlin/dev/faktor/frontend/UiWork.kt"

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

# A checksum record beside a LOCAL cargo build output (target/...) is derived
# state: the rebuild above invalidates it. Reconcile that local record so the
# product trust gate (which still refuses any genuine mismatch at start) sees
# the rebuilt bytes. Records next to operator-supplied binaries outside
# target/ are never rewritten: their fail-closed refusal stands.
sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d' ' -f1
  elif command -v openssl >/dev/null 2>&1; then
    openssl dgst -sha256 "$1" | sed 's/.*= //'
  else
    return 1
  fi
}
case "$BIN" in
  "$ROOT"/target/*)
    if [ -f "$BIN.sha256" ]; then
      expected="$(cut -d' ' -f1 "$BIN.sha256" 2>/dev/null || true)"
      actual="$(sha256_of "$BIN" || true)"
      if [ -z "$actual" ]; then
        echo "FAIL: cannot hash $BIN to reconcile $BIN.sha256" >&2
        exit 1
      fi
      if [ "$expected" != "$actual" ]; then
        echo "[compile-and-smoke] refreshing local build checksum record $BIN.sha256 (stale for the rebuilt binary)"
        printf '%s  %s\n' "$actual" "$(basename "$BIN")" > "$BIN.sha256"
      fi
    fi
    ;;
esac

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
  # Capture the full output to a log (tee keeps it streaming for CI) so an
  # OOM/kill inside the Gradle/Kotlin-daemon toolchain is classified typed
  # instead of surfacing as an opaque rc.
  run_gradle_smoke() { # label args...
    local label="$1"
    shift
    local log rc=0
    log="$(mktemp "${TMPDIR:-/tmp}/faktor-jb-gradle.XXXXXX.log")"
    (cd "$JETBRAINS" && ./gradlew "$@" 2>&1 | tee "$log"; exit "${PIPESTATUS[0]}") || rc=$?
    if [ "$rc" -ne 0 ]; then
      report_toolchain_failure "$label" "$log" "$rc"
      rm -f "$log"
      return "$rc"
    fi
    rm -f "$log"
    return 0
  }
  if [ ${#SMOKE_SELECTION[@]} -eq 0 ]; then
    run_gradle_smoke "gradle :backend:smoke :frontend:smoke" \
      "${GRADLE_SMOKE_ARGS[@]}" :backend:smoke :frontend:smoke || exit $?
    # The packaged-plugin host proof on the same Gradle-managed classpath set:
    # buildPlugin + extract + run the host matrix with the ZIP jars first.
    run_gradle_smoke "gradle :frontend:smokeHostMatrixZip" \
      --console=plain --no-daemon :frontend:smokeHostMatrixZip
    exit $?
  fi
  for label in ${SMOKE_SELECTION[@]+"${SMOKE_SELECTION[@]}"}; do
    case "$label" in
      Backend|NativeBridge) task=":backend:smoke$label" ;;
      HostMatrix) task=":frontend:smokeHostMatrixZip" ;;
      *) task=":frontend:smoke$label" ;;
    esac
    run_gradle_smoke "gradle $task" "${GRADLE_SMOKE_ARGS[@]}" "$task" || exit $?
  done
  exit 0
fi

# ---- 3. kotlin-stdlib.jar (bundled with the compiler distribution) ---------
# The test file is dependency-free (plain check/require, no kotlin.test),
# so only the stdlib is needed on the compile classpath. Both homebrew
# (opt/kotlin/libexec/lib) and distribution layouts
# (/opt/kotlinc/lib in the CI image; /usr/share/kotlin/kotlinc/lib,
# /usr/lib/kotlin/kotlinc/lib on dev hosts) ship it next to kotlinc.
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

# ---- 4. compile -------------------------------------------------------------
WORK="$(mktemp -d "${TMPDIR:-/tmp}/faktor-jb-smoke.XXXXXX")"
# Zero orphans: the lifecycle tests deliberately spawn fake releases/launchers
# (a launcher that exits early leaves its release running so ownership transfer
# can be asserted); anything the run itself leaked is reaped here by the exact
# classpath, so no smoke run — passing or failing — leaves a child behind.
trap 'pkill -f "$SMOKE_JAR" >/dev/null 2>&1 || true; rm -rf "$WORK"' EXIT
SMOKE_JAR="$WORK/smoke.jar"
KOTLINC_LOG="$WORK/kotlinc.log"

# Source lists are newline-separated; word splitting below is intentional.
# Output is captured into the log file so a heap-exhaustion failure is
# classified typed with its diagnostics (see report_toolchain_failure).
compile_kotlin() {
  local out rc
  out="$(JAVA_OPTS="$KOTLINC_JAVA_OPTS" "$KOTLINC" -classpath "$STDLIB_JAR" -include-runtime -d "$SMOKE_JAR" \
    $SHARED_SRC $BACKEND_SRC $TEST_SRC $FRONTEND_SRC 2>&1)"
  rc=$?
  printf '%s\n' "$out" >"$KOTLINC_LOG"
  return $rc
}

echo "[compile-and-smoke] kotlinc: $KOTLINC (JAVA_OPTS=$KOTLINC_JAVA_OPTS)"
compile_kotlin
COMPILE_RC=$?
if [ "$COMPILE_RC" -ne 0 ]; then
  report_toolchain_failure "kotlinc compilation" "$KOTLINC_LOG" "$COMPILE_RC"
  exit 1
fi

# ---- 5. smokes against the real daemon --------------------------------------
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
    report_toolchain_failure "$name (daemon stderr)" "$err" "$rc"
  fi
  return $rc
}

if smoke_selected Backend; then
  echo "[compile-and-smoke] running BackendSmoke (daemon lifecycle) against $BIN"
  run_smoke BackendSmoke dev.faktor.backend.BackendSmoke || exit $?
fi

if smoke_selected NativeBridge; then
  echo "[compile-and-smoke] running NativeBridgeSmoke (native protocol v1) against $BIN"
  run_smoke NativeBridgeSmoke dev.faktor.backend.NativeBridgeSmoke || exit $?
fi

if smoke_selected Frontend; then
  echo "[compile-and-smoke] running FrontendSmoke (panels + canned native JSON + real daemon) against $BIN"
  run_smoke FrontendSmoke dev.faktor.frontend.FrontendSmoke || exit $?
fi

if smoke_selected ControlPlaneCredential; then
  echo "[compile-and-smoke] running ControlPlaneCredentialSmoke (fake PasswordSafe rows) against $BIN"
  run_smoke ControlPlaneCredentialSmoke dev.faktor.frontend.ControlPlaneCredentialSmoke || exit $?
fi

if smoke_selected JetBrainsParity; then
  echo "[compile-and-smoke] running JetBrainsParitySmoke (pin hashes + fake daemon + real daemon parity) against $BIN"
  run_smoke JetBrainsParitySmoke dev.faktor.frontend.JetBrainsParitySmoke || exit $?
fi

if smoke_selected EdtWork; then
  echo "[compile-and-smoke] running EdtWorkHarness (10k-token stream / 500-message transcript / 200 deltas/s / 1k rows)"
  run_smoke EdtWorkHarness dev.faktor.frontend.EdtWorkHarness || exit $?
fi

if smoke_selected ControlExercise; then
  echo "[compile-and-smoke] running Edition5ControlExercise (all 200 controls through the real Swing panels)"
  run_smoke ControlExercise dev.faktor.frontend.Edition5ControlExercise || exit $?
fi

if smoke_selected Acceptance; then
  echo "[compile-and-smoke] running Edition5AcceptanceSmoke (A01-A40 host observations)"
  run_smoke Acceptance dev.faktor.frontend.Edition5AcceptanceSmoke || exit $?
fi

if smoke_selected Edition8ControlExercise; then
  echo "[compile-and-smoke] running Edition8ControlExercise (all 200 controls / 72 screens)"
  run_smoke Edition8ControlExercise dev.faktor.frontend.Edition8ControlExercise || exit $?
fi

if smoke_selected Edition8Acceptance; then
  echo "[compile-and-smoke] running Edition8AcceptanceSmoke (A01-A40 host overlays)"
  run_smoke Edition8Acceptance dev.faktor.frontend.Edition8AcceptanceSmoke || exit $?
fi

if smoke_selected Edition8Workflow; then
  echo "[compile-and-smoke] running Edition8WorkflowSmoke (W01-W16 x 5 cells)"
  run_smoke Edition8Workflow dev.faktor.frontend.Edition8WorkflowSmoke || exit $?
fi

if smoke_selected Edition8DaemonDogfood; then
  echo "[compile-and-smoke] running Edition8DaemonDogfood (real daemon)"
  run_smoke Edition8DaemonDogfood dev.faktor.frontend.Edition8DaemonDogfood || exit $?
fi

if smoke_selected Edition8LiveAcceptance; then
  echo "[compile-and-smoke] running Edition8LiveAcceptance (A01-A40 against the real daemon)"
  run_smoke Edition8LiveAcceptance dev.faktor.frontend.Edition8LiveAcceptance || exit $?
fi

if smoke_selected Edition8Evidence; then
  echo "[compile-and-smoke] running Edition8Evidence (optical/contrast/a11y evidence)"
  run_smoke Edition8Evidence dev.faktor.frontend.Edition8Evidence || exit $?
fi

if smoke_selected HostMatrix || smoke_selected Visual; then
# ---- 6. host matrix: the BUILT plugin ZIP ----------------------------------
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
java -Dfaktor.repo.root="$ROOT" -Duser.home="$WORK/home" $EXTRA_JVM_ARGS $HOST_MATRIX_ARGS -cp "$HOST_MATRIX_CP" dev.faktor.frontend.JetBrainsHostMatrixSmoke 2>"$WORK/HostMatrix.stderr"
HOST_RC=$?
if [ $HOST_RC -ne 0 ]; then
  report_toolchain_failure "JetBrainsHostMatrixSmoke" "$WORK/HostMatrix.stderr" "$HOST_RC"
  exit $HOST_RC
fi
fi  # smoke_selected HostMatrix/Visual
exit 0

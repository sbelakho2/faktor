#!/usr/bin/env bash
# HAR-16 (two-IDE-version matrix): run the real-IDE JetBrains frontend proof
# against BOTH supported IntelliJ Platform releases, sequentially, writing
# PER-VERSION host-matrix / parity / acceptance artifacts (the canonical
# pinned-release files are never clobbered).
#
# Legs (each runs `:frontend:smoke` + `:frontend:test`):
#
#   leg 1 — default pin (no property, byte-identical to the existing single-
#     version runs):
#       ./gradlew --console=plain --no-daemon [:opts] :frontend:smoke :frontend:test
#     -> target/certification/jetbrains-host-matrix.json (canonical)
#       target/certification/jetbrains-parity.json (canonical; only with
#       --cli/FAKTOR_CLI_BIN — a canned-only parity run writes no artifact,
#       the pre-existing contract)
#
#   leg 2 — explicitly selected OLDER stable release:
#       ./gradlew --console=plain --no-daemon [:opts] \
#         --write-verification-metadata sha256 \
#         -Pfaktor.ideaVersion=<alt> -Pfaktor.ideaVisual=structural \
#         :frontend:smoke :frontend:test
#     -> target/certification/jetbrains-host-matrix-<alt>.json
#       target/certification/jetbrains-parity-<alt>.json (with --cli)
#       (acceptance overlays suffixed the same way)
#
# Integrity contract for leg 2 (documented, fail-closed, tree-restoring):
#   * the committed gradle.lockfile pins idea:ideaIC:<pin>; the alternate
#     release resolves a different IDE set, so frontend/build.gradle.kts
#     deactivates dependency locking for that leg's configurations only
#     (backend/shared keep their locks; the PIN leg keeps full locking).
#   * gradle/verification-metadata.xml contains checksums for the pin's
#     artifacts only, so leg 2 refreshes it with `--write-verification-metadata
#     sha256` (the downloaded alternate IDE gets checksummed AS it is
#     verified for this run) and this script RESTORES the committed file
#     afterwards — trap-guarded, so a failed leg never leaves the tree dirty.
#   * `-Pfaktor.ideaVisual=structural`: visual/digest certification claims
#     (platform x fonts x PINNED IDE) are refused under another release — the
#     environment fingerprint carries no IDE release, so second-release
#     digests can neither compare against nor be written into the shared
#     per-platform baselines (JetBrainsParityMatrix + the host matrix enforce
#     this fail-closed; structural checks, canned parity rows, widths/zooms/
#     themes/keyboard/state assertions all still run).
#
# Cost: the alternate leg downloads ONE extra IDE distribution (~700-900 MB)
# into the gradle plugin cache (apps/jetbrains/build/intellijPlatform —
# project-local, so CI re-downloads per clean workspace; gradle's ~/.gradle
# caches still cover Maven artifacts). Runtime roughly doubles.
#
# Usage:
#   bash apps/jetbrains/two-ide-matrix.sh                  # both legs
#   bash apps/jetbrains/two-ide-matrix.sh --only-pin       # leg 1 only
#   bash apps/jetbrains/two-ide-matrix.sh --only-alt       # leg 2 only (CI
#         uses this after its existing byte-identical pin leg)
#   bash apps/jetbrains/two-ide-matrix.sh --alt 2024.1     # pick the release
#   bash apps/jetbrains/two-ide-matrix.sh --dry-run        # gradle --dry-run
#         (task-graph/property plumbing proof; no execution, no download)
#   FAKTOR_CLI_BIN=<path>  or  --cli <path>                # real-daemon parity
#
# Exit codes: 0 all requested legs passed; 1 a leg failed; 2 usage/setup error.
set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
JETBRAINS="$SCRIPT_DIR"

PIN=""
ALT="${FAKTOR_IDEA_ALT:-2024.1}"
RUN_PIN=1
RUN_ALT=1
DRY_RUN=""
CLI_BIN="${FAKTOR_CLI_BIN:-}"

usage() { sed -n '2,/^set -/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; }

while [ $# -gt 0 ]; do
  case "$1" in
    --only-pin) RUN_ALT=0; shift ;;
    --only-alt) RUN_PIN=0; shift ;;
    --alt)
      [ $# -ge 2 ] || { echo "two-ide-matrix: --alt needs a release (e.g. 2024.1)" >&2; exit 2; }
      ALT="$2"; shift 2 ;;
    --cli)
      [ $# -ge 2 ] || { echo "two-ide-matrix: --cli needs a binary path" >&2; exit 2; }
      CLI_BIN="$2"; shift 2 ;;
    --dry-run) DRY_RUN="--dry-run"; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "two-ide-matrix: unknown argument: $1" >&2; exit 2 ;;
  esac
done

[ "$RUN_PIN" -eq 1 ] || [ "$RUN_ALT" -eq 1 ] || {
  echo "two-ide-matrix: --only-pin and --only-alt are mutually exclusive" >&2; exit 2
}

PIN="$(sed -n 's/^faktor\.ideaVersion\.pin=//p' "$JETBRAINS/gradle.properties" | head -n 1 | tr -d '[:space:]')"
if [ -z "$PIN" ]; then
  echo "two-ide-matrix: faktor.ideaVersion.pin missing from $JETBRAINS/gradle.properties (fail closed)" >&2
  exit 2
fi
if [ "$RUN_ALT" -eq 1 ] && [ "$ALT" = "$PIN" ]; then
  echo "two-ide-matrix: alternate release '$ALT' equals the pin '$PIN'; the two legs would be identical (fail closed)" >&2
  exit 2
fi
case "$ALT" in
  *[!A-Za-z0-9._-]*|.*|*..*) echo "two-ide-matrix: alternate release '$ALT' is not a safe release id" >&2; exit 2 ;;
esac

GRADLE_ARGS=(--console=plain --no-daemon)
if [ -n "$DRY_RUN" ]; then GRADLE_ARGS+=("$DRY_RUN"); fi
if [ -n "$CLI_BIN" ]; then GRADLE_ARGS+=("-PfaktorCliBin=$CLI_BIN"); fi

# trap-guarded restore of the committed verification metadata (leg 2 rewrites
# it via --write-verification-metadata; a failed run must not leave it dirty).
VM_FILE="$JETBRAINS/gradle/verification-metadata.xml"
VM_SNAPSHOT=""
restore_vm() {
  if [ -n "$VM_SNAPSHOT" ] && [ -f "$VM_SNAPSHOT" ]; then
    cp "$VM_SNAPSHOT" "$VM_FILE" || {
      echo "two-ide-matrix: FAILED to restore $VM_FILE (fail closed)" >&2
    }
    rm -f "$VM_SNAPSHOT"
    VM_SNAPSHOT=""
    echo "[two-ide-matrix] restored committed gradle/verification-metadata.xml"
  fi
}
trap restore_vm EXIT

require_artifact() { # path leg
  local path="$1" leg="$2"
  if [ -n "$DRY_RUN" ]; then
    echo "[two-ide-matrix] $leg artifact check skipped (--dry-run): $path"
    return 0
  fi
  if [ ! -f "$path" ]; then
    echo "two-ide-matrix: $leg wrote no artifact at $path (fail closed)" >&2
    return 1
  fi
  echo "[two-ide-matrix] $leg artifact: $path"
  return 0
}

summary_line() { # log
  grep -E 'JETBRAINS (HOST MATRIX|PARITY)' "$1" 2>/dev/null | tail -n 6 || true
}

run_leg() { # name extra-gradle-args... -- tasks...
  local name="$1"; shift
  local extra=()
  while [ $# -gt 0 ] && [ "$1" != "--" ]; do extra+=("$1"); shift; done
  [ "${1:-}" = "--" ] && shift
  local log rc=0
  log="$(mktemp "${TMPDIR:-/tmp}/faktor-two-ide-matrix.XXXXXX.log")"
  echo "[two-ide-matrix] === $name === ./gradlew ${GRADLE_ARGS[*]} ${extra[*]} $*"
  (
    cd "$JETBRAINS" &&
      ./gradlew "${GRADLE_ARGS[@]}" ${extra[@]+"${extra[@]}"} "$@" 2>&1 |
      tee "$log"
    exit "${PIPESTATUS[0]}"
  ) || rc=$?
  if [ "$rc" -ne 0 ]; then
    echo "two-ide-matrix: $name FAILED (rc=$rc); diagnostics tail ($log):" >&2
    tail -n 40 "$log" >&2 2>/dev/null || true
    rm -f "$log"
    return "$rc"
  fi
  summary_line "$log"
  rm -f "$log"
  return 0
}

ROOT="$(cd "$JETBRAINS/../.." && pwd)"

# The parity MATRIX (and therefore jetbrains-parity[-release].json) only runs
# when the smoke receives a faktor-cli binary; a cli-less run is the honest
# "canned + pin only" mode and writes no parity artifact (pre-existing
# contract — the canonical file has the same rule). The host-matrix artifact
# is written either way and is always required.
parity_artifact_gate() { # leg
  local leg="$1"
  if [ -n "$CLI_BIN" ]; then
    require_artifact "$ROOT/target/certification/jetbrains-parity${PARITY_SUFFIX}.json" "$leg"
  else
    echo "[two-ide-matrix] $leg parity artifact not required (no --cli/FAKTOR_CLI_BIN: the parity matrix runs canned-only and writes no artifact)"
    return 0
  fi
}

if [ "$RUN_PIN" -eq 1 ]; then
  PARITY_SUFFIX=""
  run_leg "pin leg ($PIN)" -- :frontend:smoke :frontend:test || exit 1
  require_artifact "$ROOT/target/certification/jetbrains-host-matrix.json" "pin leg" || exit 1
  parity_artifact_gate "pin leg" || exit 1
fi

if [ "$RUN_ALT" -eq 1 ]; then
  PARITY_SUFFIX="-$ALT"
  VM_SNAPSHOT="$(mktemp "${TMPDIR:-/tmp}/faktor-two-ide-matrix.vm.XXXXXX")"
  cp "$VM_FILE" "$VM_SNAPSHOT" || {
    echo "two-ide-matrix: cannot snapshot $VM_FILE (fail closed)" >&2; exit 2
  }
  run_leg "alternate leg ($ALT, structural visual cert)" \
    --write-verification-metadata sha256 \
    -Pfaktor.ideaVersion="$ALT" -Pfaktor.ideaVisual=structural \
    -- :frontend:smoke :frontend:test || exit 1
  restore_vm
  require_artifact "$ROOT/target/certification/jetbrains-host-matrix-$ALT.json" "alternate leg" || exit 1
  parity_artifact_gate "alternate leg" || exit 1
fi

echo "[two-ide-matrix] PASS (pin=$PIN alt=$([ "$RUN_ALT" -eq 1 ] && echo "$ALT" || echo skipped))"
exit 0

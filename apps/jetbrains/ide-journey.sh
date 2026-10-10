#!/usr/bin/env bash
# The REAL IntelliJ Platform host journey lane (automated, for the self-hosted
# linux/macOS/Windows agents).
#
# Runs the platform-integration journey (`:frontend:ideJourney`, tests:
# `dev.faktor.frontend.IdeJourneyTest`) inside the IntelliJ Platform test
# application: opens a ProjectManager project, shows the plugin.xml-registered
# Faktor tool window, hands focus to the ONE Work composer, types through the
# composer's typed-character editor action and dispatches the exact action
# bound to Ctrl+Enter (submission observed), then writes evidence to
# `target/certification/jetbrains-ide-journey/` (`journey.json`, plus
# `faktor-tool-window.png`).
#
# Availability: the lane needs the pinned IntelliJ IDEA Community 2024.1.7
# distribution either cached under `~/.gradle/caches/modules-2/files-2.1/idea/
# ideaIC/` (plus the platform test runtime) or reachable from the JetBrains
# repository. When neither holds, the lane records a typed skip artifact and
# says so explicitly -- it never claims the journey ran:
#
#   {"schema":"faktor-jetbrains-ide-journey-skip/v1","status":"skipped",...}
#
# Usage (from anywhere):
#   bash apps/jetbrains/ide-journey.sh
#
# Exit codes: 0 journey passed or honestly skipped (with skip.json); non-zero
# journey failed.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
ARTIFACT_DIR="$ROOT/target/certification/jetbrains-ide-journey"

idea_distribution_cached() {
    local artifact
    artifact="$(find "$HOME/.gradle/caches/modules-2/files-2.1/idea/ideaIC" \
        -name 'ideaIC-*.tar.gz' -print -quit 2>/dev/null)"
    [ -n "$artifact" ]
}

jetbrains_repository_reachable() {
    # A short bounded probe; never a long stall.
    curl -sI --max-time 6 \
        "https://cache-redirector.jetbrains.com/intellij-repository/releases" \
        >/dev/null 2>&1
}

if ! idea_distribution_cached && ! jetbrains_repository_reachable; then
    mkdir -p "$ARTIFACT_DIR"
    printf '{"schema":"faktor-jetbrains-ide-journey-skip/v1","status":"skipped","reason":"no cached IntelliJ IDEA Community distribution under ~/.gradle/caches and the JetBrains repository is unreachable; this host cannot execute the journey lane"}\n' \
        > "$ARTIFACT_DIR/skip.json"
    echo "[ide-journey] SKIPPED: no cached IntelliJ Platform distribution and no network reachability; recorded $ARTIFACT_DIR/skip.json (the lane did NOT run -- do not claim it)"
    exit 0
fi

echo "[ide-journey] running :frontend:ideJourney (IntelliJ Platform test application)"
(cd "$SCRIPT_DIR" && ./gradlew --no-daemon --console=plain :frontend:ideJourney)
rc=$?
if [ "$rc" -ne 0 ]; then
    echo "[ide-journey] FAIL: :frontend:ideJourney exited $rc" >&2
    exit "$rc"
fi
echo "[ide-journey] PASS: evidence in $ARTIFACT_DIR (journey.json + faktor-tool-window.png)"

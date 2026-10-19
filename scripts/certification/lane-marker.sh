#!/bin/sh
# Universal lane-marker entry point for every CI lane image.
#
# The canonical implementation is the Rust `faktor-ci-tools lane-marker`
# binary built at target/release by the CI build step (and by the Rust lanes
# themselves). The Node implementation remains as a fallback for node-only
# images where the release binary has not been built; both must produce
# byte-identical markers (scripts/certification/evidence.mjs proves it).
set -eu
DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$DIR/../.." && pwd)"
TOOL="${FAKTOR_CI_TOOLS:-}"
if [ -z "$TOOL" ]; then
    if [ -n "${CARGO_TARGET_DIR:-}" ]; then
        TOOL="$CARGO_TARGET_DIR/release/faktor-ci-tools"
    else
        TOOL="$ROOT/target/release/faktor-ci-tools"
    fi
fi

if [ -x "$TOOL" ]; then
    exec "$TOOL" lane-marker "$@"
fi
if command -v node >/dev/null 2>&1; then
    exec node "$DIR/lane-marker.mjs" "$@"
fi
if command -v cargo >/dev/null 2>&1; then
    echo "lane-marker: building faktor-ci-tools (cargo build --release -p faktor-ci-tools)" >&2
    (cd "$ROOT" && cargo build --release -p faktor-ci-tools) >&2
    if [ -x "$TOOL" ]; then
        exec "$TOOL" lane-marker "$@"
    fi
    TOOL="$ROOT/target/release/faktor-ci-tools"
    if [ -x "$TOOL" ]; then
        exec "$TOOL" lane-marker "$@"
    fi
fi
echo "lane-marker: neither $TOOL nor node is available; build faktor-ci-tools with 'cargo build --release -p faktor-ci-tools'" >&2
exit 2

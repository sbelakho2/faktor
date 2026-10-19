#!/usr/bin/env bash
# License / bans / sources policy gate (audit items 20-21).
#
# Single source of truth: root `deny.toml` (cargo-deny v2 format). This
# script evaluates the SAME policy against `cargo metadata --locked` with the
# Rust faktor-ci-tools binary, so the trusted CI static lane enforces it
# without installing cargo-deny; when a cargo-deny binary IS on PATH the
# script additionally
# runs `cargo deny --offline check licenses bans sources` and requires it
# to pass, so the two engines cannot silently drift.
#
# Checks:
#   1. every workspace member resolves to license Apache-2.0 (via
#      `license.workspace = true`) and declares no license-file;
#   2. every external package's SPDX expression is satisfied by the
#      `[licenses] allow` list (OR = one allowed branch, AND = all);
#   3. every package source is an allowed registry (no unknown registries,
#      no git/other sources given the empty `[sources] allow-git`);
#   4. no workspace dependency on an EXTERNAL crate uses a `*` requirement
#      (path edges are version-less by design and excluded);
#   5. root LICENSE/NOTICE exist (the Apache-2.0 text is pinned by the
#      static-authority scan).
#
# Exit 0 only when every check passes; every violation is printed.
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT" || exit 2

CI_TOOLS="${FAKTOR_CI_TOOLS:-}"
if [ -z "$CI_TOOLS" ]; then
    if [ -n "${CARGO_TARGET_DIR:-}" ]; then
        CI_TOOLS="$CARGO_TARGET_DIR/release/faktor-ci-tools"
    else
        CI_TOOLS="$ROOT/target/release/faktor-ci-tools"
    fi
fi
[ -x "$CI_TOOLS" ] || {
    echo "check-licenses: $CI_TOOLS is missing; build it with cargo build --release -p faktor-ci-tools" >&2
    exit 2
}

TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/faktor-licenses.XXXXXX")" || exit 2
trap 'rm -rf "$TMP_DIR"' EXIT

if ! cargo metadata --format-version 1 --locked >"$TMP_DIR/metadata.json" 2>"$TMP_DIR/metadata.err"; then
    echo "check-licenses: cargo metadata --locked failed:" >&2
    sed 's/^/  /' "$TMP_DIR/metadata.err" >&2
    exit 2
fi

"$CI_TOOLS" license-policy "$ROOT" "$TMP_DIR/metadata.json"
RC=$?
if [ "$RC" -ne 0 ]; then
    exit "$RC"
fi

if command -v cargo-deny >/dev/null 2>&1; then
    echo "check-licenses: cargo-deny present; re-checking with the reference engine"
    if ! cargo deny --offline check licenses bans sources; then
        echo "check-licenses: cargo deny check failed (run 'cargo deny check licenses bans sources' without --offline for the full report)" >&2
        exit 1
    fi
fi

echo "check-licenses: OK"

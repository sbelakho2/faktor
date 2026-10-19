#!/usr/bin/env bash
# Hermetic selftests for the CI/certification scripts (audit P1-I/P1-J/P1-K,
# P2-D, P3-scripts evidence). Runs every script's own selftest; exits
# non-zero when any of them fails. No cargo, no network, no Woodpecker:
# every case runs against local fixtures and a mock API.
#
# Usage: bash scripts/certification/tests/selftests.sh
#
# Covered:
#   scripts/certification/attestation.mjs   create/verify binding + refusal matrix
#                                           incl. fail-closed signing (no key -> typed
#                                           refusal, no artifact) and the role split:
#                                           build-attestation vs release-signing keys,
#                                           cross-role signatures rejected typed,
#                                           unprovisioned role key refused fail-closed
#   scripts/certify.sh --selftest           immutable context registry, observed-value
#                                           recording, attestation fetch/verification,
#                                           release-signature role split (release
#                                           artifact class requires the release-signing
#                                           role; absent step => source class only),
#                                           ReleaseArtifactSet matrix, canonical-gate
#                                           parity, fault-campaign release gate,
#                                           certificate classes, temp-name migration
#   scripts/cross-target-check.sh           status classifier + temp-name migration
#   scripts/check-gradle-integrity.sh       planted tampering + temp-name migration
#   scripts/check-ci-image-pins.sh          image pins + apt-snapshot/apt-pinned audit
#   scripts/build-ci-image.sh --selftest    Faktor CI image pin hygiene
#   target/release/faktor-ci-tools flake-tolerance selftest
#                                           bounded retry-once semantics for the
#                                           trusted linux test lane: flaky pass
#                                           recorded, persistent failure red,
#                                           budget/unverified-pass/no-retry
#                                           refusals (fault/release/visual)
#   target/release/faktor-ci-tools publish-status selftest
#                                           stdlib GitHub status publisher:
#                                           validation + fail-closed POST,
#                                           pinned platform signature vector,
#                                           and the HERMETIC platform-key
#                                           cases (real CLI child invocations
#                                           with explicit scrub/plant child
#                                           envs; ambient os.environ never read
#                                           or mutated)
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
cd "$ROOT" || exit 2

CI_TOOLS="${FAKTOR_CI_TOOLS:-}"
if [ -z "$CI_TOOLS" ]; then
    if [ -n "${CARGO_TARGET_DIR:-}" ]; then
        CI_TOOLS="$CARGO_TARGET_DIR/release/faktor-ci-tools"
    else
        CI_TOOLS="$ROOT/target/release/faktor-ci-tools"
    fi
fi
if [ ! -x "$CI_TOOLS" ]; then
    printf 'selftests: %s is missing; build it with cargo build --release -p faktor-ci-tools\n' "$CI_TOOLS" >&2
    exit 2
fi

failures=0
ran=0

run() { # label command...
    local label="$1"
    shift
    ran=$((ran + 1))
    printf '\n== %s ==\n' "$label"
    if "$@"; then
        printf '   ok: %s\n' "$label"
    else
        printf '   FAIL: %s (exit %s)\n' "$label" "$?" >&2
        failures=$((failures + 1))
    fi
}

run "attestation.mjs selftest" node scripts/certification/attestation.mjs selftest
run "certify.sh --selftest" bash scripts/certify.sh --selftest
run "cross-target-check.sh classify selftest" env CROSS_TARGET_SELFTEST=classify bash scripts/cross-target-check.sh
run "check-gradle-integrity.sh --selftest" bash scripts/check-gradle-integrity.sh --selftest
run "check-ci-image-pins.sh --selftest" sh scripts/check-ci-image-pins.sh --selftest
run "build-ci-image.sh --selftest" bash scripts/build-ci-image.sh --selftest
run "faktor-ci-tools flake-tolerance selftest" "$CI_TOOLS" flake-tolerance selftest
run "faktor-ci-tools publish-status selftest" "$CI_TOOLS" publish-status selftest
run "faktor-ci-tools capability-tests selftest" "$CI_TOOLS" capability-tests selftest
run "faktor-ci-tools visual-baseline selftest" "$CI_TOOLS" visual-baseline selftest
run "faktor-ci-tools iana-table --check" "$CI_TOOLS" iana-table --check
run "compile-and-smoke.sh checksum reconcile selftest" bash apps/jetbrains/compile-and-smoke.sh --selftest-checksum

printf '\n=====================\n'
if [ "$failures" -eq 0 ]; then
    printf 'SCRIPT SELFTESTS: PASS (%s suite(s))\n' "$ran"
    printf '=====================\n'
    exit 0
fi
printf 'SCRIPT SELFTESTS: FAIL (%s/%s suite(s))\n' "$failures" "$ran"
printf '=====================\n'
exit 1

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
#   scripts/certify.sh --selftest           immutable context registry, observed-value
#                                           recording, attestation fetch/verification,
#                                           certificate wording, temp-name migration
#   scripts/cross-target-check.sh           status classifier + temp-name migration
#   scripts/check-gradle-integrity.sh       planted tampering + temp-name migration
#   scripts/check-ci-image-pins.sh          image pins + apt-residual annotations
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
cd "$ROOT" || exit 2

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

printf '\n=====================\n'
if [ "$failures" -eq 0 ]; then
    printf 'SCRIPT SELFTESTS: PASS (%s suite(s))\n' "$ran"
    printf '=====================\n'
    exit 0
fi
printf 'SCRIPT SELFTESTS: FAIL (%s/%s suite(s))\n' "$failures" "$ran"
printf '=====================\n'
exit 1

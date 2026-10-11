#!/bin/sh
# Fault-injection seam profile proof.
#
# The deterministic crash seams (CrashSeam/crash_arm in cas/store/evidence,
# the orchestrator's CrashSeam vocabulary) compile ONLY under the
# `fault-injection` cargo feature. This script proves BOTH configurations
# from compiled artifacts, not source text:
#
#   production profile: seam crates built WITHOUT the feature contain no
#                       seam payload (`[fault-seam]` panic strings) at all;
#   fault profile:      the same crates built WITH the feature DO contain
#                       the payload — so the production grep has teeth.
#
# Each profile builds into its own CARGO_TARGET_DIR so a stale artifact from
# another profile can never mask (or fake) the result.
set -eu

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

seam_crates="-p faktor-cas -p faktor-store -p faktor-evidence"
prod_dir="$root/target/fault-seam-proof/production"
fault_dir="$root/target/fault-seam-proof/fault"

# Print every rlib of the seam crates whose bytes contain the literal.
scan_payload() {
    dir="$1"
    found=""
    for f in "$dir"/debug/deps/libfaktor_cas-*.rlib \
        "$dir"/debug/deps/libfaktor_store-*.rlib \
        "$dir"/debug/deps/libfaktor_evidence-*.rlib; do
        [ -e "$f" ] || continue
        if LC_ALL=C grep -aqF '[fault-seam]' "$f"; then
            found="$found $f"
        fi
    done
    printf '%s' "$found"
}

# --- production profile: no seam payload may exist -------------------------
CARGO_TARGET_DIR="$prod_dir" cargo build $seam_crates
leaks="$(scan_payload "$prod_dir")"
if [ -n "$leaks" ]; then
    echo "PRODUCTION PROFILE LEAK: seam payload compiled into:$leaks" >&2
    exit 1
fi
echo "production profile: no seam payload in cas/store/evidence artifacts"

# --- fault profile: the payload must exist (the proof has teeth) -----------
CARGO_TARGET_DIR="$fault_dir" cargo build $seam_crates --features fault-injection
present="$(scan_payload "$fault_dir")"
if [ -z "$present" ]; then
    echo "FAULT PROFILE: seam payload missing; the production proof is toothless" >&2
    exit 1
fi
echo "fault profile: seam payload present under --features fault-injection"

#!/usr/bin/env bash
# Vendor the frozen upstream Kilo v7.5.6 CLIENT SDK (`packages/sdk/js`,
# `@kilocode/sdk@7.5.6`) into compat/kilo-v756/upstream-sdk/ verbatim, then
# verify the pinned hashes.
#
#   compat/kilo-v756/vendor-sdk.sh          fetch pinned commit, re-copy, re-hash, verify
#   compat/kilo-v756/vendor-sdk.sh --check  verify the vendored tree only (offline)
#
# Fetch protocol (exactly what this script does, and what
# compat/kilo-v756/upstream.json pins):
#   git clone --filter=blob:none --no-checkout --depth 1 --branch v7.5.6 \
#       https://github.com/Kilo-Org/kilocode <tmp>/repo
#   git -C <tmp>/repo sparse-checkout set packages/sdk/js
#   git -C <tmp>/repo checkout fa02955bfa17b60e57e0d7406d200a73337472ee
#   rsync -a --delete <tmp>/repo/packages/sdk/js/ compat/kilo-v756/upstream-sdk/
#   cp <tmp>/repo/LICENSE compat/kilo-v756/LICENSES/kilocode-LICENSE.txt
#   cargo test -p faktor-tests-compat --lib upstream_manifest -- --nocapture
#
# Offline behavior: when the fetch fails (no network/DNS/registry), this
# script exits nonzero and prints the protocol above. It never fabricates
# vendored content; a blocked run leaves the vendored tree untouched.

set -euo pipefail

REPO="https://github.com/Kilo-Org/kilocode"
TAG="v7.5.6"
COMMIT="fa02955bfa17b60e57e0d7406d200a73337472ee"
UPSTREAM_SDK="packages/sdk/js"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

print_protocol() {
  cat <<'EOF'
fetch protocol:
  git clone --filter=blob:none --no-checkout --depth 1 --branch v7.5.6 \
      https://github.com/Kilo-Org/kilocode <tmp>/repo
  git -C <tmp>/repo sparse-checkout set packages/sdk/js
  git -C <tmp>/repo checkout fa02955bfa17b60e57e0d7406d200a73337472ee
  rsync -a --delete <tmp>/repo/packages/sdk/js/ compat/kilo-v756/upstream-sdk/
  cp <tmp>/repo/LICENSE compat/kilo-v756/LICENSES/kilocode-LICENSE.txt
  cargo test -p faktor-tests-compat --lib upstream_manifest
EOF
}

case "${1:-}" in
  --check)
    cargo test -p faktor-tests-compat --lib upstream_manifest
    exit $?
    ;;
  --help|-h)
    sed -n '2,24p' "$0"
    exit 0
    ;;
  "") ;;
  *)
    echo "unknown argument: $1" >&2
    exit 2
    ;;
esac

if ! command -v git >/dev/null 2>&1; then
  echo "BLOCKED: git not found on PATH" >&2
  print_protocol
  exit 2
fi

TMP="$(mktemp -d "${TMPDIR:-/tmp}/faktor-vendor-sdk.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

echo "== fetching $REPO $TAG ($COMMIT)"
if ! git clone --filter=blob:none --no-checkout --depth 1 --branch "$TAG" "$REPO" "$TMP/repo"; then
  echo "BLOCKED: network fetch failed; no vendored content was written or modified" >&2
  print_protocol
  exit 2
fi

git -C "$TMP/repo" sparse-checkout set "$UPSTREAM_SDK"
if ! git -C "$TMP/repo" checkout "$COMMIT"; then
  echo "BLOCKED: checkout of pinned commit failed; no vendored content was written" >&2
  exit 2
fi

ACTUAL="$(git -C "$TMP/repo" rev-parse HEAD)"
if [ "$ACTUAL" != "$COMMIT" ]; then
  echo "REFUSING: fetched HEAD $ACTUAL != pinned $COMMIT" >&2
  exit 2
fi

echo "== vendoring verbatim (no edits)"
mkdir -p compat/kilo-v756/upstream-sdk compat/kilo-v756/LICENSES
rsync -a --delete "$TMP/repo/$UPSTREAM_SDK/" compat/kilo-v756/upstream-sdk/
cp "$TMP/repo/LICENSE" compat/kilo-v756/LICENSES/kilocode-LICENSE.txt

echo "== hashing (regenerate blake3 manifest) + verifying"
FAKTOR_COMPAT_REGEN_SDK_MANIFEST=1 cargo test -p faktor-tests-compat --lib upstream_manifest -- --nocapture
cargo test -p faktor-tests-compat --lib upstream_manifest -- --nocapture
echo "VENDOR OK: $COMMIT"

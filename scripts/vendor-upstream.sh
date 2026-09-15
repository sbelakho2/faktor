#!/usr/bin/env bash
# Vendor the pinned upstream UI paths into ui/ and compat/ (JetBrains), then verify hashes.
#
#   scripts/vendor-upstream.sh          fetch both pins, re-copy, re-hash, verify
#   scripts/vendor-upstream.sh --check  verify the existing vendored trees only (offline)
#
# Pins:
#   1. Kilo v7.5.6 VS Code webview: packages/kilo-vscode/webview-ui -> ui/kilo-v756-webview,
#      packages/kilo-ui -> ui/kilo-ui (fetch protocol in ui/upstream.json).
#   2. Kilo JetBrains 7.1.2 (additive): packages/kilo-jetbrains ->
#      compat/jetbrains-712/kilo-jetbrains, pinned at tag jetbrains/v7.1.2 whose
#      released tree is the 7.1.2 version-bump commit. The per-file hashes live
#      in ui/upstream.json under `jetbrains_712.file_hashes`; the offline
#      Kotlin smoke (JetBrainsParitySmoke) re-verifies them with no network.
#
# Fetch protocol (exactly what this script does, and what ui/upstream.json pins):
#   git clone --filter=blob:none --no-checkout --depth 1 --branch v7.5.6 \
#       https://github.com/Kilo-Org/kilocode <tmp>/repo
#   git -C <tmp>/repo sparse-checkout set \
#       packages/kilo-vscode/webview-ui packages/kilo-ui
#   git -C <tmp>/repo checkout fa02955bfa17b60e57e0d7406d200a73337472ee
#   rsync -a --delete <tmp>/repo/packages/kilo-vscode/webview-ui/ ui/kilo-v756-webview/
#   rsync -a --delete <tmp>/repo/packages/kilo-ui/ ui/kilo-ui/
#   node scripts/verify-upstream.mjs --write && node scripts/verify-upstream.mjs --verify
#
#   git clone --filter=blob:none --no-checkout --depth 1 --branch jetbrains/v7.1.2 \
#       https://github.com/Kilo-Org/kilocode <tmp>/jb-repo
#   git -C <tmp>/jb-repo fetch --depth 1 origin 436ff09e649bd0866c84bd9f98933a74cad2d25c
#   git -C <tmp>/jb-repo sparse-checkout set packages/kilo-jetbrains
#   git -C <tmp>/jb-repo checkout 436ff09e649bd0866c84bd9f98933a74cad2d25c
#   rsync -a --delete <tmp>/jb-repo/packages/kilo-jetbrains/ compat/jetbrains-712/kilo-jetbrains/
#   cp <tmp>/jb-repo/LICENSE compat/jetbrains-712/LICENSES/kilocode-LICENSE.txt
#   node <hash-jetbrains.mjs> write && node <hash-jetbrains.mjs> verify
#
# Offline behavior: when a fetch fails (no network/DNS/registry), this script
# exits nonzero and prints the protocol above. It never fabricates vendored
# content; a blocked run leaves the vendored trees untouched.

set -euo pipefail

REPO="https://github.com/Kilo-Org/kilocode"
TAG="v7.5.6"
COMMIT="fa02955bfa17b60e57e0d7406d200a73337472ee"
UPSTREAM_WEBVIEW="packages/kilo-vscode/webview-ui"
UPSTREAM_KILO_UI="packages/kilo-ui"

JETBRAINS_TAG="jetbrains/v7.1.2"
JETBRAINS_COMMIT="436ff09e649bd0866c84bd9f98933a74cad2d25c"
JETBRAINS_UPSTREAM="packages/kilo-jetbrains"
JETBRAINS_VENDOR="compat/jetbrains-712"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

print_protocol() {
  cat <<EOF
fetch protocol:
  git clone --filter=blob:none --no-checkout --depth 1 --branch $TAG \\
      $REPO <tmp>/repo
  git -C <tmp>/repo sparse-checkout set $UPSTREAM_WEBVIEW $UPSTREAM_KILO_UI
  git -C <tmp>/repo checkout $COMMIT
  rsync -a --delete <tmp>/repo/$UPSTREAM_WEBVIEW/ ui/kilo-v756-webview/
  rsync -a --delete <tmp>/repo/$UPSTREAM_KILO_UI/ ui/kilo-ui/
  node scripts/verify-upstream.mjs --write
  node scripts/verify-upstream.mjs --verify

  git clone --filter=blob:none --no-checkout --depth 1 --branch $JETBRAINS_TAG \\
      $REPO <tmp>/jb-repo
  git -C <tmp>/jb-repo fetch --depth 1 origin $JETBRAINS_COMMIT
  git -C <tmp>/jb-repo sparse-checkout set $JETBRAINS_UPSTREAM
  git -C <tmp>/jb-repo checkout $JETBRAINS_COMMIT
  rsync -a --delete <tmp>/jb-repo/$JETBRAINS_UPSTREAM/ $JETBRAINS_VENDOR/kilo-jetbrains/
  cp <tmp>/jb-repo/LICENSE $JETBRAINS_VENDOR/LICENSES/kilocode-LICENSE.txt
  node <hash-jetbrains.mjs> write
  node <hash-jetbrains.mjs> verify
EOF
}

# --------------------------------------------------------- jetbrains hasher
# The additive pin's per-file SHA-256 manifest lives in ui/upstream.json
# (`jetbrains_712.file_hashes`); this node program recomputes it (write) or
# re-verifies every file byte-for-byte with no network (verify).
write_jetbrains_hasher() {
  cat > "$TMP/hash-jetbrains.mjs" <<'NODE'
import { createHash } from 'node:crypto';
import { existsSync, readFileSync, readdirSync, writeFileSync } from 'node:fs';
import { join, resolve, sep } from 'node:path';

const mode = process.argv[2];
const ROOT = process.cwd();
const VROOT = join(ROOT, 'compat/jetbrains-712');
const HASHED = join(VROOT, 'kilo-jetbrains');
const MANIFEST = join(ROOT, 'ui/upstream.json');
const LICENSE = 'LICENSES/kilocode-LICENSE.txt';

function walk(root, prefix) {
  const out = [];
  for (const entry of readdirSync(root, { withFileTypes: true })) {
    const rel = prefix ? prefix + '/' + entry.name : entry.name;
    if (entry.isDirectory()) out.push(...walk(join(root, entry.name), rel));
    else if (entry.isFile()) out.push(rel);
    else throw new Error('symlink not allowed in the pinned tree: ' + rel);
  }
  return out;
}

const sha256 = (abs) => createHash('sha256').update(readFileSync(abs)).digest('hex');
const files = walk(HASHED, 'kilo-jetbrains').sort();
const hashes = {};
let bytes = 0;
for (const rel of files) {
  const abs = join(VROOT, rel.split('/').join(sep));
  bytes += readFileSync(abs).length;
  hashes[rel] = sha256(abs);
}
const licenseFiles = existsSync(join(VROOT, LICENSE))
  ? { [LICENSE]: sha256(join(VROOT, LICENSE)) }
  : {};

const manifest = JSON.parse(readFileSync(MANIFEST, 'utf8'));
const previous = manifest.jetbrains_712;

// The webview manifest rewrite (`verify-upstream.mjs --write`) preserves only
// its own PIN_FIELDS and drops foreign keys; `preserve`/`restore` carry the
// additive jetbrains_712 entry across that rewrite.
if (mode === 'preserve') {
  writeFileSync(process.argv[3], JSON.stringify(previous ?? {}, null, 2) + '\n');
  process.exit(0);
}
if (mode === 'restore') {
  const saved = JSON.parse(readFileSync(process.argv[3], 'utf8'));
  if (saved && saved.commit === '436ff09e649bd0866c84bd9f98933a74cad2d25c') {
    manifest.jetbrains_712 = saved;
    writeFileSync(MANIFEST, JSON.stringify(manifest, null, 2) + '\n');
  }
  process.exit(0);
}
if (!previous || previous.commit !== '436ff09e649bd0866c84bd9f98933a74cad2d25c') {
  console.error('FATAL: ui/upstream.json jetbrains_712 pin metadata is missing or names another commit');
  process.exit(1);
}

if (mode === 'write') {
  previous.fileCount = files.length;
  previous.totalBytes = bytes;
  previous.file_hashes = hashes;
  previous.licenseFiles = licenseFiles;
  // The hash keys are relative to `vendoredRoot`; keep the two fields
  // impossible to drift apart.
  previous.hashedRoot = previous.vendoredRoot;
  writeFileSync(MANIFEST, JSON.stringify(manifest, null, 2) + '\n');
  console.log(`wrote jetbrains_712: ${files.length} files, ${bytes} bytes, sha256`);
  process.exit(0);
}
if (mode !== 'verify') {
  console.error('usage: hash-jetbrains.mjs write|verify|preserve <file>|restore <file>');
  process.exit(2);
}

const errors = [];
const expected = new Set(Object.keys(previous.file_hashes || {}));
for (const [rel, want] of Object.entries(previous.file_hashes || {})) {
  const abs = join(VROOT, rel.split('/').join(sep));
  if (!existsSync(abs)) {
    errors.push('missing: ' + rel);
    continue;
  }
  const got = sha256(abs);
  if (got !== want) errors.push(`hash mismatch: ${rel} (expected ${want}, got ${got})`);
}
for (const rel of files) {
  if (!expected.has(rel)) errors.push('unexpected file: ' + rel);
}
for (const [rel, want] of Object.entries(previous.licenseFiles || {})) {
  const abs = join(VROOT, rel.split('/').join(sep));
  if (!existsSync(abs)) errors.push('missing license: ' + rel);
  else if (sha256(abs) !== want) errors.push('license hash mismatch: ' + rel);
}
if (errors.length > 0) {
  for (const error of errors.slice(0, 40)) console.error('DIVERGENCE ' + error);
  console.error(`${errors.length} divergence(s); the JetBrains pin is not the pinned commit`);
  process.exit(1);
}
console.log(`jetbrains_712 OK: ${previous.repository} ${previous.tag} ${previous.commit}`);
console.log(`${files.length} files, ${bytes} bytes, sha256 verified`);
NODE
}

case "${1:-}" in
  --check)
    node scripts/verify-upstream.mjs --verify
    TMP="$(mktemp -d "${TMPDIR:-/tmp}/faktor-vendor-upstream.XXXXXX")"
    trap 'rm -rf "$TMP"' EXIT
    write_jetbrains_hasher
    node "$TMP/hash-jetbrains.mjs" verify
    exit $?
    ;;
  --help|-h)
    sed -n '2,45p' "$0"
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

TMP="$(mktemp -d "${TMPDIR:-/tmp}/faktor-vendor-upstream.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

write_jetbrains_hasher
JB_META="$TMP/jetbrains-meta.json"

echo "== fetching $REPO $TAG ($COMMIT)"
if ! git clone --filter=blob:none --no-checkout --depth 1 --branch "$TAG" "$REPO" "$TMP/repo"; then
  echo "BLOCKED: network fetch failed; no vendored content was written or modified" >&2
  print_protocol
  exit 2
fi

git -C "$TMP/repo" sparse-checkout set "$UPSTREAM_WEBVIEW" "$UPSTREAM_KILO_UI"
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
mkdir -p ui/kilo-v756-webview ui/kilo-ui ui/LICENSES
rsync -a --delete "$TMP/repo/$UPSTREAM_WEBVIEW/" ui/kilo-v756-webview/
rsync -a --delete "$TMP/repo/$UPSTREAM_KILO_UI/" ui/kilo-ui/
cp "$TMP/repo/LICENSE" ui/LICENSES/kilocode-LICENSE.txt
# The third-party notice directory is outside the sparse checkout; read the
# exact pinned blobs instead of widening the fetch.
rm -rf ui/LICENSES/kilo-vscode-THIRD_PARTY_LICENSES
while IFS= read -r file; do
  rel="${file#"packages/kilo-vscode/THIRD_PARTY_LICENSES/"}"
  mkdir -p "ui/LICENSES/kilo-vscode-THIRD_PARTY_LICENSES/$(dirname "$rel")"
  git -C "$TMP/repo" show "$COMMIT:$file" > "ui/LICENSES/kilo-vscode-THIRD_PARTY_LICENSES/$rel"
done < <(git -C "$TMP/repo" ls-tree -r --name-only "$COMMIT" packages/kilo-vscode/THIRD_PARTY_LICENSES)

echo "== hashing"
# Preserve the additive jetbrains_712 metadata across the webview rewrite.
node "$TMP/hash-jetbrains.mjs" preserve "$JB_META"
node scripts/verify-upstream.mjs --write
node "$TMP/hash-jetbrains.mjs" restore "$JB_META"
echo "== verifying"
node scripts/verify-upstream.mjs --verify

# --------------------------------------------- additive: JetBrains 7.1.2 pin
# The webview manifest rewrite above preserves only the webview PIN_FIELDS, so
# the jetbrains_712 entry is re-merged after it (this step is additive: it
# never touches the webview file_hashes).
echo "== fetching $REPO $JETBRAINS_TAG ($JETBRAINS_COMMIT)"
if ! git clone --filter=blob:none --no-checkout --depth 1 --branch "$JETBRAINS_TAG" \
  "$REPO" "$TMP/jb-repo"; then
  echo "BLOCKED: network fetch failed; no JetBrains content was written or modified" >&2
  print_protocol
  exit 2
fi
# The tag points at the pre-bump commit; the released 7.1.2 tree is the
# version-bump commit, fetched explicitly.
git -C "$TMP/jb-repo" fetch --depth 1 origin "$JETBRAINS_COMMIT"
git -C "$TMP/jb-repo" sparse-checkout set "$JETBRAINS_UPSTREAM"
if ! git -C "$TMP/jb-repo" checkout "$JETBRAINS_COMMIT"; then
  echo "BLOCKED: checkout of $JETBRAINS_COMMIT failed; no JetBrains content was written" >&2
  exit 2
fi
JB_ACTUAL="$(git -C "$TMP/jb-repo" rev-parse HEAD)"
if [ "$JB_ACTUAL" != "$JETBRAINS_COMMIT" ]; then
  echo "REFUSING: fetched HEAD $JB_ACTUAL != pinned $JETBRAINS_COMMIT" >&2
  exit 2
fi
echo "== vendoring JetBrains verbatim (no edits)"
mkdir -p "$JETBRAINS_VENDOR/kilo-jetbrains" "$JETBRAINS_VENDOR/LICENSES"
rsync -a --delete "$TMP/jb-repo/$JETBRAINS_UPSTREAM/" "$JETBRAINS_VENDOR/kilo-jetbrains/"
cp "$TMP/jb-repo/LICENSE" "$JETBRAINS_VENDOR/LICENSES/kilocode-LICENSE.txt"

echo "== hashing JetBrains pin"
write_jetbrains_hasher
node "$TMP/hash-jetbrains.mjs" write
echo "== verifying JetBrains pin"
node "$TMP/hash-jetbrains.mjs" verify
echo "VENDOR OK: $COMMIT + $JETBRAINS_COMMIT"

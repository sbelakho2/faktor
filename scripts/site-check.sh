#!/usr/bin/env bash
# Website gates. Run from anywhere; workspace-wide gates remain the repo rule.
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"
cargo fmt --check -p faktor-site
cargo check -p faktor-site
zola --root site build
cargo test -p faktor-site
cargo clippy -p faktor-site --all-targets -- -D warnings
scripts/site-zero-js.sh
echo "site-check: crate gates and zero-JS scan passed"

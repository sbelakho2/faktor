#!/usr/bin/env bash
# Zero-JavaScript gate: source templates, static assets and the built site.
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"
fail=0
scan() {
  local path="$1"
  [ -e "$path" ] || return 0
  if grep -RInE --include='*.html' --include='*.css' --include='*.svg' --include='*.json' --include='*.txt' \
      '<script|javascript:|[[:space:]]on[a-zA-Z]+[[:space:]]*=' "$path" >/tmp/faktor-zero-js-scan; then
    echo "zero-js: forbidden runtime script/handler in $path" >&2
    cat /tmp/faktor-zero-js-scan >&2
    fail=1
  fi
  if find "$path" -type f -name '*.js' -print -quit | grep -q .; then
    echo "zero-js: JavaScript file found under $path" >&2
    find "$path" -type f -name '*.js' -print >&2
    fail=1
  fi
}
scan site/templates
scan site/static
scan site/public
# Every form in source must be an explicit POST; no JS-only controls.
if grep -RIn '<form' site/templates site/static | grep -v 'method="post"' >/tmp/faktor-zero-js-forms; then
  echo "zero-js: non-POST form found" >&2
  cat /tmp/faktor-zero-js-forms >&2
  fail=1
fi
if [ "$fail" -ne 0 ]; then
  exit 1
fi
echo "zero-js: OK (no script tags, no inline handlers, no JavaScript files, forms are POST)"

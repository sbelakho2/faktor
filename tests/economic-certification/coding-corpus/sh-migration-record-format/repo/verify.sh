#!/bin/sh
set -eu
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cat > "$work/records.txt" <<'EOF'
name=ada
token=a=b
note=already:v2
plain=value
EOF
sh migrate.sh "$work/records.txt"
cat > "$work/expected.txt" <<'EOF'
name:ada
token:a=b
note=already:v2
plain:value
EOF
cmp -s "$work/records.txt" "$work/expected.txt" || {
  echo "migration output mismatch" >&2
  exit 1
}
before=$(cat "$work/records.txt")
sh migrate.sh "$work/records.txt"
[ "$(cat "$work/records.txt")" = "$before" ] || {
  echo "migration is not idempotent" >&2
  exit 1
}

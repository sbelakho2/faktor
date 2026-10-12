#!/bin/sh
# Usage: migrate.sh <file>
# Rewrite v1 `key=value` records (one per line) to the v2 `key:value` form.
# Only the first `=` separates key from value; v2 lines (containing `:`) are
# left byte-identical.
set -eu
file="$1"
tmp="$file.migrate.tmp"
while IFS= read -r line || [ -n "$line" ]; do
  case "$line" in
    *:* ) printf '%s\n' "$line" ;;
    *=* ) key=${line%%=*}; value=${line#*=}; printf '%s:%s\n' "$key" "$value" ;;
    * ) printf '%s\n' "$line" ;;
  esac
done < "$file" > "$tmp"
mv "$tmp" "$file"

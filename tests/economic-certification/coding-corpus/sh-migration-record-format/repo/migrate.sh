#!/bin/sh
# Usage: migrate.sh <file>
# Rewrite v1 `key=value` records (one per line) to the v2 `key:value` form.
file="$1"
sed 's/=/:/g' "$file" > "$file.tmp"
mv "$file.tmp" "$file"

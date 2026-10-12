#!/bin/sh
set -eu
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
printf 'apple 3 0.50\npear 2 1.25\n' > "$work/table.txt"
out=$(awk -f sum.awk "$work/table.txt")
if [ "$out" != "4.00" ]; then
  echo "total [$out], expected [4.00]" >&2
  exit 1
fi

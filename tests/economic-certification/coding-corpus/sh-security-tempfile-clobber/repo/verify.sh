#!/bin/sh
set -eu
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
printf 'ORIGINAL\n' > "$work/victim.txt"
printf 'payload\n' > "$work/src.txt"
ln -s "$work/victim.txt" "$work/src.txt.backup.tmp"
out=$(sh backup.sh "$work/src.txt")
if [ "$(cat "$work/victim.txt")" != "ORIGINAL" ]; then
  echo "victim file was clobbered through a pre-planted path" >&2
  exit 1
fi
expected=$(cksum < "$work/src.txt")
if [ "$out" != "$expected" ]; then
  echo "checksum output mismatch: [$out] != [$expected]" >&2
  exit 1
fi
if [ -e "$work/src.txt.backup.tmp" ] && [ ! -L "$work/src.txt.backup.tmp" ]; then
  echo "predictable temporary path was created as a real file" >&2
  exit 1
fi

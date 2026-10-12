#!/bin/sh
set -eu
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
printf 'public\n' > data/ok.txt
printf 'TOPSECRET\n' > secret.txt
out=$(python3 reader.py ok.txt)
if [ "$out" != "public" ]; then
  echo "normal read failed: [$out]" >&2
  exit 1
fi
if python3 reader.py ../secret.txt > "$work/traversal.out" 2>/dev/null; then
  echo "path traversal was accepted" >&2
  exit 1
fi
if grep -q TOPSECRET "$work/traversal.out"; then
  echo "secret leaked on stdout" >&2
  exit 1
fi
if python3 reader.py /etc/hostname > "$work/absolute.out" 2>/dev/null; then
  echo "absolute path was accepted" >&2
  exit 1
fi
rm -f data/ok.txt secret.txt

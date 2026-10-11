#!/bin/sh
# Fix: compare each word to the PREVIOUS word only.
prev=""
first=1
for w in "$@"; do
  if [ "$first" -eq 1 ] || [ "$w" != "$prev" ]; then
    printf '%s\n' "$w"
  fi
  prev="$w"
  first=0
done

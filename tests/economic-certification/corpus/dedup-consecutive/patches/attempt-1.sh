#!/bin/sh
# Attempt 1 fix (incomplete): compares every word to the FIRST word.
first=$1
for w in "$@"; do
  if [ "$w" != "$first" ]; then
    printf '%s\n' "$w"
  fi
done

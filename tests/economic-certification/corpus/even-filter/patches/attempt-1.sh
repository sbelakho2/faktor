#!/bin/sh
# Fix: keep values whose remainder is zero.
for n in "$@"; do
  if [ $((n % 2)) -eq 0 ]; then
    printf '%s\n' "$n"
  fi
done

#!/bin/sh
# Print every integer from 1 through the given N, one per line.
n="$1"
i=1
while [ "$i" -lt "$n" ]; do
  printf '%s\n' "$i"
  i=$((i + 1))
done

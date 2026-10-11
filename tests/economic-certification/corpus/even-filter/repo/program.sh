#!/bin/sh
# Print the even arguments, one per line.
for n in "$@"; do
  if [ $((n % 2)) -eq 1 ]; then
    printf '%s\n' "$n"
  fi
done

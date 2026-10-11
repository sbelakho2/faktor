#!/bin/sh
# Fix: track the running maximum instead of the last argument.
m=""
for n in "$@"; do
  if [ -z "$m" ] || [ "$n" -gt "$m" ]; then
    m="$n"
  fi
done
echo "$m"

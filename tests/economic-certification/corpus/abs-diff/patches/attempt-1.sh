#!/bin/sh
# Attempt 1 (fails): negates only when A is greater, so the sign still
# depends on the argument order.
d=$(( $1 - $2 ))
if [ "$1" -gt "$2" ]; then
  d=$((0 - d))
fi
echo "$d"

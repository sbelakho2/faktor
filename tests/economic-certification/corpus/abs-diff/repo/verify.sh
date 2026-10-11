#!/bin/sh
# Objective verification: non-negative difference either way.
set -eu
check() {
  out=$(sh program.sh "$1" "$2")
  [ "$out" = "$3" ] || { echo "abs($1,$2): expected $3 got $out"; exit 1; }
}
check 5 8 3
check 8 5 3
check 4 4 0
echo ok

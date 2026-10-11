#!/bin/sh
# Objective verification: exact row-major flat index.
set -eu
check() {
  out=$(sh program.sh "$1" "$2" "$3")
  [ "$out" = "$4" ] || { echo "index($1,$2,$3): expected $4 got $out"; exit 1; }
}
check 2 3 4 11
check 0 5 10 5
check 3 0 5 15
check 1 1 1 2
echo ok

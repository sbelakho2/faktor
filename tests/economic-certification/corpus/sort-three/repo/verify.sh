#!/bin/sh
# Objective verification: all three positions ordered.
set -eu
check() {
  out=$(sh program.sh "$1" "$2" "$3")
  [ "$out" = "$4" ] || { echo "sort($1,$2,$3): expected '$4' got '$out'"; exit 1; }
}
check 3 1 2 "1 2 3"
check 2 3 1 "1 2 3"
check 3 2 1 "1 2 3"
check 1 2 3 "1 2 3"
echo ok

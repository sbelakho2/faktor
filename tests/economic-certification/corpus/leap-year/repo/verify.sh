#!/bin/sh
# Objective verification: full Gregorian leap-year rule.
set -eu
check() {
  out=$(sh program.sh "$1")
  [ "$out" = "$2" ] || { echo "year $1: expected $2 got $out"; exit 1; }
}
check 2024 1
check 2023 0
check 1900 0
check 2000 1
check 2400 1
check 2100 0
echo ok

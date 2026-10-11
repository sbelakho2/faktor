#!/bin/sh
# Objective verification: sum of arguments must be exact.
set -eu
out=$(sh program.sh 3 5 7)
[ "$out" = "15" ] || { echo "sum(3,5,7): expected 15 got $out"; exit 1; }
out=$(sh program.sh 10)
[ "$out" = "10" ] || { echo "sum(10): expected 10 got $out"; exit 1; }
out=$(sh program.sh)
[ "$out" = "0" ] || { echo "sum(): expected 0 got $out"; exit 1; }
echo ok

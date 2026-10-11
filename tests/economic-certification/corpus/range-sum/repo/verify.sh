#!/bin/sh
# Objective verification: inclusive range sum, negative ranges included.
set -eu
out=$(sh program.sh 1 3)
[ "$out" = "6" ] || { echo "range(1,3): expected 6 got $out"; exit 1; }
out=$(sh program.sh 5 5)
[ "$out" = "5" ] || { echo "range(5,5): expected 5 got $out"; exit 1; }
out=$(sh program.sh -2 2)
[ "$out" = "0" ] || { echo "range(-2,2): expected 0 got $out"; exit 1; }
echo ok

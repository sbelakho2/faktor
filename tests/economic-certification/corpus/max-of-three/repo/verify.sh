#!/bin/sh
# Objective verification: the largest value wins regardless of position.
set -eu
out=$(sh program.sh 1 9 4)
[ "$out" = "9" ] || { echo "max(1,9,4): expected 9 got $out"; exit 1; }
out=$(sh program.sh 7 2)
[ "$out" = "7" ] || { echo "max(7,2): expected 7 got $out"; exit 1; }
out=$(sh program.sh -1 -5)
[ "$out" = "-1" ] || { echo "max(-1,-5): expected -1 got $out"; exit 1; }
echo ok

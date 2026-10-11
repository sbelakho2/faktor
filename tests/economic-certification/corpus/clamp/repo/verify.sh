#!/bin/sh
# Objective verification: both bounds clamp, in-range values pass through.
set -eu
out=$(sh program.sh 5 1 3)
[ "$out" = "3" ] || { echo "clamp(5,1,3): expected 3 got $out"; exit 1; }
out=$(sh program.sh -2 0 10)
[ "$out" = "0" ] || { echo "clamp(-2,0,10): expected 0 got $out"; exit 1; }
out=$(sh program.sh 7 0 10)
[ "$out" = "7" ] || { echo "clamp(7,0,10): expected 7 got $out"; exit 1; }
out=$(sh program.sh 11 0 10)
[ "$out" = "10" ] || { echo "clamp(11,0,10): expected 10 got $out"; exit 1; }
echo ok

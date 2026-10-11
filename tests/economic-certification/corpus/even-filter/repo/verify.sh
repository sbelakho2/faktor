#!/bin/sh
# Objective verification: only even values are emitted.
set -eu
out=$(sh program.sh 1 2 3 4 | tr '\n' ',')
[ "$out" = "2,4," ] || { echo "even(1,2,3,4): expected 2,4 got '$out'"; exit 1; }
out=$(sh program.sh 7 9)
[ -z "$out" ] || { echo "even(7,9): expected no output got '$out'"; exit 1; }
out=$(sh program.sh 0 5 8 | tr '\n' ',')
[ "$out" = "0,8," ] || { echo "even(0,5,8): expected 0,8 got '$out'"; exit 1; }
echo ok

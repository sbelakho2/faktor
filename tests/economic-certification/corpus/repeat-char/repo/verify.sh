#!/bin/sh
# Objective verification: exactly N repetitions, including N = 0.
set -eu
out=$(sh program.sh a 3)
[ "$out" = "aaa" ] || { echo "repeat(a,3): expected aaa got '$out'"; exit 1; }
out=$(sh program.sh x 0)
[ "$out" = "" ] || { echo "repeat(x,0): expected empty got '$out'"; exit 1; }
out=$(sh program.sh - 2)
[ "$out" = "--" ] || { echo "repeat(-,2): expected -- got '$out'"; exit 1; }
echo ok

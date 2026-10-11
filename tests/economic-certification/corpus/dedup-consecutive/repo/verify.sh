#!/bin/sh
# Objective verification: only ADJACENT duplicates collapse; a later repeat
# of an earlier value survives.
set -eu
out=$(sh program.sh a a b b b c | tr '\n' ',')
[ "$out" = "a,b,c," ] || { echo "dedup(a,a,b,b,b,c): expected a,b,c got '$out'"; exit 1; }
out=$(sh program.sh a b a | tr '\n' ',')
[ "$out" = "a,b,a," ] || { echo "dedup(a,b,a): expected a,b,a got '$out'"; exit 1; }
out=$(sh program.sh x)
[ "$out" = "x" ] || { echo "dedup(x): expected x got '$out'"; exit 1; }
echo ok

#!/bin/sh
# Fix: the loop runs while i is strictly below N.
c=$1
n=$2
i=0
out=""
while [ "$i" -lt "$n" ]; do
  out="$out$c"
  i=$((i + 1))
done
echo "$out"

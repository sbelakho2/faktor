#!/bin/sh
# Attempt 1 (fails): fixes the first two positions and the last two, but
# never revisits the first pair afterwards.
a=$1
b=$2
c=$3
if [ "$a" -gt "$b" ]; then
  t=$a
  a=$b
  b=$t
fi
if [ "$b" -gt "$c" ]; then
  t=$b
  b=$c
  c=$t
fi
echo "$a $b $c"

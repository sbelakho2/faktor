#!/bin/sh
# Attempt 2 (fails): adds a final pass but compares the wrong pair (a vs c).
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
if [ "$a" -gt "$c" ]; then
  t=$a
  a=$c
  c=$t
fi
echo "$a $b $c"

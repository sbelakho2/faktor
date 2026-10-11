#!/bin/sh
# Print three numeric arguments in ascending order.
a=$1
b=$2
c=$3
if [ "$b" -gt "$c" ]; then
  t=$b
  b=$c
  c=$t
fi
echo "$a $b $c"

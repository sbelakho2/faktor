#!/bin/sh
# usage: program.sh CHAR N  -> print CHAR exactly N times.
c=$1
n=$2
i=0
out=""
while [ "$i" -le "$n" ]; do
  out="$out$c"
  i=$((i + 1))
done
echo "$out"

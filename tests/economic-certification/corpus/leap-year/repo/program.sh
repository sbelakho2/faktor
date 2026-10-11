#!/bin/sh
# usage: program.sh YEAR -> print 1 when YEAR is a leap year else 0.
y=$1
leap=0
if [ $((y % 4)) -eq 0 ]; then
  leap=1
fi
echo "$leap"

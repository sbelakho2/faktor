#!/bin/sh
# Attempt 1 fix (incomplete): adds the century rule but forgets that years
# divisible by 400 are still leap years.
y=$1
leap=0
if [ $((y % 4)) -eq 0 ]; then
  leap=1
fi
if [ $((y % 100)) -eq 0 ]; then
  leap=0
fi
echo "$leap"

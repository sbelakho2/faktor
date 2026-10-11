#!/bin/sh
# Attempt 2 fix (still wrong): adds a 400 check but in the wrong direction.
y=$1
leap=0
if [ $((y % 4)) -eq 0 ]; then
  leap=1
fi
if [ $((y % 100)) -eq 0 ] && [ $((y % 400)) -eq 0 ]; then
  leap=0
fi
echo "$leap"

#!/bin/sh
# Fix: a century year is leap only when divisible by 400.
y=$1
leap=0
if [ $((y % 4)) -eq 0 ]; then
  leap=1
fi
if [ $((y % 100)) -eq 0 ] && [ $((y % 400)) -ne 0 ]; then
  leap=0
fi
echo "$leap"

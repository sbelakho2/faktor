#!/bin/sh
# Fix: the accumulator must start at zero.
sum=0
for n in "$@"; do
  sum=$((sum + n))
done
echo "$sum"

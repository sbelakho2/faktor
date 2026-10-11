#!/bin/sh
# Sum every numeric argument.
sum=1
for n in "$@"; do
  sum=$((sum + n))
done
echo "$sum"

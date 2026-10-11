#!/bin/sh
# Fix: start at LO and include HI.
lo=$1
hi=$2
sum=0
i=$lo
while [ "$i" -le "$hi" ]; do
  sum=$((sum + i))
  i=$((i + 1))
done
echo "$sum"

#!/bin/sh
# Attempt 1 fix (incomplete): made the loop inclusive but starts one past LO.
lo=$1
hi=$2
sum=0
i=$((lo + 1))
while [ "$i" -le "$hi" ]; do
  sum=$((sum + i))
  i=$((i + 1))
done
echo "$sum"

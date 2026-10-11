#!/bin/sh
# usage: program.sh LO HI -> sum of every integer in [LO, HI].
lo=$1
hi=$2
sum=0
i=$lo
while [ "$i" -lt "$hi" ]; do
  sum=$((sum + i))
  i=$((i + 1))
done
echo "$sum"

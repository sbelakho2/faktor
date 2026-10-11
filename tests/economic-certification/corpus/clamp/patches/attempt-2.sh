#!/bin/sh
# Fix: clamp into [LO, HI] on both sides.
v=$1
lo=$2
hi=$3
if [ "$v" -lt "$lo" ]; then
  v=$lo
fi
if [ "$v" -gt "$hi" ]; then
  v=$hi
fi
echo "$v"

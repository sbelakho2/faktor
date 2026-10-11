#!/bin/sh
# usage: program.sh VALUE LO HI -> clamp VALUE into [LO, HI].
v=$1
lo=$2
hi=$3
if [ "$v" -lt "$lo" ]; then
  v=$hi
fi
if [ "$v" -gt "$hi" ]; then
  v=$hi
fi
echo "$v"

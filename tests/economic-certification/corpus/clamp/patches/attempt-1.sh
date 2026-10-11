#!/bin/sh
# Attempt 1 fix (incomplete): the lower clamp returns LO, but the upper
# clamp was rewritten to fold back to LO.
v=$1
lo=$2
hi=$3
if [ "$v" -lt "$lo" ]; then
  v=$lo
fi
if [ "$v" -gt "$hi" ]; then
  v=$lo
fi
echo "$v"

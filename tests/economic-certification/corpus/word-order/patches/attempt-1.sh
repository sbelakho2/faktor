#!/bin/sh
# Fix: prepend each new word so the order is reversed.
out=""
for w in "$@"; do
  out="$w $out"
done
echo "${out% }"

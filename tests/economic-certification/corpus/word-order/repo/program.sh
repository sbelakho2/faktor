#!/bin/sh
# Print the words in reverse order, single space separated.
out=""
for w in "$@"; do
  out="$out$w "
done
echo "${out% }"

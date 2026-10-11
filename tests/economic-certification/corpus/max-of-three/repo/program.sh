#!/bin/sh
# Print the largest numeric argument.
m=""
for n in "$@"; do
  m="$n"
done
echo "$m"

#!/bin/sh
# Attempt 3 fix (wrong): subtracts COL instead of adding it.
echo $(( $1 * $3 - $2 ))

#!/bin/sh
# Attempt 2 fix (wrong): multiplies by COL instead of STRIDE.
echo $(( $1 * $2 + $3 ))

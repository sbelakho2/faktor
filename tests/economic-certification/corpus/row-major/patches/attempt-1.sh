#!/bin/sh
# Attempt 1 fix (wrong): correct shape but adds a spurious off-by-one.
echo $(( $1 * $3 + $2 + 1 ))

#!/bin/sh
# Fix: flat index = ROW * STRIDE + COL.
echo $(( $1 * $3 + $2 ))

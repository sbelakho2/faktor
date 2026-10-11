#!/bin/sh
# usage: program.sh ROW COL STRIDE -> row-major flat index ROW*STRIDE + COL.
echo $(( $1 + $2 * $3 ))

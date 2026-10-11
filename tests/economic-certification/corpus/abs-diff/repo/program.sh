#!/bin/sh
# usage: program.sh A B -> absolute difference of A and B.
d=$(( $1 - $2 ))
echo "$d"

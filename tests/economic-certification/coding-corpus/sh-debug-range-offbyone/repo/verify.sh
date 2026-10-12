#!/bin/sh
set -eu
fail=0
check() {
  expected="$1"
  shift
  actual=$(sh run.sh "$@" || true)
  if [ "$actual" != "$expected" ]; then
    echo "FAIL: run.sh $* -> [$actual], expected [$expected]" >&2
    fail=1
  fi
}
check '1
2
3
4' 4
check '1' 1
check '' 0
exit "$fail"

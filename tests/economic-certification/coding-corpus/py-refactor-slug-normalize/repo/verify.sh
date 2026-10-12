#!/bin/sh
set -eu
a=$(python3 app_a.py "  Hello   World  ")
b=$(python3 app_b.py "  Hello   World  ")
if [ "$a" != "hello-world" ]; then
  echo "app_a normalized to [$a], expected [hello-world]" >&2
  exit 1
fi
if [ "$b" != "hello-world" ]; then
  echo "app_b normalized to [$b], expected [hello-world]" >&2
  exit 1
fi
if [ ! -f common.py ]; then
  echo "shared common.py is missing" >&2
  exit 1
fi

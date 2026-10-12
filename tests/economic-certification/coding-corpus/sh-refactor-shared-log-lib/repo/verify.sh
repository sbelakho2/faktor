#!/bin/sh
set -eu
a=$(sh app1.sh hello)
b=$(sh app2.sh hello)
if [ "$a" != "[info] hello" ]; then
  echo "app1 log format: [$a]" >&2
  exit 1
fi
if [ "$b" != "[info] hello" ]; then
  echo "app2 log format: [$b]" >&2
  exit 1
fi
if [ ! -f lib/log.sh ]; then
  echo "shared lib/log.sh is missing" >&2
  exit 1
fi

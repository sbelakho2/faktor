#!/bin/sh
set -eu
python3 - <<'PY'
from parse import split_fields

assert split_fields("a,b") == ["a", "b"], "plain fields"
assert split_fields('a,"b,c",d') == ["a", "b,c", "d"], "quoted comma"
assert split_fields('"a,b",c') == ["a,b", "c"], "leading quoted field"
assert split_fields('he said "hi",ok') == ['he said "hi"', "ok"], "embedded quotes"
print("ok")
PY

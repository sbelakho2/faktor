#!/bin/sh
# Objective verification: word ORDER is reversed, letters are not.
set -eu
out=$(sh program.sh alpha beta gamma)
[ "$out" = "gamma beta alpha" ] || { echo "reverse words: expected 'gamma beta alpha' got '$out'"; exit 1; }
out=$(sh program.sh solo)
[ "$out" = "solo" ] || { echo "single word: expected 'solo' got '$out'"; exit 1; }
out=$(sh program.sh one two)
[ "$out" = "two one" ] || { echo "two words: expected 'two one' got '$out'"; exit 1; }
echo ok

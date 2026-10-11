#!/bin/sh
# Print words with duplicate VALUES removed entirely (not just adjacent
# repeats), one per line.
seen=""
for w in "$@"; do
  case " $seen " in
    *" $w "*) ;;
    *) printf '%s\n' "$w"; seen="$seen $w" ;;
  esac
done

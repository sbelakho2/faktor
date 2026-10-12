# Print the grand total of the `item count unit_price` table.
{
  total += $2 * $3
}
END {
  printf "%.2f\n", total
}

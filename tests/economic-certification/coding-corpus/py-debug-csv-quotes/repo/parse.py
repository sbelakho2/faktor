"""Delimited-field parsing helpers."""


def split_fields(line):
    """Split one delimited record into its fields."""
    return line.split(",")

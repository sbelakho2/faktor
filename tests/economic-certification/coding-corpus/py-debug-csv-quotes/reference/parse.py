"""Delimited-field parsing helpers."""
import csv
import io


def split_fields(line):
    """Split one delimited record into its fields, honoring double quotes."""
    return next(csv.reader(io.StringIO(line)))

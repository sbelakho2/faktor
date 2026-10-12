"""Shared label normalization."""


def normalize(text):
    return "-".join(text.strip().lower().split())

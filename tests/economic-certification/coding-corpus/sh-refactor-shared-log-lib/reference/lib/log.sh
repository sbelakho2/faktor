#!/bin/sh
# Shared log formatting used by every application script.
log_info() {
  printf '[info] %s\n' "$1"
}

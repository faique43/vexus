#!/usr/bin/env bash
set -euo pipefail

# Prints one formatted line.
log_line() {
  printf '%s\n' "$1"
}

function deploy() {
  log_line "deploying"
  rsync -a src/ dest/
}

deploy

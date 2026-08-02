#!/usr/bin/env bash
# Nightly database backup for the shop.
set -euo pipefail

# Timestamped backup filename.
backup_name() {
  date +"shop-%Y%m%d.sql.gz"
}

run_backup() {
  local name
  name="$(backup_name)"
  pg_dump shop | gzip > "/backups/$name"
}

run_backup

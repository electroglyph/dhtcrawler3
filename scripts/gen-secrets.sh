#!/usr/bin/env bash
# Creates random database passwords for deploy/docker-compose.yml.
# Existing files are left untouched, so re-running is safe.
#
# Permissions: the directory is 0700 (only the host user can list or reach the
# files). The files themselves are 0644 because Compose bind-mounts them into
# containers that run as other users (65532 for dhtcrawler4, 999 for
# PostgreSQL), which must be able to read them. Keep the directory private.
set -euo pipefail

dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/../deploy" && pwd)/secrets"
mkdir -p "$dir"
chmod 700 "$dir"

for name in pg_superuser_password dc4_owner_password dc4_crawler_password dc4_indexer_password dc4_web_password; do
  file="$dir/$name"
  if [[ -s "$file" ]]; then
    chmod 644 "$file"
    echo "keep    $file"
    continue
  fi
  # Read a fixed amount of randomness first, so no pipe is closed early
  # (with pipefail, `tr </dev/urandom | head` fails with SIGPIPE).
  password=""
  while [[ ${#password} -lt 40 ]]; do
    chunk="$(head -c 512 /dev/urandom | LC_ALL=C tr -dc 'A-Za-z0-9')"
    password+="$chunk"
  done
  (umask 077 && printf '%s' "${password:0:40}" >"$file")
  chmod 644 "$file"
  echo "created $file"
done

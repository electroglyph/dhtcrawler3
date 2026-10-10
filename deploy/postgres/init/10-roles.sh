#!/usr/bin/env bash
# Creates the dhtcrawler4 roles and database (docs/03-design.md §10, R12).
#
# Runs once, from /docker-entrypoint-initdb.d of the official postgres image,
# when the data directory is first initialised. Required environment:
#
#   DC4_OWNER_PASSWORD_FILE    owner: runs migrations and admin commands
#   DC4_CRAWLER_PASSWORD_FILE  crawl role
#   DC4_INDEXER_PASSWORD_FILE  index role
#   DC4_WEB_PASSWORD_FILE      web role
#
# Each names a file (e.g. a Docker secret) holding one password. Passwords
# never appear on a command line or in shell-expanded SQL: psql receives only
# the file paths (-v) and reads each file itself with a backquoted `cat`, then
# quotes the value as a literal with :'name'.
#
# Table privileges are granted by the migrations (migrations/…_grants.sql in
# crates/dc4-store), which run as dc4_owner.
set -euo pipefail

fail() {
  echo "10-roles.sh: $*" >&2
  exit 1
}

check_secret() {
  local var="$1"
  local path="${!var:-}"
  [[ -n "$path" ]] || fail "$var is not set"
  [[ -f "$path" && -r "$path" ]] || fail "$var=$path is not a readable file"
  [[ -s "$path" ]] || fail "$var=$path is empty"
  # One line only: an embedded newline would silently change the password.
  local lines
  lines="$(grep -c '' -- "$path")"
  [[ "$lines" -le 1 ]] || fail "$var=$path must hold a single line"
  return 0
}

for var in DC4_OWNER_PASSWORD_FILE DC4_CRAWLER_PASSWORD_FILE DC4_INDEXER_PASSWORD_FILE DC4_WEB_PASSWORD_FILE; do
  check_secret "$var"
done

psql --no-psqlrc -v ON_ERROR_STOP=1 \
  --username "${POSTGRES_USER:-postgres}" \
  --dbname "${POSTGRES_DB:-postgres}" \
  -v owner_pw_file="$DC4_OWNER_PASSWORD_FILE" \
  -v crawler_pw_file="$DC4_CRAWLER_PASSWORD_FILE" \
  -v indexer_pw_file="$DC4_INDEXER_PASSWORD_FILE" \
  -v web_pw_file="$DC4_WEB_PASSWORD_FILE" \
  <<'SQL'
\set owner_pw `cat -- :'owner_pw_file'`
\set crawler_pw `cat -- :'crawler_pw_file'`
\set indexer_pw `cat -- :'indexer_pw_file'`
\set web_pw `cat -- :'web_pw_file'`

SET password_encryption = 'scram-sha-256';

CREATE ROLE dc4_owner   LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS PASSWORD :'owner_pw';
CREATE ROLE dc4_crawler LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS PASSWORD :'crawler_pw';
CREATE ROLE dc4_indexer LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS PASSWORD :'indexer_pw';
CREATE ROLE dc4_web     LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS PASSWORD :'web_pw';

\unset owner_pw
\unset crawler_pw
\unset indexer_pw
\unset web_pw

-- Service roles only need a handful of connections each.
ALTER ROLE dc4_crawler CONNECTION LIMIT 200;
ALTER ROLE dc4_indexer CONNECTION LIMIT 10;
ALTER ROLE dc4_web     CONNECTION LIMIT 100;

-- A transaction left open must not hold the change-feed lock for long
-- (docs/03-design.md §10). dc4-store sets the same on every connection; this
-- also covers any other client that uses a service role's credentials.
ALTER ROLE dc4_crawler SET idle_in_transaction_session_timeout = '60s';
ALTER ROLE dc4_indexer SET idle_in_transaction_session_timeout = '60s';
ALTER ROLE dc4_web     SET idle_in_transaction_session_timeout = '60s';

-- The crawl queue is self-healing: every claim holds a 120 s lease and an
-- uncommitted batch is reclaimed on expiry, so losing ~200 ms of recent
-- crawler commits (one WAL-writer window) on a crash only re-fetches those
-- keys. Asynchronous commits cut fsyncs/IOPS (not WAL bytes) on the
-- write-hot queue. Crawler only: the indexer and web keep full durability,
-- and a cluster-wide setting stays rejected.
ALTER ROLE dc4_crawler SET synchronous_commit = off;

CREATE DATABASE dc4 OWNER dc4_owner ENCODING 'UTF8' TEMPLATE template0;
REVOKE ALL ON DATABASE dc4 FROM PUBLIC;
GRANT CONNECT ON DATABASE dc4 TO dc4_crawler, dc4_indexer, dc4_web;

\connect dc4
-- Only the owner creates objects (already the default since PostgreSQL 15).
REVOKE CREATE ON SCHEMA public FROM PUBLIC;
ALTER SCHEMA public OWNER TO dc4_owner;
SQL

echo "10-roles.sh: created roles dc4_owner, dc4_crawler, dc4_indexer, dc4_web and database dc4"

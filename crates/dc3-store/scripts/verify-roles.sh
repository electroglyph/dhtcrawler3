#!/usr/bin/env bash
# Verifies deploy/postgres/init/10-roles.sh and the grant migration end to end:
# starts a throwaway postgres:18.6 container with the init directory mounted,
# then runs tests/roles.rs in the pinned Rust image against it, connecting as
# each role, and finally checks the privilege catalog directly (docs/03-design.md
# §10). The container and the temporary secrets are always removed.
#
#   crates/dc3-store/scripts/verify-roles.sh
#
# DC3_TARGET   cargo target directory name under ./target (default: dc3-store)
# DC3_NETWORK  docker network shared by both containers (default: dc3-dev)
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
network="${DC3_NETWORK:-dc3-dev}"
target="${DC3_TARGET:-dc3-store}"
name="dc3-roles-check-$$-$RANDOM"
secrets="$(mktemp -d)"

cleanup() {
  docker rm -f "$name" >/dev/null 2>&1 || true
  rm -rf "$secrets"
}
trap cleanup EXIT

docker network inspect "$network" >/dev/null 2>&1 || docker network create "$network" >/dev/null

# Random passwords, one per role. Include characters that need quoting in SQL
# and in the shell to prove neither layer interpolates them.
for role in owner crawler indexer web; do
  pw="$(od -An -N12 -tx1 /dev/urandom | tr -d ' \n')"
  printf "%s'q\"\$x;" "$pw" >"$secrets/$role"
done
# A trailing newline (as `echo pw > file` writes) is not part of the password.
echo >>"$secrets/web"

chmod 644 "$secrets"/*

docker run -d --rm --name "$name" --network "$network" \
  -e POSTGRES_PASSWORD=bootstrap-only \
  -e DC3_OWNER_PASSWORD_FILE=/run/dc3/owner \
  -e DC3_CRAWLER_PASSWORD_FILE=/run/dc3/crawler \
  -e DC3_INDEXER_PASSWORD_FILE=/run/dc3/indexer \
  -e DC3_WEB_PASSWORD_FILE=/run/dc3/web \
  -v "$root/deploy/postgres/init":/docker-entrypoint-initdb.d:ro \
  -v "$secrets":/run/dc3:ro \
  postgres:18.6 >/dev/null

# The entrypoint restarts the server after init; wait for the final one.
for _ in $(seq 1 60); do
  if docker logs "$name" 2>&1 | grep -q "PostgreSQL init process complete"; then
    if docker exec "$name" pg_isready -q -U postgres -d dc3 2>/dev/null; then
      break
    fi
  fi
  if ! docker inspect "$name" >/dev/null 2>&1; then
    echo "postgres container exited during init" >&2
    exit 1
  fi
  sleep 1
done
docker logs "$name" 2>&1 | grep "10-roles.sh" || { docker logs "$name"; exit 1; }

# Build URLs with percent-encoded passwords.
urlenc() { tr -d '\n' <"$1" | od -An -tx1 -v | tr -d ' \n' | sed 's/\(..\)/%\1/g'; }
url() { printf 'postgres://dc3_%s:%s@%s:5432/dc3' "$1" "$(urlenc "$secrets/$1")" "$name"; }

docker run --rm --network "$network" \
  -e DC3_ROLES_OWNER_URL="$(url owner)" \
  -e DC3_ROLES_CRAWLER_URL="$(url crawler)" \
  -e DC3_ROLES_INDEXER_URL="$(url indexer)" \
  -e DC3_ROLES_WEB_URL="$(url web)" \
  -v "$root":/w \
  -v dc3-cargo-registry:/usr/local/cargo/registry \
  -v dc3-cargo-git:/usr/local/cargo/git \
  -e CARGO_TARGET_DIR="/w/target/$target" \
  -e CARGO_TERM_COLOR=never \
  -w /w \
  rust:1.98-slim-trixie \
  cargo test -p dc3-store --locked --test roles -- --nocapture

# The privilege matrix, read from the catalog as the superuser. Each row is
# (what, actual, expected); any mismatch fails the script.
docker exec -i "$name" psql --no-psqlrc -v ON_ERROR_STOP=1 -q -U postgres -d dc3 <<'SQL'
DO $$
DECLARE
    r   record;
    bad text := '';
BEGIN
    FOR r IN
        SELECT * FROM (VALUES
            -- web: SELECT on two tables, nothing else.
            ('web may select torrents',
             has_table_privilege('dc3_web', 'public.torrents', 'SELECT'), true),
            ('web may select stats_daily',
             has_table_privilege('dc3_web', 'public.stats_daily', 'SELECT'), true),
            ('web may update torrents',
             has_any_column_privilege('dc3_web', 'public.torrents', 'UPDATE'), false),
            ('web may insert into torrents',
             has_any_column_privilege('dc3_web', 'public.torrents', 'INSERT'), false),
            ('web may read pending',
             has_any_column_privilege('dc3_web', 'public.pending', 'SELECT'), false),
            ('web may write pending',
             has_any_column_privilege('dc3_web', 'public.pending', 'UPDATE')
             OR has_table_privilege('dc3_web', 'public.pending', 'INSERT')
             OR has_table_privilege('dc3_web', 'public.pending', 'DELETE'), false),
            ('web may read audit_log',
             has_any_column_privilege('dc3_web', 'public.audit_log', 'SELECT'), false),
            ('web may write audit_log',
             has_table_privilege('dc3_web', 'public.audit_log', 'INSERT'), false),
            ('web may read settings',
             has_any_column_privilege('dc3_web', 'public.settings', 'SELECT'), false),
            ('web may write stats_daily',
             has_table_privilege('dc3_web', 'public.stats_daily', 'INSERT')
             OR has_any_column_privilege('dc3_web', 'public.stats_daily', 'UPDATE'), false),
            ('web may use change_seq',
             has_sequence_privilege('dc3_web', 'public.change_seq', 'USAGE')
             OR has_sequence_privilege('dc3_web', 'public.change_seq', 'UPDATE'), false),
            ('web may create in public',
             has_schema_privilege('dc3_web', 'public', 'CREATE'), false),
            ('web may create temp tables',
             has_database_privilege('dc3_web', 'dc3', 'TEMPORARY'), false),
            -- crawler and indexer.
            ('crawler may use change_seq',
             has_sequence_privilege('dc3_crawler', 'public.change_seq', 'USAGE'), true),
            ('crawler may delete pending',
             has_table_privilege('dc3_crawler', 'public.pending', 'DELETE'), true),
            ('crawler has table-wide UPDATE on torrents',
             has_table_privilege('dc3_crawler', 'public.torrents', 'UPDATE'), false),
            ('crawler may update torrents.deleted_at',
             has_column_privilege('dc3_crawler', 'public.torrents', 'deleted_at', 'UPDATE'), true),
            ('crawler may delete torrents',
             has_table_privilege('dc3_crawler', 'public.torrents', 'DELETE'), false),
            ('crawler may read settings',
             has_any_column_privilege('dc3_crawler', 'public.settings', 'SELECT'), false),
            ('indexer may read pending',
             has_any_column_privilege('dc3_indexer', 'public.pending', 'SELECT'), false),
            ('indexer may read stats_daily',
             has_any_column_privilege('dc3_indexer', 'public.stats_daily', 'SELECT'), false),
            ('indexer may write torrents',
             has_any_column_privilege('dc3_indexer', 'public.torrents', 'UPDATE')
             OR has_table_privilege('dc3_indexer', 'public.torrents', 'INSERT'), false),
            ('indexer may read change_seq',
             has_sequence_privilege('dc3_indexer', 'public.change_seq', 'SELECT'), true),
            ('indexer may use change_seq',
             has_sequence_privilege('dc3_indexer', 'public.change_seq', 'USAGE'), false)
        ) AS v(what, actual, expected)
    LOOP
        IF r.actual IS DISTINCT FROM r.expected THEN
            bad := bad || format(E'\n  %s: got %s, want %s', r.what, r.actual, r.expected);
        END IF;
    END LOOP;
    IF bad <> '' THEN
        RAISE EXCEPTION 'privilege check failed:%', bad;
    END IF;
END
$$;
SQL
echo "verify-roles: privilege matrix OK"

# A missing secret must abort init loudly.
bad="dc3-roles-check-bad-$$-$RANDOM"
if docker run --rm --name "$bad" \
  -e POSTGRES_PASSWORD=x \
  -e DC3_OWNER_PASSWORD_FILE=/nonexistent \
  -v "$root/deploy/postgres/init":/docker-entrypoint-initdb.d:ro \
  postgres:18.6 >"$secrets/bad.log" 2>&1; then
  echo "init with a missing secret should have failed" >&2
  exit 1
fi
grep -q "DC3_OWNER_PASSWORD_FILE=/nonexistent is not a readable file" "$secrets/bad.log" || {
  cat "$secrets/bad.log" >&2
  exit 1
}
echo "verify-roles: OK"

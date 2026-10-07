-- Re-applies the service-role grants, converging roles created after the
-- earlier migrations ran (audit A2-34). The 03/04 grants are conditional on
-- role existence at migration time and never re-applied, so a database
-- migrated before its roles existed (any non-docker/manual flow: the
-- `10-roles.sh` + `migrate` ordering is only enforced by docker-compose)
-- left late-created `dc3_crawler`/`dc3_indexer`/`dc3_web` with 42501 errors.
-- Re-running every surviving grant here is idempotent.
--
-- This also fixes a live permission bug the conditionals were masking: the
-- crawler never had DELETE on `torrents`, so every `purge_tombstoned` sweep
-- (which hard-deletes old tombstones) failed with 42501 and tombstones were
-- never purged. The crawler runs that sweep; it gets the grant below.
--
-- Dead objects are NOT re-granted: `denylist` + `dc3_key_denied` (dropped by
-- 07) and `submit_report` (dropped by 06) stay gone.

DO $$
BEGIN
    IF EXISTS (SELECT FROM pg_roles WHERE rolname = 'dc3_crawler') THEN
        GRANT USAGE ON SCHEMA public TO dc3_crawler;
        GRANT SELECT ON torrents TO dc3_crawler;
        GRANT INSERT (dht_key, info_hash_v1, info_hash_v2, name, total_size, file_count, files,
                      files_truncated, piece_length, seen_count, first_seen_at, change_seq)
            ON torrents TO dc3_crawler;
        GRANT UPDATE (info_hash_v1, info_hash_v2, name, total_size, file_count, files,
                      files_truncated, piece_length, seen_count, last_seen_at, deleted_at,
                      change_seq)
            ON torrents TO dc3_crawler;
        GRANT UPDATE (last_scraped_at, seeders_est, scrape_failures)
            ON torrents TO dc3_crawler;
        -- Purge duty (see header): hard-deletes old tombstones.
        GRANT DELETE ON torrents TO dc3_crawler;
        GRANT SELECT, INSERT, UPDATE, DELETE ON pending TO dc3_crawler;
        GRANT SELECT, INSERT, UPDATE ON stats_daily TO dc3_crawler;
        GRANT SELECT, INSERT, UPDATE, DELETE ON removed_keys TO dc3_crawler;
        GRANT SELECT, INSERT, DELETE ON purged_torrents TO dc3_crawler;
        GRANT INSERT ON audit_log TO dc3_crawler;
        GRANT USAGE ON SEQUENCE change_seq TO dc3_crawler;
    END IF;

    IF EXISTS (SELECT FROM pg_roles WHERE rolname = 'dc3_indexer') THEN
        GRANT USAGE ON SCHEMA public TO dc3_indexer;
        GRANT SELECT ON torrents TO dc3_indexer;
        GRANT SELECT ON purged_torrents TO dc3_indexer;
        GRANT SELECT ON SEQUENCE change_seq TO dc3_indexer;
    END IF;

    IF EXISTS (SELECT FROM pg_roles WHERE rolname = 'dc3_web') THEN
        GRANT USAGE ON SCHEMA public TO dc3_web;
        GRANT SELECT ON torrents, stats_daily TO dc3_web;
    END IF;
END
$$;

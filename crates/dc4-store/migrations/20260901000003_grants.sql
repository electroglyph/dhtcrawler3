-- Least-privilege grants (docs/03-design.md §10, R12). The roles are created by
-- deploy/postgres/init/10-roles.sh; each block is skipped when its role does
-- not exist (e.g. in test databases). Migrations run as the owner role, which
-- owns every object and keeps all rights on them.
--
-- | Role        | Grants                                                        |
-- |-------------|---------------------------------------------------------------|
-- | dc4_crawler | SELECT on torrents; INSERT/UPDATE on the torrents columns it  |
-- |             | writes (never hidden_at or reviewed_at); SELECT/INSERT/UPDATE |
-- |             | on pending, stats_daily; DELETE on pending; SELECT/INSERT on  |
-- |             | denylist; INSERT on audit_log; USAGE on change_seq            |
-- | dc4_indexer | SELECT on torrents and denylist; SELECT on change_seq (the    |
-- |             | high-water mark reads last_value; nextval needs USAGE)        |
-- | dc4_web     | SELECT on torrents, denylist, stats_daily; EXECUTE on         |
-- |             | submit_report. No INSERT, UPDATE or DELETE anywhere, and no   |
-- |             | access to pending, reports, audit_log or settings.            |
--
-- Every role that bumps change_seq directly needs USAGE on it: only the
-- crawler. The web role bumps it only inside submit_report, which runs as the
-- owner. All three roles call dc4_key_denied (the visibility test). Nobody
-- else may call a function of ours: the schema migration revoked EXECUTE from
-- PUBLIC.

DO $$
BEGIN
    IF EXISTS (SELECT FROM pg_roles WHERE rolname = 'dc4_crawler') THEN
        GRANT USAGE ON SCHEMA public TO dc4_crawler;
        -- Column lists: the moderation columns (hidden_at, reviewed_at) belong
        -- to submit_report and the admin, so a compromised crawler can neither
        -- un-hide a reported torrent nor make one immune to auto-hide.
        GRANT SELECT ON torrents TO dc4_crawler;
        GRANT INSERT (dht_key, info_hash_v1, info_hash_v2, name, total_size, file_count, files,
                      files_truncated, piece_length, seen_count, first_seen_at, change_seq)
            ON torrents TO dc4_crawler;
        GRANT UPDATE (info_hash_v1, info_hash_v2, name, total_size, file_count, files,
                      files_truncated, piece_length, seen_count, last_seen_at, deleted_at,
                      change_seq)
            ON torrents TO dc4_crawler;
        GRANT SELECT, INSERT, UPDATE, DELETE ON pending TO dc4_crawler;
        GRANT SELECT, INSERT, UPDATE ON stats_daily TO dc4_crawler;
        -- Automatic denials (csam-auto, private) and their audit trail.
        GRANT SELECT, INSERT ON denylist TO dc4_crawler;
        GRANT INSERT ON audit_log TO dc4_crawler;
        GRANT USAGE ON SEQUENCE change_seq TO dc4_crawler;
        GRANT EXECUTE ON FUNCTION dc4_key_denied(bytea, bytea, bytea) TO dc4_crawler;
    END IF;

    IF EXISTS (SELECT FROM pg_roles WHERE rolname = 'dc4_indexer') THEN
        GRANT USAGE ON SCHEMA public TO dc4_indexer;
        GRANT SELECT ON torrents, denylist TO dc4_indexer;
        GRANT SELECT ON SEQUENCE change_seq TO dc4_indexer;
        GRANT EXECUTE ON FUNCTION dc4_key_denied(bytea, bytea, bytea) TO dc4_indexer;
    END IF;

    IF EXISTS (SELECT FROM pg_roles WHERE rolname = 'dc4_web') THEN
        GRANT USAGE ON SCHEMA public TO dc4_web;
        GRANT SELECT ON torrents, denylist, stats_daily TO dc4_web;
        GRANT EXECUTE ON FUNCTION dc4_key_denied(bytea, bytea, bytea) TO dc4_web;
        GRANT EXECUTE ON FUNCTION submit_report(bytea, text, text, text) TO dc4_web;
    END IF;
END
$$;

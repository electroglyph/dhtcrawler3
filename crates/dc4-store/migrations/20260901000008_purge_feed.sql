-- Feed-visible tombstone purges (audit A2-11). `purge_tombstoned` hard-deletes
-- rows, which previously vanished without a change-feed entry: an indexer
-- lagging past the purge grace never learned the delete and kept serving the
-- document. Every purge now records one row per deleted torrent with its own
-- `change_seq` stamp, and the change feed unions those rows back in as
-- `visible = false` entries (deleted by id, like tombstones).
--
-- Retention: `purge_tombstoned` prunes entries older than 90 days. An indexer
-- resuming from a checkpoint older than that must re-sync from scratch (its
-- view is stale for every other feed row too, not just purges).

CREATE TABLE IF NOT EXISTS purged_torrents (
    torrent_id bigint PRIMARY KEY,
    purged_seq bigint NOT NULL,
    purged_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS purged_torrents_seq ON purged_torrents (purged_seq);

-- Grants for the new table, same conditional pattern as 03/04 (skipped when
-- the roles do not exist yet; 09_reapply_grants converges late-created
-- roles). The crawler writes and prunes; the indexer reads via the feed.
DO $$
BEGIN
    IF EXISTS (SELECT FROM pg_roles WHERE rolname = 'dc4_crawler') THEN
        GRANT SELECT, INSERT, DELETE ON purged_torrents TO dc4_crawler;
    END IF;

    IF EXISTS (SELECT FROM pg_roles WHERE rolname = 'dc4_indexer') THEN
        GRANT SELECT ON purged_torrents TO dc4_indexer;
    END IF;
END
$$;

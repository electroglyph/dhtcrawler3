-- BEP 33 scrape state (bep33.md §3). Scrape bookkeeping lives on the
-- torrent row so the indexer sees no churn (record_scrape never bumps
-- change_seq); removal memory lives in removed_keys (§4a).
--
-- * torrents.last_scraped_at: NULL until the first scrape; also the claim
--   lease (claim_scrape_due stamps it, so a crashed worker stalls those rows
--   until the next interval -- accepted for v1, see bep33.md §3 LB-7).
-- * torrents.seeders_est: NULL until an aware scrape estimates the swarm;
--   NULL also means "unaware, back off" for scheduling.
-- * torrents.scrape_failures: consecutive dead (aware, est <= threshold)
--   scrapes; reset on any live scrape, untouched by unaware scrapes.
-- * removed_keys: one row per scrape-tombstoned DHT key (20 bytes only),
--   consulted by admission while the cooldown runs. removals escalates the
--   cooldown 7d -> 30d -> 90d (cap); sightings counts post-removal sightings
--   for the strong-evidence rule; both reset when the key is fetched again.

ALTER TABLE torrents ADD COLUMN last_scraped_at timestamptz NULL;
ALTER TABLE torrents ADD COLUMN seeders_est integer NULL CHECK (seeders_est >= 0);
ALTER TABLE torrents ADD COLUMN scrape_failures integer NOT NULL DEFAULT 0
  CHECK (scrape_failures >= 0);

-- Claim order (last_scraped_at ASC NULLS FIRST) over visible rows only, so
-- the planner never index-scans hidden/deleted rows to find due ones.
CREATE INDEX torrents_scrape_due ON torrents (last_scraped_at ASC NULLS FIRST)
  WHERE deleted_at IS NULL AND hidden_at IS NULL;

CREATE TABLE removed_keys (
  key        bytea PRIMARY KEY CHECK (octet_length(key) = 20),
  removed_at timestamptz NOT NULL DEFAULT now(),
  removals   integer NOT NULL DEFAULT 1 CHECK (removals >= 1),
  sightings  integer NOT NULL DEFAULT 0 CHECK (sightings >= 0)
);

CREATE INDEX removed_keys_age ON removed_keys (removed_at);

-- The crawler's UPDATE allow-list on torrents is column-specific (see
-- 03_grants.sql): extend it with the scrape columns, and give the crawler
-- full access to removed_keys. SELECT on torrents is already full-table.
DO $$
BEGIN
    IF EXISTS (SELECT FROM pg_roles WHERE rolname = 'dc3_crawler') THEN
        GRANT UPDATE (last_scraped_at, seeders_est, scrape_failures)
            ON torrents TO dc3_crawler;
        GRANT SELECT, INSERT, UPDATE, DELETE ON removed_keys TO dc3_crawler;
    END IF;
END
$$;

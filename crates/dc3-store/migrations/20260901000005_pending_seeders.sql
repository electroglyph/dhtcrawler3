-- BEP 33 liveness-ordered fetch queue (bep33.md §8). Fetch prefers
-- known-live keys: `CLAIM_SQL` orders by `seeders_est DESC NULLS LAST`
-- (unscraped keys sink; the scrape queue instead prefers unchecked rows
-- with NULLS FIRST, intentionally asymmetric). Estimates arrive for free
-- via the piggybacked fetch scrape lookups (`note_fetch_estimate`) and via
-- backfill from `torrents` on re-queue (`OBSERVE_QUEUE_SQL`); unaware
-- lookups leave NULL, never 0 (0 means measured dead).
--
-- No new grants: the crawler already holds full SELECT/INSERT/UPDATE/DELETE
-- on `pending` (03_grants.sql).

ALTER TABLE pending ADD COLUMN seeders_est integer NULL CHECK (seeders_est >= 0);

-- Serves `ORDER BY seeders_est DESC NULLS LAST, next_attempt_at` over due
-- rows only, so reordering the claim over millions of rows never seq-scans.
CREATE INDEX pending_seeders ON pending (seeders_est DESC NULLS LAST, next_attempt_at)
  WHERE NOT gave_up;

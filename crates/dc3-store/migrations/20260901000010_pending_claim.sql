-- Claim-order index for the attempts-first fetch queue.
--
-- The dead-key sieve reordered `CLAIM_SQL` to
-- `ORDER BY attempts ASC, seeders_est DESC NULLS LAST, next_attempt_at`,
-- but no index led with `attempts`: `pending_ready` leads with
-- `next_attempt_at` and `pending_seeders` with `seeders_est`. Every claim
-- (512 fetch workers sharing one pool) sorted millions of `pending` rows to
-- return 8, held connections past the 10 s acquire timeout, and stalled the
-- crawl with "pool timed out while waiting for an open connection".
--
-- `pending_claim` matches the claim's `ORDER BY` exactly under its `NOT
-- gave_up` predicate, so the `LIMIT 8 ... FOR UPDATE SKIP LOCKED` subquery
-- walks the index in order instead of sorting. The time/lease filters
-- (`next_attempt_at <= now()`, expired `lease_until`) stay heap-checked:
-- `now()` is not immutable, so no partial predicate can encode them.
--
-- `pending_seeders` is dropped in the same transaction: nothing else orders
-- `pending` by `seeders_est` (the scrape queue lives on `torrents`), and an
-- unused index on this write-hot table only buys back the WAL churn it was
-- meant to fix.
--
-- No new grants: indexes carry the owner's rights; the crawler's table
-- grants (03_grants.sql, re-applied by 09) are unchanged.

DROP INDEX IF EXISTS pending_seeders;

CREATE INDEX pending_claim ON pending
  (attempts ASC, seeders_est DESC NULLS LAST, next_attempt_at ASC)
  WHERE NOT gave_up;

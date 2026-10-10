-- Drop the unused `pending_gave_up` index (db churn plan follow-up).
--
-- `pending_gave_up` on `(last_attempt_at) WHERE gave_up` has zero scans
-- across four live samples (two before the gave-up purge shipped, two
-- after, 92 MB), yet every `pending` update pays one index write for it.
-- The only query filtering on that predicate is the purge DELETE
-- (`Store::purge_gave_up`), which removes the bulk of the table per sweep
-- at equilibrium and is correctly seq-scanned by the planner, so the
-- index serves nothing — not even the purge.
--
-- Same playbook as `pending_ready`, dropped in `..._11_drop_pending_ready.sql`.
-- Plain `DROP INDEX`, not `CONCURRENTLY`: migrations run inside a
-- transaction and `CONCURRENTLY` cannot run in one.
--
-- Rollback (no data change involved):
-- `CREATE INDEX pending_gave_up ON pending (last_attempt_at) WHERE gave_up;`
--
-- No new grants: indexes carry the owner's rights.

DROP INDEX IF EXISTS pending_gave_up;

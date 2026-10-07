-- Remove the denylist subsystem. Fresh databases never get these objects;
-- existing databases lose them here. Table and function grants vanish with
-- the objects, including the crawler/indexer/web EXECUTE grants from
-- 03_grants.sql. The audit_log deny/undeny history is a separate table and
-- stays. The removal-memory tables (removed_keys) and the stats_daily table
-- itself stay; only the now-writerless stats_daily.blocked column goes.
DROP FUNCTION IF EXISTS public.dc3_key_denied(bytea, bytea, bytea);

DROP TABLE IF EXISTS public.denylist;

ALTER TABLE public.stats_daily DROP COLUMN IF EXISTS blocked;

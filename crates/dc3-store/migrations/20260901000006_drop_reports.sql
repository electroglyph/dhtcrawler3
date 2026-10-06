-- Remove the visitor-report subsystem (report form, submit_report, CSAM
-- auto-hide). Fresh databases never get these objects; existing databases
-- lose them here. The grants migration's submit_report EXECUTE vanishes with
-- the function. Column-level grants on the dropped columns vanish with them.
DROP FUNCTION IF EXISTS public.submit_report(bytea, text, text, text);

DROP TABLE IF EXISTS public.reports;

-- The scrape-due index predicate references hidden_at; rebuild it without
-- the hidden clause before dropping the columns.
DROP INDEX IF EXISTS torrents_scrape_due;
CREATE INDEX torrents_scrape_due ON torrents (last_scraped_at ASC NULLS FIRST)
  WHERE deleted_at IS NULL;

ALTER TABLE torrents DROP COLUMN IF EXISTS hidden_at;
ALTER TABLE torrents DROP COLUMN IF EXISTS reviewed_at;

DELETE FROM settings WHERE key IN ('autohide_per_hour', 'open_reports_cap');

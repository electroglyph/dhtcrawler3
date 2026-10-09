-- dhtcrawler4 schema (docs/03-design.md §10). This file is the specification.
--
-- Keys are raw bytes: a DHT key and a v1 infohash are 20 bytes, a v2 infohash is
-- 32 bytes. A 32-byte v2 infohash is found in the DHT under its first 20 bytes,
-- so every "is this key denied?" test compares 20-byte prefixes.
--
-- Every 20-byte prefix below is written `substring(x FROM 1 FOR 20)`, the form
-- the expression indexes use, so the planner can match them.

-- Change feed. Every write the index must see sets change_seq = nextval(...)
-- while holding pg_advisory_xact_lock_shared(CHANGE_LOCK_KEY). CACHE 1 keeps
-- last_value exact, which the indexer's high-water mark relies on.
CREATE SEQUENCE change_seq AS bigint MINVALUE 1 START WITH 1 CACHE 1 NO CYCLE;

CREATE TABLE torrents (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    dht_key         bytea NOT NULL CHECK (octet_length(dht_key) = 20),
    info_hash_v1    bytea CHECK (octet_length(info_hash_v1) = 20),
    info_hash_v2    bytea CHECK (octet_length(info_hash_v2) = 32),
    -- Sanitised display name; '' only after a tombstone wiped it.
    name            text NOT NULL CHECK (char_length(name) <= 1024),
    total_size      bigint NOT NULL CHECK (total_size >= 0),
    file_count      bigint NOT NULL CHECK (file_count >= 0),
    -- [{"p": path, "s": size}], at most 2 000 entries (the rest are counted in
    -- file_count and flagged by files_truncated). The shape is checked by the
    -- torrents_files_shape trigger: PostgreSQL evaluates every CHECK on every
    -- UPDATE, so a CHECK here would detoast the list on each popularity bump.
    files           jsonb NOT NULL DEFAULT '[]'::jsonb,
    files_truncated boolean NOT NULL DEFAULT false,
    piece_length    bigint CHECK (piece_length > 0),
    seen_count      bigint NOT NULL DEFAULT 1 CHECK (seen_count >= 0),
    first_seen_at   timestamptz NOT NULL DEFAULT now(),
    last_seen_at    timestamptz NOT NULL DEFAULT now(),
    change_seq      bigint NOT NULL,
    -- Hidden pending review (set by submit_report for a CSAM report).
    hidden_at       timestamptz,
    -- An admin dismissed a report about this torrent: it is not hidden again
    -- automatically.
    reviewed_at     timestamptz,
    -- Tombstone after a denial; name and files are wiped.
    deleted_at      timestamptz,
    CONSTRAINT torrents_dht_key_key UNIQUE (dht_key),
    CONSTRAINT torrents_info_hash_v1_key UNIQUE (info_hash_v1),
    CONSTRAINT torrents_info_hash_v2_key UNIQUE (info_hash_v2),
    CONSTRAINT torrents_tombstone_wiped CHECK (
        deleted_at IS NULL OR (name = '' AND files = '[]'::jsonb)
    )
);

CREATE UNIQUE INDEX torrents_change_seq ON torrents (change_seq);
-- A 20-byte key may be the truncated form of a stored v2 infohash.
CREATE INDEX torrents_v2_prefix ON torrents ((substring(info_hash_v2 FROM 1 FOR 20)));

-- File lists compress well; lz4 is faster than pglz. Servers built without lz4
-- keep the default.
DO $$
BEGIN
    ALTER TABLE torrents ALTER COLUMN files SET COMPRESSION lz4;
EXCEPTION WHEN OTHERS THEN
    RAISE NOTICE 'lz4 compression unavailable, keeping default: %', SQLERRM;
END
$$;

-- Rejects a torrents.files value that is not an array of at most 2 000
-- entries. Runs only when the column is written. Trigger functions need no
-- EXECUTE grant to fire.
CREATE FUNCTION dc3_check_files()
RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public, pg_temp
AS $$
BEGIN
    IF jsonb_typeof(NEW.files) IS DISTINCT FROM 'array' THEN
        RAISE EXCEPTION 'torrents.files must be a JSON array'
            USING ERRCODE = 'check_violation', TABLE = 'torrents', COLUMN = 'files';
    END IF;
    IF jsonb_array_length(NEW.files) > 2000 THEN
        RAISE EXCEPTION 'torrents.files holds more than 2000 entries'
            USING ERRCODE = 'check_violation', TABLE = 'torrents', COLUMN = 'files';
    END IF;
    RETURN NEW;
END
$$;

REVOKE ALL ON FUNCTION dc3_check_files() FROM PUBLIC;

CREATE TRIGGER torrents_files_shape
    BEFORE INSERT OR UPDATE OF files ON torrents
    FOR EACH ROW EXECUTE FUNCTION dc3_check_files();

-- Lease queue of keys whose metadata has not been fetched yet.
CREATE TABLE pending (
    dht_key         bytea PRIMARY KEY CHECK (octet_length(dht_key) = 20),
    discovered_at   timestamptz NOT NULL DEFAULT now(),
    seen_count      bigint NOT NULL DEFAULT 1 CHECK (seen_count >= 0),
    attempts        integer NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    next_attempt_at timestamptz NOT NULL DEFAULT now(),
    lease_until     timestamptz,
    last_attempt_at timestamptz,
    gave_up         boolean NOT NULL DEFAULT false
);

CREATE INDEX pending_ready ON pending (next_attempt_at) WHERE NOT gave_up;
CREATE INDEX pending_gave_up ON pending (last_attempt_at) WHERE gave_up;

CREATE TABLE denylist (
    key        bytea PRIMARY KEY CHECK (octet_length(key) IN (20, 32)),
    reason     text NOT NULL
               CHECK (reason IN ('dmca', 'csam', 'csam-auto', 'abuse', 'private', 'other')),
    note       text CHECK (char_length(note) <= 2000),
    created_at timestamptz NOT NULL DEFAULT now(),
    created_by text NOT NULL CHECK (char_length(created_by) BETWEEN 1 AND 128)
);

CREATE INDEX denylist_prefix ON denylist ((substring(key FROM 1 FOR 20)));

-- True when any denylist entry covers one of a torrent's keys. The body is
-- bound when the function is created, so the caller's search_path does not
-- matter. Invoker's rights: callers need SELECT on denylist.
CREATE FUNCTION dc3_key_denied(p_dht_key bytea, p_v1 bytea, p_v2 bytea)
RETURNS boolean
LANGUAGE sql STABLE PARALLEL SAFE
RETURN EXISTS (
    SELECT 1 FROM denylist d
    WHERE substring(d.key FROM 1 FOR 20) IN (p_dht_key, p_v1, substring(p_v2 FROM 1 FOR 20))
);

REVOKE ALL ON FUNCTION dc3_key_denied(bytea, bytea, bytea) FROM PUBLIC;

-- Abuse reports. No IP addresses, by design (R11). The web role stores them
-- only through submit_report().
CREATE TABLE reports (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    torrent_id      bigint REFERENCES torrents (id) ON DELETE SET NULL,
    -- The key the visitor reported: 20 or 32 bytes.
    dht_key         bytea NOT NULL CHECK (octet_length(dht_key) IN (20, 32)),
    reason          text NOT NULL CHECK (reason IN ('csam', 'copyright', 'malware', 'other')),
    message         text NOT NULL CHECK (char_length(message) <= 2000),
    contact         text CHECK (char_length(contact) <= 320),
    created_at      timestamptz NOT NULL DEFAULT now(),
    status          text NOT NULL DEFAULT 'open'
                    CHECK (status IN ('open', 'actioned', 'dismissed')),
    resolved_at     timestamptz,
    resolved_by     text CHECK (char_length(resolved_by) BETWEEN 1 AND 128),
    resolution_note text CHECK (char_length(resolution_note) <= 2000),
    CONSTRAINT reports_resolution CHECK (
        (status = 'open') = (resolved_at IS NULL AND resolved_by IS NULL)
    )
);

-- Open reports, oldest first; also bounds the open-report count.
CREATE INDEX reports_open ON reports (created_at) WHERE status = 'open';
CREATE INDEX reports_torrent ON reports (torrent_id) WHERE torrent_id IS NOT NULL;
-- "Is another CSAM report about this torrent open?"
CREATE INDEX reports_open_csam ON reports (torrent_id)
    WHERE status = 'open' AND reason = 'csam' AND torrent_id IS NOT NULL;

CREATE TABLE audit_log (
    id      bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    at      timestamptz NOT NULL DEFAULT now(),
    actor   text NOT NULL CHECK (char_length(actor) BETWEEN 1 AND 128),
    action  text NOT NULL CHECK (char_length(action) BETWEEN 1 AND 64),
    subject text NOT NULL CHECK (char_length(subject) <= 256),
    detail  jsonb NOT NULL DEFAULT '{}'::jsonb
);

CREATE INDEX audit_log_at ON audit_log (at);
-- The auto-hide budget counts recent rows of one action.
CREATE INDEX audit_log_action_at ON audit_log (action, at);

-- Per-day pipeline counters (UTC days).
CREATE TABLE stats_daily (
    day          date PRIMARY KEY,
    discovered   bigint NOT NULL DEFAULT 0 CHECK (discovered >= 0),
    fetched      bigint NOT NULL DEFAULT 0 CHECK (fetched >= 0),
    fetch_failed bigint NOT NULL DEFAULT 0 CHECK (fetch_failed >= 0),
    blocked      bigint NOT NULL DEFAULT 0 CHECK (blocked >= 0)
);

-- Owner-managed configuration that database functions read.
CREATE TABLE settings (
    key   text PRIMARY KEY CHECK (char_length(key) BETWEEN 1 AND 64),
    value text NOT NULL CHECK (char_length(value) <= 1024),
    -- Numeric settings hold a non-negative integer that fits in int4.
    CONSTRAINT settings_numeric CHECK (
        key NOT IN ('autohide_per_hour', 'open_reports_cap') OR value ~ '^[0-9]{1,9}$'
    )
);

INSERT INTO settings (key, value) VALUES
    -- CSAM auto-hides allowed per hour (web.autohide_per_hour; migrate syncs it).
    ('autohide_per_hour', '60'),
    -- submit_report refuses new reports while this many are open.
    ('open_reports_cap', '100000');

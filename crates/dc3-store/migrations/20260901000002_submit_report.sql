-- submit_report(): the web role's only write path (docs/03-design.md §10).
--
-- SECURITY DEFINER: the function runs with the privileges of its owner, the
-- role that runs the migrations (dc3_owner in a deployment). dc3_web gets
-- EXECUTE on it and no write privilege on any table. Because the body runs
-- with the owner's privileges:
--   * search_path is pinned, with pg_temp last, as the PostgreSQL manual
--     advises for SECURITY DEFINER functions;
--   * every table and every function of ours is schema-qualified;
--   * built-in functions resolve in pg_catalog, which comes first. The
--     20-byte prefix keeps the standard `substring(x FROM 1 FOR 20)` form,
--     which is the expression the indexes are built on.
--
-- In the caller's transaction, the function:
--   1. validates its arguments (SQLSTATE 22023, invalid_parameter_value);
--   2. for a csam report, takes the change-feed lock in shared mode before any
--      other lock (§10: the lock comes first in every writer);
--   3. refuses the report when settings.open_reports_cap reports are already
--      open (SQLSTATE D3R01). The count stops at the cap. Concurrent calls
--      may overshoot the cap by at most their number;
--   4. links the report to the torrent the key names. A 20-byte key matches
--      dht_key, info_hash_v1 or the 20-byte prefix of info_hash_v2; a
--      32-byte key matches info_hash_v2. Tombstoned or denylisted torrents
--      are not linked. Hidden torrents are linked, so dismissing one report
--      cannot un-hide a torrent that another open csam report is about;
--   5. hides the torrent only when all of these hold:
--        - the reason is csam;
--        - the torrent is linked and not hidden;
--        - reviewed_at is NULL (no admin has dismissed a report about it);
--        - no other csam report about it is open;
--        - fewer than settings.autohide_per_hour audit_log rows with action
--          'auto-hide' are less than an hour old.
--      The budget check runs under its own advisory lock, so concurrent calls
--      never exceed the budget. Hiding sets hidden_at, bumps change_seq and
--      writes audit_log (actor 'web', action 'auto-hide', subject = key hex);
--   6. returns the report id, whether it hid the torrent, and whether a hide
--      was skipped only because the hourly budget was used up.
--
-- The lock keys are dc3_store::CHANGE_LOCK_KEY and dc3_store::AUTOHIDE_LOCK_KEY;
-- the crate's tests check that the values here match.
CREATE FUNCTION public.submit_report(p_key bytea, p_reason text, p_message text, p_contact text)
RETURNS TABLE (report_id bigint, hidden boolean, budget_exhausted boolean)
LANGUAGE plpgsql
VOLATILE
SECURITY DEFINER
SET search_path = pg_catalog, public, pg_temp
AS $fn$
DECLARE
    c_change_lock      CONSTANT bigint := 7233681928533508097;   -- 0x6463336368670001, "dc3chg"
    c_autohide_lock    CONSTANT bigint := 7233681950024925185;   -- 0x6463336869640001, "dc3hid"
    c_default_cap      CONSTANT bigint := 100000;
    c_default_budget   CONSTANT bigint := 60;
    c_budget_window    CONSTANT interval := interval '1 hour';
    c_max_message      CONSTANT integer := 2000;
    c_max_contact      CONSTANT integer := 320;
    v_cap         bigint;
    v_open        bigint;
    v_budget      bigint;
    v_used        bigint;
    v_torrent_id  bigint;
    v_hidden_at   timestamptz;
    v_reviewed_at timestamptz;
    v_report_id   bigint;
    v_hide        boolean := false;
    v_exhausted   boolean := false;
BEGIN
    -- 1. Arguments. No table is touched and no lock is taken before step 2.
    IF p_key IS NULL OR pg_catalog.octet_length(p_key) NOT IN (20, 32) THEN
        RAISE EXCEPTION 'submit_report: the key must be 20 or 32 bytes'
            USING ERRCODE = 'invalid_parameter_value';
    END IF;
    IF p_reason IS NULL OR p_reason NOT IN ('csam', 'copyright', 'malware', 'other') THEN
        RAISE EXCEPTION 'submit_report: unknown reason'
            USING ERRCODE = 'invalid_parameter_value';
    END IF;
    IF p_message IS NULL OR pg_catalog.char_length(p_message) > c_max_message THEN
        RAISE EXCEPTION 'submit_report: the message must be at most % characters', c_max_message
            USING ERRCODE = 'invalid_parameter_value';
    END IF;
    IF p_contact IS NOT NULL AND pg_catalog.char_length(p_contact) > c_max_contact THEN
        RAISE EXCEPTION 'submit_report: the contact must be at most % characters', c_max_contact
            USING ERRCODE = 'invalid_parameter_value';
    END IF;

    -- 2. Only a csam report can bump change_seq.
    IF p_reason = 'csam' THEN
        PERFORM pg_catalog.pg_advisory_xact_lock_shared(c_change_lock);
    END IF;

    -- 3. Open-report cap.
    SELECT s.value::bigint INTO v_cap
      FROM public.settings s
     WHERE s.key = 'open_reports_cap';
    v_cap := coalesce(v_cap, c_default_cap);
    SELECT pg_catalog.count(*) INTO v_open
      FROM (SELECT 1 FROM public.reports r WHERE r.status = 'open' LIMIT v_cap) AS o;
    IF v_open >= v_cap THEN
        RAISE EXCEPTION 'submit_report: % reports are already open', v_cap
            USING ERRCODE = 'D3R01',
                  HINT = 'An admin must resolve open reports before new ones are accepted.';
    END IF;

    -- 4. The torrent the key names.
    IF pg_catalog.octet_length(p_key) = 20 THEN
        SELECT t.id INTO v_torrent_id
          FROM public.torrents t
         WHERE (t.dht_key = p_key
                OR t.info_hash_v1 = p_key
                OR substring(t.info_hash_v2 FROM 1 FOR 20) = p_key)
           AND t.deleted_at IS NULL
           AND NOT public.dc3_key_denied(t.dht_key, t.info_hash_v1, t.info_hash_v2)
         ORDER BY (t.dht_key = p_key) DESC, t.id
         LIMIT 1;
    ELSE
        SELECT t.id INTO v_torrent_id
          FROM public.torrents t
         WHERE t.info_hash_v2 = p_key
           AND t.deleted_at IS NULL
           AND NOT public.dc3_key_denied(t.dht_key, t.info_hash_v1, t.info_hash_v2);
    END IF;

    -- 5. Whether to hide. The row lock serialises reports about one torrent,
    -- and the WHERE clause is re-checked against the latest committed row.
    IF p_reason = 'csam' AND v_torrent_id IS NOT NULL THEN
        SELECT t.hidden_at, t.reviewed_at INTO v_hidden_at, v_reviewed_at
          FROM public.torrents t
         WHERE t.id = v_torrent_id
           AND t.deleted_at IS NULL
           AND NOT public.dc3_key_denied(t.dht_key, t.info_hash_v1, t.info_hash_v2)
           FOR UPDATE;
        IF NOT FOUND THEN
            -- Tombstoned or denied since step 4.
            v_torrent_id := NULL;
        ELSIF v_hidden_at IS NULL
              AND v_reviewed_at IS NULL
              AND NOT EXISTS (SELECT 1
                                FROM public.reports r
                               WHERE r.torrent_id = v_torrent_id
                                 AND r.status = 'open'
                                 AND r.reason = 'csam') THEN
            PERFORM pg_catalog.pg_advisory_xact_lock(c_autohide_lock);
            SELECT s.value::bigint INTO v_budget
              FROM public.settings s
             WHERE s.key = 'autohide_per_hour';
            v_budget := coalesce(v_budget, c_default_budget);
            SELECT pg_catalog.count(*) INTO v_used
              FROM (SELECT 1
                      FROM public.audit_log a
                     WHERE a.action = 'auto-hide'
                       AND a.at > pg_catalog.now() - c_budget_window
                     LIMIT v_budget) AS u;
            v_hide := v_used < v_budget;
            v_exhausted := NOT v_hide;
        END IF;
    END IF;

    INSERT INTO public.reports (torrent_id, dht_key, reason, message, contact)
    VALUES (v_torrent_id, p_key, p_reason, p_message, p_contact)
    RETURNING id INTO v_report_id;

    IF v_hide THEN
        UPDATE public.torrents
           SET hidden_at = pg_catalog.now(),
               change_seq = pg_catalog.nextval('public.change_seq'::regclass)
         WHERE id = v_torrent_id;
        INSERT INTO public.audit_log (actor, action, subject, detail)
        VALUES ('web', 'auto-hide', pg_catalog.encode(p_key, 'hex'),
                pg_catalog.jsonb_build_object('report_id', v_report_id,
                                              'torrent_id', v_torrent_id));
    END IF;

    -- 6.
    report_id := v_report_id;
    hidden := v_hide;
    budget_exhausted := v_exhausted;
    RETURN NEXT;
END
$fn$;

COMMENT ON FUNCTION public.submit_report(bytea, text, text, text) IS
    'Stores a visitor report and may hide the torrent (bounded, audited). The web role''s only write path.';

-- Only the roles granted in the grants migration may call it.
REVOKE ALL ON FUNCTION public.submit_report(bytea, text, text, text) FROM PUBLIC;

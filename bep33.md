# BEP 33 scrape worker — plan

Goal: a dedicated scrape thread that keeps the index to live torrents only,
bounding disk on small machines. Prioritizes unchecked torrents first, then
stalest recheck. Torrents with no seeders are removed (re-added later by the
normal pipeline if they become live).

Status: in implementation (2026-10-06). Done: §0 bloom filter + estimator (`dc3-dht/src/bloom.rs`); §2 krpc scrape/seed wire fields (`krpc.rs`: `GetPeers{scrape}`, `AnnouncePeer{seed}`, `BFsd`/`BFpe` 256 B strict); §2+§5 answer-half (`peer_store.rs` seed flag + per-family `BFsd`/`BFpe`, `responder.rs` scrape answers with 532 B headroom, `lookup.rs` scrape traversal + `ScrapeOutcome` OR-union estimator, `node.rs` dedicated scrape bucket + `query_scrape`, `Dht::scrape` sibling API, `scrape_packets_per_sec`/`scrape_query_timeout` knobs); §3 schema + store (`migrations/20260901000004_scrape.sql`: `torrents.last_scraped_at/seeders_est/scrape_failures` + `torrents_scrape_due` partial index + `removed_keys(key,removed_at,removals,sightings)` + crawler grants; `Store::{claim_scrape_due,record_scrape,tombstone_dead,purge_tombstoned,note_removal,note_removed_sighting,removal_cooldown_remaining,removed_keys_count}` + `ScrapeItem`; `complete()` clears removal memory (LB-17); 6 new DB tests + index list updated, 38 store tests green). Next: §6 crawl config knobs. Everything in §1 was re-verified against
the tree on 2026-10-06 (exact `file:line` throughout); §10 logs what the
double-check corrected. §11 adds executable proofs (scripts in
`/tmp/opencode/bep33proof/` plus `/tmp/bep33_check_vectors.py`,
`/tmp/bep33_check_math.py`, `/tmp/bep33_check_sql.py`,
`/tmp/bep33_final_audit.py` in `/tmp/`, `/tmp/bep33_review_20261006.py` in `/tmp/`, and `/tmp/opencode/eff_calc.py` —
all run 2026-10-06 **[FIXED: path]** prior header implied every script lives
in `bep33proof/`; `eff_calc.py` lives in `/tmp/opencode/` and the five
`/tmp/bep33_*.py` oracles live in `/tmp/`) for every quantitative claim.
Prior revision errors corrected in this pass are marked **[FIXED]** with the
reason; load-bearing design corrections are marked **[LB-…]** (§10 logs all of them, LB-1…LB-32; the latest review added LB-27…LB-32, proven in
`/tmp/bep33_review_20261006.py`).

## 0. BEP 33 in 60 seconds (spec facts, not our choices)

Canonical source: `https://www.bittorrent.org/beps/bep_0033.html` (BEP 33,
"DHT Scrapes", Draft, version `9c5c1dd…`, last-modified 2016-07-21) and, for
packet size only, `https://www.bittorrent.org/beps/bep_0032.html`.

- Request: `get_peers` with `scrape=1` in the `a` dict. Response: the `r` dict
  gains `BFsd` (seeds) and `BFpe` (peers), each a **256-byte** Bloom filter
  built per the BEP's pseudocode (all values unsigned; implementations must be
  functionally equivalent). **[FIXED: conditional]** Filters are required only
  **when the responder has database entries for that infohash** — spec: "If
  `scrape` is set to 1 in the request *and* the responding node has database
  entries for that infohash then it must add … `BFsd` … `BFpe`". A scrape to a
  node with no entries legitimately returns no filters; absence of filters on
  one response is not absence of swarm. Only IP addresses (v4 or v6) may be
  inserted; port numbers or host names are forbidden. `k = 2`, `m = 256*8 =
  2048` bits, `insertIP` via `sha1(ip)`, `index1 = hash[0]|hash[1]<<8`,
  `index2 = hash[2]|hash[3]<<8`, `index %= m`. Proof: `/tmp/opencode/bep33proof/proof_bloom.py`
  builds this exact construction, checks `len == 256`, and inverts the
  estimator on synthetic inserts.
- Announce: a seeding node adds `seed=1` to `a`. Responders must store seed
  status per `<infohash, IP>` (unique IPs; port/seed updatable). **[FIXED:
  default rule]** Spec default: "if no seed key was present or the value was
  not 1 then it should assume that the announcing node is a peer." A responder
  must therefore treat missing/`≠1` as peer, not unknown. Spec: "`<Infohash,IP>`
  tuples … must be unique while port numbers, seed status and other values may
  be updated."
- `noseed=1` asks for non-seeds on a best-effort basis (spec modal verb is
  "should", not "must"; we will send neither `noseed` nor need it).
- Size guidance: keep `max(seeds, peers)` below 6000 per infohash (filters
  saturate near ~8000); achieve it by withholding tokens past the cap, and a
  token-less reply means "don't announce here". **[FIXED: exact bound]**
  Prior draft wrote "~6000"; spec says "below 6000" (guidance, "should try").
  Numbers 6000 / ~8000 exact per spec: "Nodes should try to keep
  *max(seeds,peers)* below 6000 since the bloom filters reach a false positive
  rate of 1.0 as they approach a set size of ~8000 entries." Mechanism:
  "This can be achieved by not responding with tokens to get_peers requests
  for an infohash which has reached this limit." Token semantics (GET_PEERS
  section): "If a node does not return a token it indicates that it currently
  cannot accept announces for this infohash. Thus the requesting node should
  only announce to the K nodes which are the closest to the target *and* have
  returned a token" (sic double "the" in original). Proof of saturation:
  `proof_bloom.py` computes standard FP `(1-e^{-kn/m})^k` with `m=2048,k=2`:
  `n=6000 → fp≈0.9943 (5.8 zero bits left)`, `n=8000 → fp≈0.9992 (0.8 zeros)`,
  `n=12000 → 1.0`. The 6000 cap is therefore load-bearing, not aesthetic.
- Packet guidance: filters cost 512+ bytes; trim `values`/`nodes` so replies
  still fit BEP 32 (~1024 bytes). **[FIXED: citation]** The "~1024" figure is
  BEP 32, not BEP 33. BEP 33 says only: "Implementations which also implement
  BEP 32 must take care not to exceed the specified packet sizes since the
  filters require over 512 bytes in the packet. This can be achieved by
  returning less values and only returning the mandatory node lists even when
  additional ones are requested." BEP 32 states: "A node … must not send UDP
  datagrams with a payload larger than 1024 octets." "512+" = 2×256 B payload
  plus bencode keys/overhead. Proof: `proof_trim.py` shows 88×v4-values +
  8×v4-nodes ≈ 1012 B fits, but +524 B filters ≈ 1536 B overflows (524 as computed in that script; exact 532 B per LB-29 — same overflow conclusion) — hence §5
  must account filter bytes in the trim budget.
- Reader rules: **[FIXED: MAY, not MUST]** spec says "The requesting nodes
  *may* perform sanity checks … e.g. checking if they are not full (within the
  6000 entry limit …) or if the returned values are actually contained in
  either of the two filters." Prior "Reader rules: sanity-check" overstated
  obligation. Correct: MAY/SHOULD hygiene. If a response has only `values`
  when a scrape was requested, "an implementation *may* generate a bloom
  filter for that list locally", which "*should* be considered as a peer
  filter" — hence never proof of zero seeds (editorial gloss, valid
  inference). **[FIXED: omitted proviso]** Before inserting such values "it
  should check if the IPs are not present in the seed filters of other nodes
  to prevent the item from being counted as seed and peer at the same time."
  The union/counting implementation must implement this dedup check.
- Scheduling guidance: piggyback active torrents on announces; inactive ones
  carefully — 4 concurrent scrapes at startup, 1 steady-state, **3 concurrent
  RPCs** (not "~3 RPCs per scrape"), 10s timeouts, randomized intervals, cache
  node lists across repeat scrapes of the same torrent. **[FIXED: concurrency
  wording]** Spec: "The number of concurrently active scrapes for inactive
  torrents should be limited, e.g. 4 during startup and 1 for steady state
  operation" ("e.g." = example; "~" fairly conveys). "Scrapes for inactive
  torrents should be performed with a lower RPC concurrency and more lenient
  timeouts than regular lookups … 3 concurrent RPC calls and 10s timeouts
  should be sufficient." Prior "§0 ~3 RPCs per scrape" is corrected to "3
  concurrent RPCs". "Intervals … should be somewhat randomized to avoid
  wave-like traffic patterns." "Caching node lists from previous lookups …
  can significantly cut down on … traffic for repeated scrapes." Piggyback:
  "Scrapes for active torrents can be piggybacked on the announces and thus
  should only incur minimal additional cost." **[FIXED: omitted rule]**
  First scheduling bullet dropped previously: "Clients should only perform DHT
  scrapes on torrents where no tracker is available or tracker scrapes are not
  successful." Our crawler has no tracker path, so DHT scraping everything is a
  conscious divergence — document it, do not silently omit.
- Estimator (for §2 implementors): `c = min(m-1, countZeroBits)`,
  `size = log(c/m) / (k·log(1-1/m))` (log base cancels — `ln`, `log2`,
  `log10` give identical results; proven in
  `/tmp/bep33_review_20261006.py` §C: `z=619 → 1224.9309` under all three
  bases), union by bitwise OR. Proof in
  `proof_bloom.py`: `estimate` inverts synthetic `n=100 → 100.0`,
  `n=1256 → 1255.7`; OR-union is monotone (`zeros_union ≤ zeros_part` ⇒
  `est_union ≥ est_part`); edge cases proven: empty filter clamps to `m-1`
  and estimates `1/k = 0.5` (must special-case empty), saturated filter (0
  zeros) makes `log(0)` undefined and must be reported saturated/unknown, not
  zero. **[FIXED 2026-10-06: explicit mapping]** The raw formula never
  returns `≤0` (minimum `0.5` at `z=2047/m`, maximum `7805.7` at `z=1` —
  proven in `/tmp/bep33_audit_fix.py`: `est min 0.5000, max 7805.70`), so
  threshold `0` (§6) never fires without the mapping. Implement exactly:
  `zeros==m → 0` (dead, empty swarm), `zeros==0 → UNKNOWN/saturated`
  (never `0`, never huge — treat as unaware for classification), else the
  formula. BEP test vector (192.0.2.0/24 + 2001:DB8… → fixed 256 B hex, 1256
  inserts → 1224.9308) is the conformance vector §9 must pin.

## 1. What the code does today (verified)

### 1.1 No seeder awareness anywhere

- `crates/dc3-peer/src/fetch.rs:125` reads and discards bitfield/have
  messages. **[FIXED: line precision]** `:125` is the comment ("Keep-alives
  and non-extended messages (bitfield, have, …) are skipped."); operative
  `continue` is `:126-128` (`let Frame::Extended(payload) = frame else {
  continue; }`). Same discard repeated Phase 2 at `:149-155` and `:162`.
  Metadata (BEP 9) is fetched from any reachable peer; seeder and
  leecher count identically. Proof: no `seed`/`seeder` symbol in
  `dc3-peer/src/fetch.rs` (qualifier: the crate does contain a test-only
  `Seeder`/`seeder` harness in `dc3-peer/src/seeder.rs` + `lib.rs:14,21` —
  a `ut_metadata` test server, not fetch-path awareness); bitfield never
  influences peer selection.
- `dc3-dht` stores no seed bit: `PeerStore::Slot` is
  `Vec<(SocketAddr, Instant)>` (`crates/dc3-dht/src/peer_store.rs:51-55`,
  `announce()` at `:102-175` — `pub(crate) fn announce(` at 102, closing `}`
  at 175); tuple is only `(SocketAddr, Instant)` = expiry, no seed/bool.
  One entry per IP with port refresh is already the policy
  (`peer_store.rs:131-139` find-by-`policy.key` then `*entry = (peer,
  expires)`), which matches the BEP's "unique IPs, port updatable" half —
  only the seed bit is missing. `grep -i seed` in `dc3-dht/src` hits only
  bootstrap seeds and RNG seeds (12 hits case-insensitive:
  `node.rs:109,1290,1296,1299`, `lookup.rs:261,269,329,334,337`,
  `sampler.rs:546,880,884` — exact hits, not contiguous ranges;
  **[FIXED: exact hits]** prior `node.rs:1290-1299` / `lookup.rs:329-337`
  read as contiguous ranges; `sampler.rs:546` is
  `use rand::{Rng, SeedableRng}` which matches only case-insensitively —
  case-sensitive `seed` is 11 hits, the 12th needs `-i`. Proof:
  `/tmp/bep33_check_sql.py` analogues plus direct `rg -n seed` vs `rg -in`
  in `/tmp` runs). KRPC has no `seed`
  field (`Method::AnnouncePeer`, `crates/dc3-dht/src/krpc.rs:92-97`, decode
  `:274-301` parses exactly `info_hash/implied_port/port/token`, encodes same
  four at `:442-455`).
- The web UI shows "Times seen" (`seen_count`, our own deduplicated DHT
  sighting count), never swarm state
  (`crates/dc3-web/templates/torrent.html:13`,
  `handlers/torrent.rs:65-80`, `handlers/api.rs:30-57`). No
  seeder/leecher string exists anywhere in `dc3-web` (`grep -i
  "seeder|leecher"` = no matches; `torrent.html` is 44 lines, only count
  row at `:13`).

### 1.2 Queue and tables

Migrations: only three files; schema lives in
`crates/dc3-store/migrations/20260901000001_schema.sql`
(`02` adds `submit_report()`, `03` grants only). Verified by directory
listing: exactly `20260901000001_schema.sql`,
`20260901000002_submit_report.sql` (only table-level object is `CREATE FUNCTION
public.submit_report(…)` at `:41` — file additionally contains
`COMMENT ON FUNCTION` at `:180-181` and `REVOKE … FROM PUBLIC` at `:184`,
no table DDL/DML), `20260901000003_grants.sql` (single `DO
$$…$$` grant block `:24-62`, no DDL/DML).

`pending` (`:92-101`) has **8** columns: `dht_key` PK, `discovered_at`,
`seen_count`, `attempts`, `next_attempt_at`, `lease_until`,
`last_attempt_at`, `gave_up`. Ready index: `pending_ready(next_attempt_at)
WHERE NOT gave_up` (`:103`); gave-up index on `last_attempt_at` (`:104`).
Proof: direct column count 93–100 = 8; prior draft's omission of
`last_attempt_at` is the §10-logged correction.

`torrents` (`:15-48`) has 17 columns: `id`, `dht_key` (UNIQUE), `info_hash_v1`
(UNIQUE), `info_hash_v2` (UNIQUE), `name` (≤1024 chars), `total_size`,
`file_count`, `files` jsonb, `files_truncated`, `piece_length`, `seen_count`,
`first_seen_at`, `last_seen_at`, `change_seq` (no default — always set),
`hidden_at`, `reviewed_at`, `deleted_at`. `UNIQUE(change_seq)` at `:50`;
v2-prefix index at `:52` (`substring(info_hash_v2 FROM 1 FOR 20)` — the form
the planner matches, per schema header comment).

`claim()` (`crates/dc3-store/src/crawler.rs:70-81`): leases due rows
(`NOT gave_up`, `next_attempt_at <= now()`, lease expired) ordered by
**`next_attempt_at`**, `LIMIT min(n, MAX_CLAIM=10_000)`
(`crates/dc3-store/src/lib.rs:112`), `FOR UPDATE SKIP LOCKED`. Atomically
`UPDATE … SET lease_until = now()+make_interval … WHERE dht_key IN (SELECT …
FOR UPDATE SKIP LOCKED) RETURNING …` — the lease is what makes multi-worker
claim safe; §3's scrape claim must copy this shape (see LB-7).

`fail()` (`crawler.rs:91-100,468-482`): backoff
`min(5min × 2^attempts, 7days)` → waits 5, 10, 20, 40, 80 min; `gave_up` on
the 6th failure (`MAX_FETCH_ATTEMPTS=6`, `lib.rs:135-139`), ~155 min ≈2.6h
after the first attempt. Proof: `FAIL_SQL` (`:91-100`) uses pre-increment
`attempts` (`attempts+1`, `power(2, least(attempts,30))`, `gave_up =
attempts+1 >= $4`); `lib.rs:135 FAIL_BASE_BACKOFF=5*60`, `:137
FAIL_MAX_BACKOFF=7*24*3600`, `:139 MAX_FETCH_ATTEMPTS=6`.
`/tmp/opencode/bep33proof/proof_backoff.py` executes the formula:
`[300,600,1200,2400,4800]s = 5,10,20,40,80min`, sum `9300s = 155min ≈2.58h`,
6th sets `gave_up=true`. `purge_gave_up(older_than)` deletes only
`gave_up` rows (`crawler.rs:502-510`) — the queue, never `torrents`.

`complete()` (`crawler.rs:337-462`): deletes the `pending` row first (even on
the denied path, `:366-371` `DELETE … RETURNING seen_count, discovered_at`);
denylist hit → commit + `Err(Denied)` (`:380-389` `SELECT
dc3_key_denied($1,$2,$3)` then `tx.commit(); return Err(Denied)`);
otherwise **[FIXED: match priority]** single lookup `WHERE dht_key=$1 OR
info_hash_v1=$2 OR info_hash_v2=$3 ORDER BY id FOR UPDATE` (`:391-399`), then
Rust picks `find(dht_key match).or_else(first)` (`:409-412`) — i.e.
**dht_key-match, else lowest `id`**. There is **no v1-before-v2 priority**;
prior "matching `dht_key`, then `info_hash_v1`, then `info_hash_v2`
(`:391-412`)" is wrong and is corrected here. Conflict guard `:418-421`
keeps stored hash if another row owns the new one. Preserves `first_seen_at`
by omission and always bumps `change_seq` via `TORRENT_UPDATE_SQL`
(`:111-121`: no `first_seen_at`, `deleted_at=NULL` at `:119`,
`change_seq=nextval` at `:120`). **[FIXED: line range]** execute block is
`:422-436`, not `:422-437` (`:437` is `(id, pending.is_some())`).
Inserts set `first_seen_at = coalesce(pending.discovered_at, now())` and
`seen_count = max(pending_seen, 1)` (`TORRENT_INSERT_SQL :123-127`,
`:440-455` with `.bind(add_seen.max(1))` at `:450`). Update path also clears
`deleted_at` — i.e. `complete()` **revives tombstones** (doc comment `:329`
"A tombstoned row is restored"; matters for §4 LB-13).

`crawl.max_pending` default `5_000_000`
(`crates/dhtcrawler3/src/config.rs:214`, asserted `crawl.rs:406`) caps the
**queue**: at 2×, admission closes; at 1×, only Sample/Announce priority keys
are admitted (`crawler.rs:182-210`: `pending_depth_reaches(max*2)` →
`Closed`; all-priority batch → `Open`; `pending_depth_reaches(max)` →
`PriorityOnly`; else `Open`; `PriorityOnly = rest.partition(|m| m.priority)`
at `:639`, non-priority merely counted via `OBSERVE_COUNT_ONLY_SQL`
(definition `crawler.rs:63-68` — `const` at `:63`, `///` doc at `:62` —
usage `:651-658` **[FIXED 2026-10-06: both
cited]** prior cited usage only). Gate lives in `Store::observe` (dc3-store), keyed off
`Observation.priority` set by admission.

Admission dedup (`crates/dhtcrawler3/src/admission.rs`): **2 generations ×
2M 64-bit salted fingerprints** (`DEDUP_GENERATION_KEYS`, `:39`;
`DEDUP_ROTATION=30min`, `:41`), rotated by age *or* when full (`:196-222`:
`insert :196-207` on `len>=capacity → rotate`, `maybe_rotate :211-217`,
`rotate :219-222`). `FingerprintSet = HashSet<u64,
BuildHasherDefault<PassThroughHasher>>` (`:145`), salt `RandomState::new()` (`:175`), `fingerprint()` hashes `key.0`
(`:184-186`). Steady-state capacity ≈4M fingerprints, not keys. A key enters
the set only after its observation transaction commits **[FIXED: range]**
(`:554-556` insert loop, `:557-559` `sources.clear()`, `:560`
`batch.clear()`, inside `Ok(outcome)` after `store.observe` succeeds
`:550-551`; prior `:554-559` off by ~3 lines, substance correct).
`GetPeers`-only keys additionally need sightings from ≥2 distinct /24 (v4) or
/48 (v6) (`GET_PEERS_MIN_SOURCES`, `:43`; `source_network()`, `:241-250`:
v4→`/24` via `u32>>8` with `V4_TAG`, v6→`/48` via `u128>>80`, IPv4-mapped
canonicalised).

### 1.3 Change feed, indexer, ranking

Every index-visible write sets `change_seq=nextval('change_seq')` (`CACHE 1`,
schema `:10-13`) under `pg_advisory_xact_lock_shared(CHANGE_LOCK_KEY)`.
**[FIXED: citation]** `lib.rs:82` is the constant definition
(`CHANGE_LOCK_KEY = 0x6463_3363_6867_0001`), not a lock call. Actual call is
`lock_change_shared` at `lib.rs:298-304`
(`SELECT pg_advisory_xact_lock_shared($1)`), invoked by `observe`
(`crawler.rs:221`), `complete` (`crawler.rs:356`), `deny_in_tx`
(`crawler.rs:700`); contract at `lib.rs:28-31`. Popularity-only observations
bump it only when `floor(log2(seen_count))` changes (`crawler.rs:23-25,164-169`
with `OBSERVE_KNOWN_SQL :26-35`, `OBSERVE_ALIAS_SQL :39-48` implementing
`CASE WHEN (old # new) > (old & new) THEN nextval ELSE keep`). Proof:
`/tmp/opencode/bep33proof/proof_log2.py` fuzzes 200k correlated
near-pairs (`b = a + randint(0,1e6)`, biased toward the same high bit) plus
power-of-two boundaries and proves `(a XOR b) < (a AND b)` ⇔ same highest set
bit ⇔ same `floor(log2)` — hence the trick exactly tracks log2 changes and
the "no churn on popularity-only" claim holds. Independent uniform pairs
(50k to 1e18) plus exhaustive `1..199 × 1..199` are covered in
`/tmp/bep33_check_math.py` (`log2 trick VERIFIED`) — see §11 and
`/tmp/bep33_audit_fix2.py` for why the `proof_log2.py`-only description
"200k random pairs" was inaccurate. **[FIXED 2026-10-06: correlated vs
independent]** Zero-edge (`seen_count = 0`) is unreachable in practice
(`DEFAULT 1`, `schema.sql:31,95`, plus `max(pending_seen, 1)` at
`crawler.rs:450`) — proven in `/tmp/bep33_final_audit.py` §7, full argument
in the §11 `proof_log2.py` bullet.

Feed: `CHANGES_SQL` (`crates/dc3-store/src/indexer.rs:17-43`) reads
`$1 < change_seq <= $2 ORDER BY change_seq LIMIT min(limit, 1000)` with a
64MiB byte budget (`MAX_FEED_PAGE`, `FEED_PAGE_MAX_BYTES`, `lib.rs:117-121`:
`MAX_FEED_PAGE=1000`, `FEED_PAGE_MAX_BYTES=64*1024*1024`; cap applied
`indexer.rs:115` `.bind(limit.min(MAX_FEED_PAGE))`, byte budget `$4`
`s.before < $4`). High-water mark: exclusive advisory lock with 200ms
`lock_timeout` (`indexer.rs:60-87` sets `lock_timeout :63-66`, takes
`SELECT pg_advisory_xact_lock($1)` exclusive at `:67`, on `55P03` rolls back
and `return Ok(None)` `:73-76`; `HWM_LOCK_TIMEOUT`, `lib.rs:147` = 200ms);
on lock timeout the indexer keeps its old mark (consumer
`dhtcrawler3/src/index.rs:320-331` keeps old `mark` on `Ok(None)`).

Indexer (`crates/dhtcrawler3/src/index.rs:130-138`): `if row.visible &&
!blocked_by_policy → upsert else delete` (`apply_rows`, exact). `visible=false`
means hidden, tombstoned (`deleted_at`) or denylisted
(`dc3-store/src/types.rs:499-501`, SQL `types.rs:208-213` `visible_sql!`:
`(hidden_at IS NULL AND deleted_at IS NULL AND NOT
dc3_key_denied(…))`). **[FIXED: file]** Prior "SQL `:208-213`" implied the
schema file; it is `types.rs`. Batch is `batch_size` (default 1000,
`dhtcrawler3/src/config.rs:234`) capped by `MAX_INDEX_BATCH=MAX_FEED_PAGE=1000`
(`dhtcrawler3/src/config.rs:63`, `dc3-store/src/lib.rs:117`) **[FIXED: wording + path]**
prior `min(batch_size=1000, 1000)` was a tautology and bare `:63` read as
`index.rs:63` (which is `IndexError`, not the cap) — correctly: effective batch =
`min(batch_size, MAX_FEED_PAGE)` **plus** the 64MiB byte budget
(`FEED_PAGE_MAX_BYTES`, `lib.rs:121`; enforced `indexer.rs:115` +
`$4` byte budget) — short pages prove nothing (`index.rs:209-241`
`drain`, default `batch_size:1000` at `dhtcrawler3/src/config.rs:234`,
`MAX_INDEX_BATCH=MAX_FEED_PAGE` at `dhtcrawler3/src/config.rs:63`, validation `:733` caps
`1..=MAX_INDEX_BATCH`; header `index.rs:4-10` "an empty page means the range
is drained … (a short page proves nothing)", break only on empty page
`:236-238`). Batch deletes also cover policy matches (`blocked_by_policy`,
`index.rs:101-107`, `matches_affixed` — "affixed" shorthand).

Ranking (`crates/dc3-search/src/index.rs:63-67,689-712`): Tantivy BM25 score
(no k1/b constants in repo — grep only hits `query.rs:53` doc "BM25 weighted
by popularity"; `IndexSettings::default()` at `index.rs:264` = Tantivy
default BM25) × `popularity_multiplier(seen) = 1 + 0.15·log10(1+seen)`
(`POPULARITY_WEIGHT=0.15` at `:67`, `NAME_BOOST=3.0` at `:63`,
`PREFIX_BOOST=0.5` at `:65`; used `:507` name, `:571-586` prefix; multiplier
`:689-695` f64, applied `:702-709` as `(f64::from(score) *
popularity_multiplier(value)) as Score` f64→f32). Proof of weights:
`proof_trim.py` tail prints `seen=0→1.0, 1→1.045, 10→1.156, 100→1.301,
1000→1.450` — sublinear, safe to mirror. Non-relevance sorts use score `0.0`
with fast-field ordering (`:711` `Newest|Size|Seen → (0.0, value, id)`,
`requires_scoring` false except Relevance `:670-672`, ordering via fast
fields `:674-686`). Fast fields in the schema: `SIZE` u64, `SEEN` u64,
`FILE_COUNT` u64, `CREATED` i64 (`:214-217` = `SIZE:214, CREATED:215, SEEN:216,
FILE_COUNT:217`;
`SIZE` at `:80`, `CREATED` i64 at `:81`, `SEEN` at `:82`, `FILE_COUNT` at
`:83`) — **there is no seeders field** (`grep seed
dc3-search/src` = no matches; drives the §7 decision).

Sorts are exactly four: relevance/newest/size/seen
(`dc3-search/src/query.rs:52-84` enum + `as_str`/`parse`;
web `SORT_CHOICES`, `dc3-web/src/handlers/search.rs:39-44`, error string at
`:58` "The sort order must be relevance, newest, size or seen.").

### 1.4 Fetch path constants (not config keys)

`[crawl]` holds `dht_port, bind_v4/v6, bootstrap, state_dir,
max_packets_per_sec=250, sampler_concurrency=32, fetch_workers=64,
max_connections=256, max_metadata_bytes=8MiB, max_inflight_metadata_bytes=
256MiB, max_pending` (`config.rs:176-217`; defaults `:196-217`: port
`dc3_dht::DEFAULT_PORT=6881` (`dc3-dht/config.rs:11`), `max_packets_per_sec =
DEFAULT_MAX_PACKETS_PER_SEC=250` (`dc3-dht/config.rs:13`),
`sampler_concurrency = DEFAULT_SAMPLER_CONCURRENCY=32` (`:15`),
`fetch_workers:64` (`:210`), `max_connections:256` (`:211`),
`max_metadata_bytes = DEFAULT_MAX_METADATA=8MiB` (`dc3-peer/src/lib.rs:44`),
`max_inflight=256MiB` (`:213`)). There are **no timeout keys** —
timeouts are constants in `crates/dhtcrawler3/src/fetch.rs:37-59`:
`CLAIM_BATCH=1` (`:37`), `CLAIM_LEASE=120s` (`:39`),
`LEASE_RENEW_INTERVAL=90s` (`:41`), `GET_PEERS_TIMEOUT=15s` (`:47`),
`KEY_DEADLINE=60s` (`:49`), `MAX_PEER_ATTEMPTS=8` (`:51`),
`PARALLEL_ATTEMPTS=3` (`:53`), connect/handshake/fetch 5/5/30s
(`:55/57/59`). New duration knobs must follow the repo's u64 pattern (e.g.
`index.poll_interval_ms: u64`, `config.rs:226`, default `1000` at `:235`,
validated `1..=3_600_000` at `:734-739`) — §6 uses `*_secs`/`*_ms` u64. Env
overrides: `DC3_<SECTION>__<KEY>` (`ENV_PREFIX DC3_` at `config.rs:32-35`,
`apply_env_overrides :476-514` plus `set_path :575-591`, unknown
section/key errors at `:526,531`
plus `deny_unknown_fields`); unknown keys are
errors.

Fetch peer discovery today: `self.peers.get_peers(key, timeout)` returns
`Vec<SocketAddr>` (`dc3-dht/src/lib.rs:155`; call site `fetch.rs:602`
`self.peers.get_peers(key, tuning.get_peers_timeout)`; test fake
`fetch.rs:1126` same signature). Piggybacking scrape data (§8) changes this
return shape — hence the §2 sibling-API decision.

### 1.5 DHT responder, budgets, sampler (exact)

- `get_peers` reply (`dc3-dht/src/responder.rs:104-117`): issue token (`:105`),
  draw up to `MAX_VALUES_PER_REPLY = MAX_PEERS_PER_KEY = 100` live same-family
  peers (`responder.rs:20` alias, `peer_store.rs:30` = 100,
  `peer_store.rs:177-198` filter live `exp>now` + same `Family`, shuffle,
  truncate), attach wanted node lists (`with_nodes` at `:97-100`; production
  filters by `want`, test harness `:259-271` same pattern).
  `krpc::encode` then trims to `MAX_DATAGRAM_OUT=1024` (`krpc.rs:19`) in order
  values→samples→nodes6→nodes (`krpc.rs:522-535` `trim`, `:539-561` `encode`
  loop; `pop_bytes :510-518`, `value_entry_len :501-507` 8 B/v4 21 B/v6;
  cited `501-561` covers all three). **[FIXED: range]** Prior `509-561`
  omitted `value_entry_len` (`:501-507`); correct span is `501-561`. **[FIXED: test location + 28/29 drift]**
  Counts live in **`responder.rs:522-532`**, not `krpc.rs:522-532`:
  "With one node list, about 88 IPv4 or 28 IPv6 values fit" (`:522`),
  asserts `Some(88)` (`:527`) and `Some(28)` (`:532`). But `responder.rs:19`
  doc says "roughly 88 IPv4 or 29 IPv6" and `docs/03-design.md:236` says
  "≤88/≤29". Test-measured 28 vs doc 29 is an unresolved 1-entry
  inconsistency — §5 must re-measure after adding 512 B filters; this plan
  follows the test (28). Proof: `proof_trim.py` reproduces both budgets
  (88×8+8×26+100≈1012 ≤1024; 28×21+8×38+100≈992 ≤1024) and proves filters
  break them (+524 B → ≈1536 B overflow; 524 as computed in that script, exact 532 B per LB-29).
- Tokens: 8 bytes, per-IP SHA-1, current+previous secret
  (`token.rs:14 TOKEN_LEN=8`, `:46-57` `issue`+`verify` current|previous with
  constant-time eq, `:60-73` `compute` canonical IP); bad token → 203
  (`responder.rs:124-131` `error_reply(…, PROTOCOL, "bad token")`,
  `krpc.rs:32-41` `PROTOCOL=203`, `METHOD_UNKNOWN=204`).
- Unknown methods carrying `target` **or** `info_hash` are answered like
  `find_node` (`krpc.rs:101-103` `Other{name,target}` doc, `:259-269` decode
  fallback target-or-info_hash else `METHOD_UNKNOWN`; `responder.rs:103`
  `FindNode|Other => with_nodes`; tests `krpc.rs:840-867`,
  `responder.rs:573-602` incl. no-target→204 at `:589-596`, bad id→203 at
  `:598-601` — prior `:573-597,589-597` off by a few lines, substance
  correct); without either → 204. Our node is therefore an "unaware" node for
  others' scrapers today (no `BFsd`/`BFpe` anywhere in `krpc.rs`).
- Budgets (units matter): global outbound `250/s` with burst = rate
  (`TokenBucket::new(max,max)` at `node.rs:316`, default 250 at
  `dc3-dht/src/config.rs:13,73`; **[FIXED: path]** bare "config.rs" citations
  in this section are `dc3-dht/src/config.rs`, not
  `dhtcrawler3/src/config.rs`). Responder `500 replies/s + 64_000 bytes/s`
  (`dc3-dht/src/config.rs:28-30,200-201`; `ResponderBudget` at
  `ratelimit.rs:91-123`, 1s burst each). Per-address spacing 1s — keyed
  per-IP on v4, per-/64 on v6 (`dc3-dht/src/config.rs:128 field, :194
  default`; `host_key` at `compact.rs:375-377` with `HOST_PREFIX_V6=64` at
  `:31`). Inbound 4/s burst 8 (`dc3-dht/src/config.rs:198-199`, test `:314`).
  **[FIXED: nuance]** The inbound limiter keys v4-IP / v6-/48 via
  `inbound_key` (`compact.rs:379-384`, `ratelimit.rs:157-168`), distinct from
  spacing's v4-IP / v6-/64. Plan's spacing claim is correct; the /48-vs-/64
  distinction is now explicit so §2's "shared spacing" does not confuse the
  two maps.
- Sampler skip pattern to mirror: unsupported (no `samples`, incl.
  Remote/Malformed) → 6h skip; timeout → 1h; anything else → 300s
  (`dc3-dht/src/config.rs:215-218` defaults `interval_sent 21600,
  min_resample 300, unsupported 6h, timeout 1h`; `sampler.rs:479-518`:
  `Ok+None → unsupported_skip :508`, `Timeout → timeout_skip :512`,
  `Remote|Malformed → unsupported_skip :513-515`, other → `min_resample
  :516`; cited `config.rs:167-174,215-218` covers sampler tuning struct +
  defaults **[FIXED 2026-10-06: range]** prior `170-174` omitted
  `sample_interval_sent` at `:168` (doc `:167`); correct struct span is
  `:167-174`). Sample interval sent in our answers: 21600s
  (`dc3-dht/src/config.rs:167,215`, sent `responder.rs:162`, asserted
  `Some(21600)` at `responder.rs:549`).

Existing metrics to extend (verified names): `dc3_discovered_total`,
`dc3_admitted_total`, `dc3_blocked_total`, `dc3_fetch_total{outcome}`
(ok/no_peers/fetch_failed/parse_error/blocked/private/denied/store_error —
`fetch.rs:85` gauge/counter decls, `:142-189` outcomes = `:142-143` doc +
`:144-161` enum + `:163-189` impl **[FIXED 2026-10-06: span]**),
`dc3_destination_skipped_total` (`:86`), `dc3_queue_depth` (`crawl.rs:38`),
`dc3_index_lag{,_seconds}` + `dc3_index_docs` (`index.rs:36-38`),
`dc3_dht_*` (`crawl.rs:307-329`).

## 2. `dc3-dht`: send + answer scrapes

- **KRPC**: parse `seed` on `announce_peer` requests and `scrape` on
  `get_peers` requests (`krpc.rs` decode/encode both directions); add
  `BFsd`/`BFpe` (each enforced 256 bytes) to `Response`
  (`krpc.rs:135-149` today ends at `interval`; new fields extend
  `response_fields :460-499` + `decode_response :316-336` + size accounting).
  **[LB-1 FIXED: underspecified routing]** `Method::GetPeers{info_hash}`
  already routes via `target()` (`krpc.rs:129` `Some(NodeId::from(info_hash))`,
  used `node.rs:723-733` for closest nodes). Adding flags means the variant
  becomes `GetPeers{info_hash, scrape: bool}` (and `AnnouncePeer` gains
  `seed: bool`), so `target()` **must keep returning `Some(info_hash)`**,
  and `name()` (`:111-120`), `query_args()` (`:432-456`), `decode_query`
  (`:248-254`), `want_for` (`node.rs:411-419`), `lookup::ask`
  (`lookup.rs:237-254`) must all be updated. "Wire `Method::target()` so
  scrapes route" is true but hides the diff — implement as above. Tests:
  vectors from the BEP pseudocode for filter build/estimate (use BEP §"Test
  vectors": 192.0.2.0/24 + 2001:DB8… fixed hex; 1256 inserts → 1224.9308;
  `proof_bloom.py` is the local oracle); unknown-method fallback unchanged.
  Missing `seed`/`≠1` defaults to peer (§0); `scrape=1` without local entries
  returns no filters (§0).
- **PeerStore**: per-infohash seed + peer IP sets (unique IPs, port/seed
  updatable per spec); **keep the 100-address store for v1** (do NOT grow
  toward the BEP's 6000-entry guidance — token omission stays a non-goal
  (see Non-goals) — and document saturation instead). `announce()` gains
  the seed flag; `Slot` shape changes.
  **[LB-5 FIXED: 6000-vs-100 contradiction]** Today `MAX_PEERS_PER_KEY=100`
  (`peer_store.rs:30`), `MAX_KEYS=20000`, one entry/IP with port refresh.
  Growing to 6000 without the token-omission cap is unsound: proof
  `proof_peerstore_cap.py` shows `20000×6000×~48 B ≈ 5.76 GB` — OOM on small
  machines — vs `20000×100×48 B ≈ 96 MB` today. And keeping 100 while
  claiming "6000-bounded" saturates filters early (`proof_bloom.py`: 100
  inserts already `fp≈0.009`, 1000 inserts `fp≈0.39`; a 100-cap swarm
  undercounts viral swarms by 60×). Correct scoping: **keep 100 for v1 and
  document saturation** (estimates above ~1000 are lower bounds; threshold 0
  stays safe not because saturation overcounts but because a truncated live
  swarm still estimates `>0` (live stays live) and a truly saturated filter
  maps to UNKNOWN, never `0` — proven in `/tmp/bep33_review_20261006.py` §E
  — see §10), or
  implement a count-only sketch beside the 100-address store. Do not claim
  "6000-bounded" while deferring the bounding mechanism (token omission).
  Token omission stays a non-goal for mega-viral infohashes only (§Non-goals).
- **Lookup**: full Kademlia traversal per infohash (a scrape is a lookup, not
  one message); 3 concurrent RPCs, 10s per-RPC timeouts per BEP guidance;
  union filters across responses; estimate via the BEP counting formula with
  the spec's sanity checks (not-full, values contained — MAY-level, §0;
  empty→0 / saturated→UNKNOWN mapping per §0) and the both-families-required
  death rule (§12 fix). Cache node lists across repeat scrapes of the same
  torrent per the BEP.
  **[LB-4 FIXED: "3 RPCs" vs traversal]** Our lookup is `ALPHA=3,
  MAX_ROUNDS=8` (`lookup.rs:27-29`, `pick(ALPHA)` at `:280,457`), per-family
  fanout (`lib.rs:157-169`), `query_timeout=4s, lookup_timeout=30s`
  (`dc3-dht/src/config.rs:116-123,190-192`), fetch `GET_PEERS_TIMEOUT=15s`
  (`fetch.rs:47`). A full traversal is **dozens of RPCs, not 3**; 10s matches
  neither 4s nor 30s/15s. BEP's "3" is concurrency, and assumes cached nodes.
  **[FIXED 2026-10-06: per-RPC vs overall — single knob was wrong]** BEP
  "10s timeouts" is per-RPC (`query_timeout` analogue: `node.rs:507`
  `after(now, tuning.query_timeout)`; scrape per-RPC `10s > 4s` is the
  "more lenient" direction). A single `scrape_timeout_secs=10` read as an
  *overall* deadline is stricter than fetch `15s` (`fetch.rs:602` via
  `Dht::get_peers(key, timeout)` at `lib.rs:155-156`, deadline passed to
  `lookup::get_peers`) and lookup `30s` (`lib.rs:177`, `node.rs:1173,1298`,
  `sampler.rs:537`) — the opposite of "more lenient". Correct: two knobs
  `scrape_query_timeout_secs=10` (per-RPC, replaces 4s) and
  `scrape_lookup_timeout_secs=60` (overall, replaces 30s/15s with headroom
  for slower rounds; `query_slow_after` stays 2s or gains its own scrape
  analogue — specify in implementation). Proof: `/tmp/bep33_audit_fix2.py`.
  Plus a node-list cache (no store exists today —
  new `HashMap<DhtKey, Vec<CompactNode>>` LRU) or
  single-round scrape; else the 25/s budget collapses (see LB-19 proof).
  **[FIXED 2026-10-06: sampler-state reuse withdrawn]** Prior "or reuse sampler
  state" is wrong: sampler tracks per-*node* sample state (`sampler.rs`), not
  per-*torrent* closest-node lists. Only a dedicated per-torrent LRU
  (`scrape_node_cache_keys`, §6) satisfies the BEP's "caching node lists from
  previous lookups". Proof of necessity: `/tmp/bep33_final_audit.py` §5
  (48 RPCs → 79 pps over budget, 8–12 cached → 13–20 pps fits).
  Union by OR across **all** aware responses (both families) per spec
  (§0 estimator proof + `/tmp/bep33_check_vectors.py` byte-exact vector +
  `/tmp/bep33_final_audit.py` §3 idempotence/monotonicity); OR separately per
  kind — all aware `BFsd` ORed → `seeders_est`, all aware `BFpe` ORed → peers
  sanity only (**[LB-25 FIXED 2026-10-06: peers-est ambiguity]** prior "union
  filters" never said which union drives death; peers union is sanity/dedup
  only, never stored, never blocks tombstone — seedless = dead by index-goal
  design, see §4);
  dedup proviso (check seed filters before
  inserting local-values filter); per-family double-count resolved in §10
  LB-23 **[LB-23 CORRECTED: OR-union, not estimate-max]** — prior
  "per-family estimate-max" contradicted the spec ("estimate the size of a
  union … simply by ORing the bits") and is unsafe (max undercounts distinct
  v4+v6 populations, risking live-kill; OR overcounts at most per-IP, which
  is the safe direction at threshold 0).
- **Unaware handling**: responses without filter keys classify the *response*,
  not the torrent; skip-cooldown for such nodes mirroring the sampler (6h
  pattern, §1.5: `Ok+None→unsupported 6h`, `Timeout→1h`, else `300s`).
  A scrape with zero aware responses is UNKNOWN (§3). Correct, with the §10
  LB-23 (unaware-backoff) addition: UNKNOWN rows need backoff (not a lookup every interval forever).
- **Budgets**: scrapes charge a **dedicated token bucket** (separate from the
  shared 250/s crawl bucket), same 1s-burst semantics (`TokenBucket::new(rate,
  rate)` pattern, `node.rs:316`). **[LB-2/LB-3 FIXED: plumbing + head-of-line]**
  `Inner::query_gated` (`node.rs:479-498`) does `spacing.reserve()` then
  `acquire_budget()` on the single `self.budget` (`:122-133` `spacing`,
  `:133` `budget`, `:135` `responder_budget`). A second bucket needs
  `Inner::scrape_budget: Mutex<TokenBucket>` + `query_scrape()` that shares
  `shared.spacing` but charges the scrape bucket; lock order
  `spacing→budget` must stay consistent or scrape+crawl deadlock. Sharing
  `QuerySpacing` serializes scrape+crawl to 1/s correctly (per-IP v4, per-/64
  v6), but the second contender sleeps up to `max_send_wait` (4s,
  `dc3-dht/src/config.rs:124-126`). Under load this head-of-line-blocks
  crawl — document `max_send_wait` applies to the sum and add a `Throttled`
  metric (cf. `QueryError::Throttled` at `node.rs:69-84`, `Send` at `:79`,
  counted as `DropReason::Throttled`/`SendError` in `stats.rs:60-86`;
  **[FIXED: name+lines]** prior `SendError at node.rs:62-72` named the wrong
  enum (`SendError` is a `DropReason` in `stats.rs`, not a `node.rs` error)
  and pointed at `RECV_ERROR_PAUSE/MIN_BUDGET_WAIT/SHUTDOWN_GRACE` constants
  (`:58-65`); the query error is `QueryError` at `:69-84`). Per-IP-/64 spacing stays
  shared — scrape + crawl queries to one host must not double up past 1/s.
  New key `scrape_packets_per_sec` (default 25/s); total outbound rises by up
  to the scrape rate, so the ops "good citizen" story must quote both numbers
  (250/s crawl + 25/s scrape) in `check-config`.
- **API change**: `Dht::get_peers` currently returns `Vec<SocketAddr>`
  (`lib.rs:155`). Scrapes need a richer return (peers + per-response filter
  data + aware/unaware flags). Add a sibling method (e.g. `scrape(key) ->
  ScrapeReport`) rather than changing `get_peers`' signature, so
  `fetch.rs:602` and its test fake (`:1126`) keep working until §8. Correct
  choice; §8 updates the fake.
- **v1 first**: scrape by DHT key (== v1 infohash). Hybrids share the lookup;
  pure-v2 keys need a v2 swarm path — deferred to a later pass. **[FIXED:
  key-size note]** `removed_keys` (§3) is 20 B-only (`CHECK
  octet_length=20`); pure-v2 later needs 32 B (like `denylist`'s `IN (20,32)`
  at `schema.sql:107`).

## 3. Schema (new migration `04_scrape.sql`)

```sql
-- [LB-6..LB-12 FIXED — see notes below; do not use the prior draft verbatim]
ALTER TABLE torrents ADD COLUMN last_scraped_at timestamptz NULL;
ALTER TABLE torrents ADD COLUMN seeders_est integer NULL CHECK (seeders_est >= 0);
ALTER TABLE torrents ADD COLUMN scrape_failures integer NOT NULL DEFAULT 0
  CHECK (scrape_failures >= 0);
CREATE INDEX torrents_scrape_due ON torrents (last_scraped_at ASC NULLS FIRST)
  WHERE deleted_at IS NULL AND hidden_at IS NULL;
CREATE TABLE removed_keys (
  key            bytea PRIMARY KEY CHECK (octet_length(key) = 20),
  removed_at     timestamptz NOT NULL DEFAULT now(),
  removals       integer NOT NULL DEFAULT 1 CHECK (removals >= 1),
  sightings      integer NOT NULL DEFAULT 0 CHECK (sightings >= 0)
  -- **[LB-32 FIXED 2026-10-06: sightings column]** §4a needs a persistent
  -- per-key sighting counter for the 7d-cooldown strong-evidence rule
  -- (≥3 distinct /24s//48s). `SourceTracker` cannot span the cooldown
  -- (200k keys x 4 nets, cleared on dedup rotation). Without this column
  -- (or a sibling table) the ≥3-nets rule has nowhere to live.
);
CREATE INDEX removed_keys_age ON removed_keys (removed_at);
-- Grants (same style as 03): crawler needs UPDATE(last_scraped_at,
-- seeders_est, scrape_failures) on torrents and full SELECT/INSERT/UPDATE/DELETE
-- on removed_keys; SELECT on torrents is already full-table
-- (03_grants.sql:31), only INSERT (:32-34) / UPDATE (:35-38) are column
-- allow-lists **[FIXED 2026-10-06: SELECT nuance]** prior "SELECT+UPDATE(...)"
-- implied SELECT was also column-listed. Without extending the UPDATE
-- allow-list writes fail. Extend 03's UPDATE allow-list in this migration.
```

New `Store` methods (all bound-parameter, role grants extended in the same
migration style as `03`):

- `claim_scrape_due(limit, live_interval, unknown_interval)`: rows with
  `deleted_at IS NULL AND hidden_at IS NULL` (don't waste lookups on
  already-invisible rows) and
  `last_scraped_at IS NULL
    OR (seeders_est IS NULL AND last_scraped_at IS NOT NULL
        AND last_scraped_at < now() - unknown_interval)
    OR (seeders_est IS NOT NULL
        AND last_scraped_at < now() - live_interval)`,
  **[LB-27 FIXED 2026-10-06: OR-swallow]** Prior `OR last_scraped_at <
  now() - live_interval` (bare, no `seeders_est` gate) matched UNKNOWN rows
  too, so an unaware-10d-ago row was due every 7d and the 30d UNKNOWN
  interval never applied — wiping win-4 exactly like LB-24 did. Proven in
  `/tmp/bep33_review_20261006.py` §A (`buggy=True/fixed=False` on
  unaware-10d-ago). The live clause must be gated on
  `seeders_est IS NOT NULL`; the middle clause already gates unaware on
  `IS NULL AND IS NOT NULL` (never-scraped stays `last IS NULL`, always
  due).
  `ORDER BY last_scraped_at ASC NULLS FIRST LIMIT n
  FOR UPDATE SKIP LOCKED`.
  **[LB-24 FIXED 2026-10-06: single-interval defeats win 4]** Prior
  `last_scraped_at < now() - interval` (one knob) re-polls UNKNOWN rows every
  7d, wiping the §4b/§12 30d saving (38.3% at 50% unaware,
  `/tmp/bep33_final_audit.py` §9). `seeders_est IS NULL AND last_scraped_at IS
  NOT NULL` = scraped-but-still-NULL = unaware (never-scraped is
  `last_scraped_at IS NULL`, always due). Bind both intervals via
  `make_interval(secs=>$1/$2)` like `CLAIM_SQL`/`FAIL_SQL`. Validation must
  assert `unknown_interval >= live_interval` (§6), else UNKNOWN is polled
  faster than live. **[LB-6 FIXED]** Prior
  `ON torrents (last_scraped_at NULLS FIRST)` was a full index with no
  direction and no predicate; it neither matches `pending_ready …
  WHERE NOT gave_up` (`schema.sql:103` partial-index pattern) nor serves the
  query's visibility filter (planner index-scans hidden/deleted rows too).
  Fixed above to partial + explicit `ASC`. **[LB-7 FIXED]** Claim filter must
  also add `AND NOT dc3_key_denied(dht_key, info_hash_v1, info_hash_v2)`
  (3-arg signature at `schema.sql:120-126`, not 1-arg) — `visible=false`
  includes denylisted (`types.rs:499-501`, `visible_sql!` at
  `types.rs:208-213`), else we waste lookups on denied rows. Must also take a
  lease: bare `SELECT … FOR UPDATE SKIP LOCKED` with no `lease_until` column
  lets two workers (`scrape_workers>1`) re-claim the same rows (vs
  `CLAIM_SQL`'s atomic `UPDATE … SET lease_until … WHERE dht_key IN (SELECT …
  FOR UPDATE SKIP LOCKED) RETURNING …`, `crawler.rs:70-81`). Fixed pattern:
  `UPDATE torrents SET last_scraped_at=now() WHERE id IN (SELECT id … FOR
  UPDATE SKIP LOCKED) RETURNING …`, bind interval via
  `make_interval(secs=>$1)` like `CLAIM_SQL`/`FAIL_SQL`. (NULLS FIRST matches
  the existing `pending_ready` partial-index pattern — now actually true.)
  **[FIXED 2026-10-06: crash-lease stall — accepted limitation]** Unlike
  `CLAIM_SQL`'s 120s `lease_until` (`fetch.rs:39`), this stamp is also the
  next-due clock, so a worker crash between claim and `record_scrape` stalls
  those rows until the 7d `scrape_interval` expires (vs 120s for fetch).
  Proof: `/tmp/bep33_audit_fix2.py` (`64 rows × 7d stall`). Accepted for v1
  (crashes are rare, stall is bounded, next claim self-heals); do NOT add a
  separate `scrape_lease_until` column unless crash-stall measures as a
  problem. Document here so a reviewer does not mistake the stamp for a
  fetch-style short lease.
- `record_scrape(id, seeders_est, failures, when)`: stats-only write — must
  NOT call `nextval('change_seq')`, so the indexer sees no churn (mirrors
  the popularity-observation rule, §1.3, proven by `proof_log2.py`).
  **[LB-11 verified correct]** Must still be one atomic `UPDATE SET
  seeders_est, scrape_failures, last_scraped_at` with no `nextval` and no
  `files` touch (avoids `torrents_files_shape` trigger `schema.sql:87-89`,
  which fires only on `UPDATE OF files`).
- `tombstone_dead(id)`: wipe `name`/`files` (bulk freed), set `deleted_at`,
  bump `change_seq` — same mechanics as `deny` (`TOMBSTONE_SQL`,
  `crawler.rs:129-135`) but **without** a denylist insert.
  **[LB-9 FIXED]** Must take `lock_change_shared + lock_keys` like
  `deny_in_tx` (`crawler.rs:700-701`) or HWM races (`indexer.rs:60-87`);
  must set `files_truncated=false` to mirror the tombstone invariant
  (`schema.sql:45-47` only checks `name='' AND files='[]'`, but consistency
  matters for readers); must be conditional (see LB-13):
  `UPDATE … SET name='', files='[]'::jsonb, files_truncated=false,
  deleted_at=coalesce(deleted_at,now()), change_seq=nextval … WHERE id=$1
  AND deleted_at IS NULL AND last_seen_at=$old AND change_seq=$old`.
- `purge_tombstoned(grace)`: `DELETE FROM torrents WHERE deleted_at <
  now() - grace AND NOT dc3_key_denied(dht_key, info_hash_v1, info_hash_v2)`
  — **[LB-10 FIXED: 3-arg guard]** prior `NOT dc3_key_denied(…)` with one
  arg does not exist; signature is `(p_dht_key, p_v1, p_v2)` at
  `schema.sql:120-126` (prefix-compared at `:125`). The denylist guard is
  load-bearing: never purge `deny` tombstones, they are the block record.
  Grace e.g. 1h so the indexer (poll 1s default, HWM 200ms) observably drops
  the doc first via `visible=false`. Also needs column/table grants (above).
- `note_removal(key)` / `removal_cooldown_remaining(key)`: upsert/check
  `removed_keys` for §4a. Correct, subject to LB-16 placement (batch in
  `flush()`, not per-event sync) and LB-17 reset rule.
- `pending.seeders_est integer NULL CHECK (seeders_est >= 0)` + partial index (only in the §8 follow-up, not here;
  mirror the `torrents.seeders_est ... CHECK (seeders_est >= 0)` constraint so a negative estimate can never order the queue):
  **[LB-12 FIXED: composite]** `CREATE INDEX pending_seeders ON
  pending (seeders_est DESC NULLS LAST, next_attempt_at) WHERE NOT gave_up`
  — prior single-column `(seeders_est DESC NULLS LAST)` cannot serve
  `ORDER BY seeders_est DESC NULLS LAST, next_attempt_at`. Without it,
  reordering `claim()` over ~5M rows (`max_pending=5M`) seq-scans per fetch
  (`CLAIM_SQL` today uses `pending_ready(next_attempt_at) WHERE NOT gave_up`,
  `schema.sql:103`). Call this out in review: the index is part of that
  change. Prove with `EXPLAIN` in §9.

**[LB-8 FIXED]** Prior `ORDER BY last_scraped_at NULLS FIRST, last_scraped_at
ASC` duplicates the key; fixed to single `ORDER BY last_scraped_at ASC NULLS
FIRST`.

## 4. Scrape worker (binary, `crawl` role, 1 worker default)

Loop:

1. `claim_scrape_due(batch)` (§3, LB-7 lease pattern). Skip rows invisible
   since claim (recheck `visible` — cheap, avoids wasted lookups on rows
   hidden/denied between claim and lookup).
2. `Dht::scrape(key)` per key (§2, `scrape_query_timeout_secs=10` per-RPC
   and `scrape_lookup_timeout_secs=60` overall — not the 4s/30s/15s
   lookup/fetch timeouts; see §2 fix for why a single 10s overall would be
   stricter, not more lenient).
3. Classify (estimator mapping per §0 FIX: `zeros==m → 0`, `zeros==0 →
   UNKNOWN/saturated`, else formula; saturated never counts as dead;
   **[LB-31 FIXED 2026-10-06: saturated-aware path]** a response WITH filter
   keys but `zeros==0` is aware-at-the-wire but UNKNOWN for classification
   — it takes the unaware path below (keep old est, failures unchanged),
   never the dead/live branches; raw formula has no value there (`log(0)`
   undefined, clamped range `(0.5,7805.7]` never `≤0` —
   `/tmp/bep33_review_20261006.py` §F):
   - **Aware + est > threshold** → `record_scrape(est, failures=0)` (success
     resets; LB-14). Aware means ≥1 response with filter keys; est is the **seeds** estimate from OR-ing **all** aware `BFsd` across
     **both** families (§10 LB-23 OR-union + `/tmp/bep33_final_audit.py` §3), never
     per-family-max. Peers (`BFpe` union) is sanity/dedup only and is never
     stored — a `peers>0/seeds=0` swarm still tombstones after
     `max_scrape_failures` (conscious index-goal choice; proven safe-direction
     in `/tmp/bep33_final_audit.py` §1-2: raw ∈ [0.5,7805.7]).
   - **Aware + est ≤ threshold** → `record_scrape(est, failures+1)`; if
     `failures+1 >= max_scrape_failures` → conditional `tombstone_dead()` +
     `note_removal()` in one txn (purge happens later via `purge_tombstoned`,
     §3). Default `max=2`: 0→1 keep, 1→2 tombstone — 2 consecutive deads.
     **[FIXED 2026-10-06: death requires both families; TIGHTENED 2026-10-06]** A dead verdict is
     valid only if both v4 and v6 passes were actually attempted in this round
     (a node-list cache hit is a start set, never proof of no-route — prior
     parenthetical withdrawn); a v4-only zero plus skipped v6 must
     classify as UNKNOWN, not dead — else v6-live/v4-dead swarms are killed
     after 2 rounds (proven in `/tmp/bep33_audit_fix.py`: v4-zero-only
     tombstones a v6-live swarm falsely). Same constraint binds the §4b/§12
     win-2 early-exit and win-3 single-family-first shortcuts: **[LB-28 FIXED 2026-10-06: dead
     early-exit forbidden]** no dead early-exit exists — a 3-zero subset says
     dead while the full union says live (`/tmp/bep33_review_20261006.py`
     §B: early zeros=2048→0 vs full est 21.2→live), because DHT announces
     scatter over a dozen+ nodes and no 3 nodes hold the full swarm. Only a
     live early-exit (quorum agreeing nonzero) may stop early; a dead verdict
     always requires the full traversal over both families. See §4b/§12.
   - **Unaware-only** → `record_scrape(keep old est, failures unchanged,
     last_scraped_at=now())`. Unknown is not death; the timestamp only stops
     tight-looping. **[LB-14 clarified]** `dead,unknown,dead` tombstones
     after 2 spaced deads; unknown never resets — document as intended.
     Needs LB-23 unaware backoff so unknown rows do not cost a lookup every
     interval forever.
4. Sleep when nothing is due (same ticker pattern as admission's 1s loop,
   `admission.rs:629-631` — but that is just `ticker.tick()->flush`; scrape
   needs a semaphore + idle backoff, `IDLE_SLEEP_MIN/MAX` pattern at
   `fetch.rs:43-45`, not a literal copy — LB-15).

Removal is therefore **two-phase**: tombstone (indexer drops doc through the
existing `visible=false` path, no indexer changes) → purge after grace
(key becomes unknown → normal re-admission if still sampled). `deny`
tombstones are never purged (§3 guard).

**[LB-13 FIXED: revive race]** Pre-claim recheck is insufficient. `complete()`
clears `deleted_at` (`TORRENT_UPDATE_SQL`, `crawler.rs:119`) — revives
tombstones. Window: claim visible → 10–30s `scrape()` → concurrent
`complete()` updates row → worker `tombstone_dead()` wipes fresh fetch data.
Fix: conditional tombstone `WHERE id=$1 AND deleted_at IS NULL AND
last_seen_at=$old AND change_seq=$old` (snapshot at claim; `change_seq` moves
on every visible write, `last_seen_at` on every observe/complete) — else
`record_scrape` only. Test the race in §9.

**[LB-15 FIXED: batch/concurrency + atomicity]** `scrape_batch=64` with
`scrape_concurrency=3` × 10s lookups ≈ 213s/round; rows sit claimed with no
lease under the prior draft. With the LB-7 lease (`last_scraped_at=now()` at
claim) the window is bounded. `tombstone_dead()+note_removal()` must be one
txn or a crash leaves a tombstone without memory (re-fetch storm).

### 4a. Removal memory (accepted: simple variant)

`removed_keys` (§3) so deletion stays effective without a fetch per flap:

- On tombstone: upsert (`removed_at=now()`, `removals+1`) in the same txn as
  the tombstone (LB-15).
- Admission consults `removal_cooldown_remaining(key)`: skip while positive.
  Base cooldown **7d**, escalating 7 → 30 → 90d (cap) per consecutive
  removal. Hook point: **[LB-16 FIXED: placement]** `Admission::handle()`
  (`admission.rs:477-523`) runs per discovery (chan 65_536,
  `DISCOVERED_CHANNEL_CAPACITY:37` **[FIXED: line]** prior `:38` is the doc
  comment `/// Keys per dedup generation`; the const is at `:37`); a synchronous PK lookup per event kills
  throughput. Correct placement is batch-check in `flush()`
  (`admission.rs:539-597`) against `removed_keys` with an LRU negative cache.
  "Single indexed PK lookup" is true (`PRIMARY KEY(key)`), but per-event
  sync is infeasible — batch it.
- Strong evidence **shortens ÷4, never bypasses**: `announce_peer` with
  `seed=1`, or sightings from ≥3 distinct /24s (v4) / /48s (v6) — same
  network units as `source_network()` (`admission.rs:241-250`). Shorten so a
  lying announcer can't force a fetch loop. 7→1.75d, 30→7.5d, 90→22.5d.
  **[LB-17 FIXED: two gaps]** (a) `removals` never resets in the prior draft —
  resurgent torrents penalized forever. Fix: reset `removals` on successful
  fetch (`complete()` after cooldown expiry — the positive-liveness proof;
  the vaguer time-based alternative is dropped). (b) `announce_peer seed=1` has no wire: `Discovered`
  (`lib.rs:111-120`) has no seed flag and `PeerStore::announce()` takes no
  seed — both need extension (unmentioned). (c) `≥3 /24s//48s` cannot reuse
  `SourceTracker`: it holds only 200k keys × 4 nets (`admission.rs:44-47`,
  `GET_PEERS_MIN_SOURCES=2` at `:43`) and clears on dedup rotation
  (`:495-496,557-558`) — cannot span a 7d cooldown. Fix: **[LB-32 §3 schema]**
  persistent `removed_keys.sightings` (added §3, default 0) incremented by the
  batched `flush()` removal-check path, not `SourceTracker`.
- Cap ~1M rows LRU (delete oldest `removed_at` beyond cap in the same
  transaction as insert, or a periodic sweep — implementation choice;
  prefer periodic sweep: delete-oldest-in-insert-txn is heavy under flap).
  Add `dc3_removed_keys_count` gauge (missing in prior §7).
- NOT doing slim stub rows: no fetch-cost saving over this, and an
  absent-stub predicate would thread through every reader. Revisit only if
  admit-path lookups measure hot. Reasoned — accept.

### 4b. Efficiency fast-paths (wins 2, 4, 6)

These cut steady-state scrape cost without changing classification semantics;
full detail in §12, knobs in §6.

- **Announce short-circuit (win 6):** our own responder already records every
  `announce_peer` in `PeerStore::announce()` (`peer_store.rs:102`). When an
  announce for a known `torrents` row arrives with `seed=1` (post-§2 wire),
  `last_scraped_at` is refreshed and `scrape_failures` reset without a lookup:
  a live seeder just proved itself. Cost: one indexed `UPDATE` on an event we
  already handle — zero DHT RPCs. Seeder→peer transitions still need a scrape
  to observe death, so this only defers, never skips, the next due scrape.
  **[LB-32 FIXED 2026-10-06: plumbing]** The responder (`dc3-dht`,
  no PG handle) cannot `UPDATE torrents` itself — wire an explicit path
  (e.g. responder emits a `SeedAnnounce(key)` event on the existing
  discovery/observation channel into the crawl-role worker, which performs
  the indexed `UPDATE` batched with `record_scrape` writes). Without this
  the win is a dangling updater with no caller.
- **Adaptive intervals (win 4):** UNKNOWN rows (unaware-only, LB-23) re-scrape
  on `scrape_unknown_interval_secs` (30d default), not the 7d live interval.
  Proof `eff_calc.py`: at 50% unaware this saves 38.3% of scrape load; the
  rows are kept, just polled slower. Reset to 7d on the first aware response.
- **Live-only early exit + node cache (win 2):** stop the traversal once a quorum
  (`scrape_early_exit_quorum=3`) of aware responses agree NONZERO (live);
  fall back to the full traversal on any zero or disagreement.
  **[LB-28 FIXED 2026-10-06: dead early-exit forbidden — prior quorum-spans-families rule withdrawn]**
  Prior "3 agreeing zeros (spanning families) may stop early" is unsound and
  contradicts the §4b header ("without changing classification semantics"):
  DHT announces scatter over a dozen+ nodes (BEP rationale), so any 3-node
  subset union undercounts the full union. Proven in
  `/tmp/bep33_review_20261006.py` §B (3-empty → est 0 dead vs full → est 21.2
  live). A 3×nonzero quorum proves live (one live proof suffices — safe to
  stop and store the partial-union lower bound); a 3×zero quorum proves
  nothing about unseen nodes and must NEVER stop early — death always needs
  the full both-families traversal (§4 step 3). Reuse the §2 node-list cache as the start set so cached
  scrapes cost ~8–12 RPCs, not ~48. Proof `eff_calc.py`: 1M torrents at
  12 RPCs/scrape = 19.8 pps (fits 25/s); at 48 = 79.4 pps (does not).

## 5. Responder work (same project, do together)

§2's answer-half: parse/store `seed` (default peer when missing/`≠1`, §0),
per-infohash IP sets, emit filters on `scrape=1` (only when entries exist,
§0; IP-only insertion), trim values/nodes to fit 1024B (existing
`krpc::encode` trim order already drops values first — verify filter bytes
are accounted in the trim budget, else 512B of filters + 100 values
overflows). **[LB-18 FIXED: load-bearing]** `trim()` order
values→samples→nodes6→nodes (`krpc.rs:522-535`) with `≈88v4/28v6`
(`responder.rs:522-532`) never trims `BFsd`/`BFpe`: small replies fit
(512+overhead=532<1024 — **[LB-29 FIXED 2026-10-06: 524→532]** prior 524
understated by 8 B; exact bencode is `4:BFsd256:<256B>` = 2+4+4+256 = 266 B
per filter, 532 B for both, proven in `/tmp/bep33_review_20261006.py` §D),
but full replies (100 values + 8+8 nodes + filters)
overflow and `encode` returns `TooLarge` (`krpc.rs:552-553`) — proven by
`proof_trim.py` + `/tmp/bep33_review_20261006.py` §D (≈1544 B). Worse, dropping `values` first destroys the
"values contained" sanity check (§0 MAY-level). Fix: add `BFsd`/`BFpe` to
`response_fields` (`krpc.rs:460-499`) + size accounting in
`value_entry_len`/`trim`, and for `scrape=1` **reserve 532 B headroom for
`BFsd`/`BFpe` up front (2×256 B payload + 20 B bencode keys/overhead,
`/tmp/bep33_final_audit.py` §6 + `/tmp/bep33_review_20261006.py` §D:
1012+532=1544 overflows) while keeping the
existing values-first trim order** **[LB-29 FIXED 2026-10-06: 512→524→532]**
prior "~512 B" omitted bencode overhead (`proof_trim.py` used 524 B as
`2*256+12`); exact is 532 B (`2*(2+4+4+256)`). Conclusion unchanged
(overflow → TooLarge) but the reserve constant must be 532, not 524.
**[FIXED 2026-10-06: trim-priority inversion]** Prior "trim nodes before
values (or reserve 512 B headroom)" listed a spec-inverted option first: BEP
33 says "returning less values and only returning the mandatory node lists",
i.e. values are expendable, node lists are mandatory — nodes-first
contradicts the spec and also costs traversals (requesters need nodes to
continue the lookup). Proof: `/tmp/bep33_audit_fix2.py`. Unit-test filter
bytes against the BEP pseudocode (`proof_bloom.py` oracle + BEP test vector).
This also fixes our "unaware node" status for others' scrapers.

## 6. Config (new keys under `[crawl]`, u64 durations per §1.4)

| Key | Default | Notes |
|---|---|---|
| `scrape_workers` | 1 | dedicated threads |
| `scrape_interval_secs` | 604800 (7d) | min age before re-scrape |
| `scrape_batch` | 64 | rows per claim round |
| `max_scrape_failures` | 2 | consecutive dead scrapes before tombstone |
| `scrape_seeder_threshold` | 0 | tombstone when est ≤ threshold (with §0 empty→0 / saturated→UNKNOWN mapping; raw formula never returns ≤0 — `/tmp/bep33_audit_fix.py`) |
| `scrape_query_timeout_secs` | 10 | per-RPC timeout per BEP guidance (replaces 4s `query_timeout`; the "more lenient" direction). Was `scrape_timeout_secs` — **[FIXED 2026-10-06: split]** a single 10s overall is stricter than fetch 15s / lookup 30s, the opposite of lenient (`/tmp/bep33_audit_fix2.py`) |
| `scrape_lookup_timeout_secs` | 60 | overall scrape deadline (replaces 30s `lookup_timeout` / 15s fetch `GET_PEERS_TIMEOUT` with headroom for slower rounds) |
| `scrape_concurrency` | 3 | in-flight lookups per worker (BEP concurrent-RPC guidance) |
| `scrape_packets_per_sec` | 25 | dedicated scrape bucket (1s burst); see §2 |
| `removal_cooldown_days` | 7 | base; ×~4 per repeat to 90d cap (cap is a constant, not a knob) |
| `tombstone_purge_hours` | 1 | grace before hard purge |
| `scrape_sweep_secs` | 3600 | **[LB-19 FIXED: missing]** interval for `purge_tombstoned` + `removed_keys` LRU sweep; prior draft had no knob |
| `scrape_unknown_interval_secs` | 2592000 (30d) | **(win 4)** re-scrape interval for UNKNOWN rows; 38.3% load saving at 50% unaware (`eff_calc.py`) |
| `scrape_node_cache_keys` | 4096 | **(win 2)** LRU size of the §2 node-list cache reused as scrape start set |
| `scrape_early_exit_quorum` | 3 | **(win 2)** agreeing NONZERO (live) responses that end the traversal early (LB-28 live-only; dead quorum NEVER exits early) |

Validation ranges + `check-config` coverage follow the existing
`check_range` pattern (`config.rs:601-613`, e.g. `crawl.max_pending :718`,
`index.batch_size 1..=MAX_INDEX_BATCH :733`). Assert
`scrape_unknown_interval_secs >= scrape_interval_secs` (else UNKNOWN polled
faster than live, inverting win 4 — **[LB-24 FIXED 2026-10-06]**) and
`scrape_lookup_timeout_secs > scrape_query_timeout_secs` (per-RPC vs overall
split, §2). Suggested ranges:
workers 1–64, batch 1–1000, failures 1–10, threshold 0–1000,
query-timeout 1–60s, lookup-timeout 10–300s (must exceed query-timeout —
assert in validation),
concurrency 1–16, packets 1–250, cooldown 1–365d, purge 0–168h,
unknown-interval 7–90d, node-cache 0–65536, quorum 2–5.
u64 `*_secs`/`*_ms` follows `index.poll_interval_ms` (`config.rs:226`) —
correct. **[FIXED: duplicated paragraph]** Prior draft repeated this
paragraph twice, the second copy truncated (missing unknown-interval,
node-cache, quorum). Merged to one.

**[LB-19 FIXED: capacity math]** Ranges are sensible in isolation (25/s = 10%
of 250/s crawl; 7d interval; 1h grace vs indexer poll 1s + HWM 200ms
`store/lib.rs:147`), but defaults do not keep up. Proof
`proof_capacity.py`: `N/7d` scrapes/s = 0.17 (100k), 1.65 (1M), 8.27 (5M);
at α3×8 rounds×2 families ≈ 48 RPCs/scrape without node cache → 7.9 / 79 /
397 pps. 1M torrents already need ~79 pps > 25/s. Fix: sizing note +
`scrape_workers×concurrency` guidance and the §2 node-list cache; without the
cache, raise `scrape_packets_per_sec` or lower expectations. `check-config`
prints both budgets (250/s crawl + scrape rate).

## 7. Display, ranking, metrics (accepted; after §2–§4)

**Index-field decision (verified constraint):** `seeders_est` lives in PG;
Tantivy docs carry only `SIZE`/`SEEN`/`FILE_COUNT` fast fields (§1.3), and
every scrape updating an index field would bump `change_seq` → full reindex
churn per scrape interval (proven unnecessary by `proof_log2.py`: popularity
bumps already avoid churn unless `floor(log2)` moves). So:

- **Display + sort**: show `seeders_est` + `last_scraped_at` on detail/API;
  hide past the freshness window (`last_scraped_at` older than
  `scrape_interval`). **[LB-20 FIXED: overstatement]** Prior "fields exist in
  handlers: `torrent.rs:65-80`, `api.rs:30-57`" is wrong — those files show
  only `seen_count`; new `seeders_est`/`last_scraped_at` must be added to
  `TorrentRecord`/`IndexRow`, handlers, and templates. `sort=seeders` is a
  **web-side top-N resort** after hydration (approximate across pages —
  documented, not index-exact; feasible: search page → `get_many` ≤1000).
  Requires touching `Sort::parse` (`query.rs:76-84`), `SORT_CHOICES` + error
  string (`search.rs:39-44,58`) — the triple holds; a fifth sort needs all
  three (§1.3). Display + metrics can land with the worker.
- **Ranking boost**: `score × popularity_multiplier(seen)` today
  (`index.rs:702-712`); proposed `× (1 + SEEDER_WEIGHT·log10(1+seeders_est))`
  with `SEEDER_WEIGHT ≈ 0.15` mirroring `POPULARITY_WEIGHT`, applied
  **web-side after hydration, fresh estimates only** (same approximation
  caveat as sort). Stale rows rank exactly as today. `SEEDER_WEIGHT≈0.15` is
  arbitrary but consistent (sublinear curve proven in `proof_trim.py` tail +
  `/tmp/opencode/bep33proof/proof_sort.py` **[FIXED 2026-10-06: cited]** prior
  orphan with false "Cited conceptually" docstring and zero `grep` hits —
  now cited here and in §11; content is the 4-point weight-mirror subset of
  the `proof_trim.py` tail, independently re-proven in
  `/tmp/bep33_final_audit.py` §12);
  land after display. Saturation overcounts (§0 FP proof), so boosting on a
  saturated estimate is safe direction (popular stays popular).
- **Metrics** (new, worker-emitted): `dc3_scrape_zero_seeder_share`,
  `dc3_scrape_unaware_share` (gauges with div-zero guard), plus per-run
  counters (`dc3_scrape_total{outcome}` with kept/tombstoned/unaware). Follows
  the existing `gauge!`/`counter!` patterns (`index.rs:36-38`,
  `admission.rs:73-76`). **[LB-20 FIXED: missing]** Also add
  `dc3_removed_keys_count` (gauge), `dc3_purge_tombstoned_total` (counter),
  `dc3_scrape_due_depth` (gauge for `claim_scrape_due` backlog).

## 8. Liveness-ordered fetch queue (accepted; after §2)

- `pending.seeders_est integer NULL CHECK (seeders_est >= 0)` + partial index (§3 composite SQL).
- Fetch `get_peers` lookups (already performed at `fetch.rs:602` with
  `GET_PEERS_TIMEOUT=15s`) send `scrape=1`; record estimates when filters
  come back for free. **(win 1 — piggyback is the cheapest scrape in the
  plan: ~48 RPCs saved per pending key, 100% of that key's scrape RPCs,
  because the traversal was already paid for by the fetch.)** Needs the §2
  sibling API (can't reuse the `Vec<SocketAddr>` return; update the test
  fake at `fetch.rs:1126` too).
- `CLAIM_SQL` gains `ORDER BY seeders_est DESC NULLS LAST, next_attempt_at`
  — or NULLS handling per review; dead keys sink into existing backoff.
  **[LB-21 FIXED: chicken-egg + NULLS]** `pending.seeders_est` can only be
  set after a fetch attempt, but `complete()` deletes the pending row
  (`crawler.rs:366-371`) — first attempts are always NULL; ordering only
  helps retries. Specify backfill from `torrents` on re-queue (purge→re-admit
  path carries `seeders_est` via `observe` or admit-time join), or the
  ordering is a no-op for new keys. `NULLS LAST` (unscraped sinks in fetch)
  vs scrape queue `NULLS FIRST` (unchecked first in §3) is intentional and
  now justified: fetch prefers known-live; scrape prefers unknown. Index
  must be the LB-12 composite or the 5M-row sort seq-scans. Piggyback
  unaware (no filters → leave NULL, never write 0 — 0 means measured dead).
- NOT doing scrape-before-fetch gating (a full traversal per pending key
  ≈ doubles query load for keys that fail fast anyway — roughly 2 traversals
  vs 1, correct estimate). Revisit only if fetch-worker TCP time, not DHT
  budget, measures as the constraint.
- **Size-ordered reclamation (win 5):** `purge_tombstoned` deletes grace-expired
  rows in `total_size DESC` order under the sweep cap, so each sweep frees the
  most disk first. Proof `eff_calc.py`: under a 90/10 pareto size distribution
  the first 10% of purged rows free ~9× the bytes of size-blind order. Small
  dead rows wait longer for purge but cost little disk; tombstone (index-drop)
  is unaffected — only hard-delete order changes, so recall behaviour is
  identical.

## 9. Tests

- `dc3-dht`: filter build/estimate vs BEP pseudocode vectors (BEP test vector
  + `proof_bloom.py` oracle: 256 B, `k=2,m=2048`, estimator inversion,
  clamp/saturation edges, OR-union monotonicity; plus `/tmp/bep33_audit_fix.py`
  mapping: `zeros==m → 0`, `zeros==0 → UNKNOWN`, raw range `[0.5, 7805.7]`
  never `≤0` so threshold 0 depends on the empty→0 rule); responder emits 256B
  parseable filters within the 1024B trim budget (LB-18: assert no `TooLarge`
  at 100 values + nodes + filters; assert values-trimmed-first with 532 B
  headroom (2x256 B + 20 B bencode overhead = 532 B — [LB-29 FIXED 2026-10-06: 512→524→532]; exact entry `4:BFsd256:<256B>` = 266 B each, proven `/tmp/bep33_review_20261006.py` §D)
  reserved on scrape — never nodes-first, per 2026-10-06 fix); reader ORs all aware `BFsd` → seeds-est and all aware `BFpe` → peers-sanity separately (LB-25: peers union never stored / never blocks tombstone); saturated-aware (zeros==0, keys present) takes the unaware path, never dead/live (LB-31); claim uses two intervals GATED (LB-24 + LB-27: unaware `seeders_est IS NULL AND last IS NOT NULL < now()-unknown`, live `seeders_est IS NOT NULL AND last < now()-live` — regression-test bare-OR: unaware-10d-ago NOT due on 30d, live rows due on 7d); node cache is a dedicated per-torrent LRU, never sampler state (LB-26); reader unions filters with the §0 dedup proviso; unaware
  classification; sampler-style skip-cooldown; `seed` round-trip +
  `target()` routing (LB-1: `GetPeers{scrape}` keeps `target()=Some(hash)`);
  per-host shared-spacing ≤1/s (per-IP v4, per-/64 v6 — not global) under concurrent scrape+crawl + `Throttled` metric
  (LB-3); dedicated-bucket-binds assertion (LB-2); v4/v6 OR-union (LB-23
  corrected: assert OR-ing all aware `BFsd` → seeds-est plus a separate `BFpe` peers-sanity union (LB-25) across families then
  estimating once — idempotent on repeat filters, per-IP counting for
  distinct v4/v6; assert *not* estimate-max, which undercounts); both-families
  death rule (§4 FIX: v4-zero-only with v6 untried classifies UNKNOWN, never
  dead — regression-test the v6-live/v4-dead kill from
  `/tmp/bep33_audit_fix.py`); per-RPC vs overall timeouts (§2 FIX: 10s
  per-RPC, 60s overall); live-only early exit (LB-28: 3-nonzero may stop; 3-zero NEVER stops — assert subset-zero vs full-live from `/tmp/bep33_review_20261006.py` §B).
- `dc3-store` (needs `DATABASE_URL`, like existing tests): claim ordering
  NULLS FIRST → stalest; lease prevents double-claim with `scrape_workers=2`
  (`SKIP LOCKED` concurrency, LB-7); stats-only write leaves `change_seq`
  untouched; tombstone bumps it + takes locks + sets `files_truncated=false`
  (LB-9); conditional tombstone loses to concurrent `complete()` (LB-13
  race); `purge_tombstoned` never touches denylisted rows (3-arg guard,
  LB-10) and respects grace (indexer observes `visible=false` first);
  `scrape_failures` increment/reset; `removed_keys` cooldown/escalation/reset
  (LB-17); grants on new columns/tables (LB-10); `EXPLAIN` for new indexes
  (LB-6/LB-12 composite serves the query).
- Binary harness (`seeder.rs` pattern): full swarm kept, dead swarm
  tombstoned after N failures then purged, unaware-only swarm kept with
  backoff (§10 LB-23 unaware-backoff), removed key re-admitted through the pipeline, cooldown
  suppresses immediate re-fetch (÷4 shortens, never bypasses).
- End-to-end: seed → scrape → tombstone → purge → re-announce → re-fetch.
- Load: scrape traffic stays within its dedicated bucket (assert via
  counters — assert the scrape bucket, not the shared one, binds under
  load); crawl traffic unaffected except documented `max_send_wait` sharing
  (LB-3); capacity note (LB-19: 1M/7d needs ~79 pps at 48 RPCs/scrape vs 25/s
  default — test with node cache on/off); config `check_range` +
  `check-config` prints both budgets.
- Flap storm: ÷4 shortens, never bypasses; 1M LRU sweep keeps count bounded.
- Efficiency regressions (§12): piggybacked fetch lookups carry `scrape=1`
  and populate `pending.seeders_est` with zero added RPCs; early exit fires on
  3-agreeing NONZERO (live) responses (LB-28 live-only; dead quorum NEVER exits early) and falls back
  to full traversal on any zero or disagreement;
  single-family-first tries v4 first and skips v6 only on proven-live (death
  always requires both families — assert the v6-live case is not tombstoned); UNKNOWN rows sleep 30d; announce with `seed=1` refreshes
  `last_scraped_at` with no lookup; purge sweep deletes `total_size DESC`;
  `record_scrape` batches stay stats-only (no `change_seq` bump — assert feed
  position unchanged after a 1k-row scrape batch).

## 10. Risks (expanded)

- **Recall loss**: only polled peers count; healthy torrents whose seeders we
  missed look dead. Consecutive-failure requirement + threshold 0 mitigate.
  Saturation overcounts (§0 FP proof: 6000→0.994 FP), so the failure mode near
  the threshold is keeping dead rows, not killing live ones — safe direction.
  **[LB-30 FIXED 2026-10-06: direction]** Truncation (100-cap store) UNDERCOUNTS viral swarms; only true filter saturation overcounts. Threshold 0 stays safe for a different reason, proven in `/tmp/bep33_review_20261006.py` §E: a truncated live swarm still estimates `>0` (live stays live), and a saturated filter maps to UNKNOWN, never `0` — neither path manufactures a false dead.
  Near-zero FP is low for small swarms (`proof_bloom.py`: n=100→0.009).
- **Estimate fuzz near zero**: Bloom FPs; threshold 0 + consecutive
  failures; never act on a single scrape. Empty (`zeros==m`) maps to `0`,
  saturated (`zeros==0`) maps to UNKNOWN/saturated (§0 FIX — the raw formula's
  `0.5` clamp artifact must never reach classification), never estimated.
- **Flapping cost**: bounded by §4a to ~1 fetch per cooldown window (7→30→90d,
  ÷4 on strong evidence: 1.75/7.5/22.5d).
- **Unaware pockets**: non-compliant clusters near a torrent yield UNKNOWN
  forever — accepted (row kept, rescraped next interval). **[LB-23 FIXED: unaware-backoff]**
  Prior "rescraped next interval" implies a lookup every 7d forever — a
  permanent tax. Fix: unaware backoff (e.g. 7d→30d for consecutive UNKNOWN,
  capped, reset on first aware response).
- **v4/v6 double counting**: same host reachable via both families yields two
  distinct IPs (v4 + v6), i.e. two `<Infohash,IP>` tuples per the spec's
  uniqueness rule — not one. **[LB-23 CORRECTED: OR-union is spec-compliant]**
  Prior "per-family estimate-max (not union-then-estimate, which
  double-counts dual-stack)" is **wrong and is withdrawn**: (a) it contradicts
  the spec, which mandates "estimate the size of a union … simply by ORing
  the bits of each filter" (§0); (b) OR-union is idempotent — the same filter
  returned twice (same node via both families) contributes once, so there is
  no double-count of identical sets; (c) distinct v4/v6 IPs *should* count
  twice per the `<Infohash,IP>` model — `max` would undercount real
  v4-distinct + v6-distinct populations (unsafe: risks killing live torrents),
  while OR at most overcounts per-IP (safe at threshold 0, since saturation
  already overcounts — §0 FP proof). Fix: define responder filter coverage
  (one filter over both families vs per-family `BFsd/BFpe` pairs — either way
  the reader ORs *all* aware `BFsd` together and all aware `BFpe` together,
  then estimates once per union) with documented residual: one dual-stack
  host counts as 2 (spec-correct per-IP counting). Consistent with the per-IP
  `PeerStore` model. Proof: `/tmp/bep33_check_vectors.py` (OR-union
  monotonicity `zeros_union ≤ zeros_part ⇒ est_union ≥ est_part`) and
  `/tmp/bep33_check_math.py` (FP/saturation).
- **Tombstone purge safety**: the `NOT dc3_key_denied(3-arg)` guard + grace
  period are both load-bearing; a bug here deletes block records. Test both
  (LB-10). `complete()` revive (§1.2) makes the conditional tombstone (LB-13)
  load-bearing too.
- **Bulk `removed_keys` growth**: 1M LRU cap + periodic sweep; monitor
  `dc3_removed_keys_count`.
- **Load**: one extra lookup per torrent per interval against the dedicated
  §2 bucket (default 25/s); crawl traffic untouched except `max_send_wait`
  sharing (LB-3). `check-config` prints both budgets. **[LB-19]** Defaults do
  not sustain 1M torrents at full-traversal cost (~79 pps — `proof_capacity.py`);
  node-list cache or higher bucket/workers required; document sizing.
- **Efficiency skews (§12):** live-only early exit on 3-agreeing NONZERO responses can miss a
  minority aware response that disagrees — bounded by falling back to the full
  traversal on any zero or disagreement (LB-28: a dead quorum NEVER exits early, so no
  short scrape ever manufactures death) and by the consecutive-failure rule (one
  short scrape never tombstones). Single-family-first skips v6 only on
  proven-live (§12 FIX); death always requires both families, so dual-stack
  swarms reachable only over v6 are never killed by a v4-only shortcut —
  the prior "bounded by the consecutive-failure rule alone" was insufficient
  because a systematically skipped family fails identically every round
  (`/tmp/bep33_audit_fix.py`). Size-ordered purge delays hard-delete of small
  dead rows (disk cost, not recall: the index drop happens at tombstone).
  Announce short-circuit only defers scrapes on positive liveness proof, never
  manufactures death.
- **What the double-check corrected** (earlier draft said): `pending` has 8
  columns (had omitted `last_attempt_at`); dedup entries are salted
  fingerprints with size- *and* time-rotation (not "keys"); responder budget
  units are replies/s + bytes/s; per-address spacing is per-IP (v4) /
  per-/64 (v6) — distinct from the inbound /48 map; fetch timeouts are code
  constants, not `[crawl]` keys (new knobs use u64 `*_secs`/`*_ms`); `complete()`
  revives tombstones (drives the two-phase design); sorts are exactly four (a
  fifth needs the `Sort::parse` + `SORT_CHOICES` + error-string triple).
- **What this pass corrected** (§0 modality/omissions: conditional filters,
  default-peer, dedup proviso, tracker-first, MAY-vs-MUST, BEP-32 citation,
  concurrent-RPC wording, exact 6000; §1 citations: fetch `:126-128`,
  `complete()` lowest-id not v1-then-v2, `:422-436`, dedup `:554-556`,
  `lib.rs:298-304` lock call, `types.rs:208-213` visible SQL,
  `responder.rs:522-532` counts with 28-vs-29 note, `dc3-dht/src/config.rs`
  paths, /48-vs-/64 nuance; §2–§10 LB-1…LB-23 as marked; **this review
  (2026-10-06) additionally fixed**: §6 duplicated validation paragraph
  (merged, second copy was truncated); `SendError at node.rs:62-72` →
  `QueryError at node.rs:69-84` (wrong enum name + lines pointing at
  constants); `DISCOVERED_CHANNEL_CAPACITY:38` → `:37` (const, not comment);
  `grep seed` → `grep -i seed` (`sampler.rs:546` is `SeedableRng`,
  case-insensitive-only); `509-561` → `501-561` (include `value_entry_len`);
  batch `min(1000,1000)` tautology → `min(batch_size,MAX_FEED_PAGE)` + 64MiB;
  proof-scripts path (`eff_calc.py` in `/tmp/opencode/`, new oracles in
  `/tmp/`); **LB-23 load-bearing reversal**: `estimate-max` withdrawn,
  OR-union restored as spec-mandated and safe-direction (max undercounts
  distinct v4+v6, risking live-kill; OR is idempotent and per-IP-correct);
   **[LB-24]** single-interval claim re-polled UNKNOWN every 7d, wiping win-4
   38.3% saving — fixed to two-interval claim (`live_interval` /
   `unknown_interval`, `seeders_est IS NULL AND last_scraped_at IS NOT NULL`
   = unaware) with `unknown >= live` validation;
   **[LB-25]** "union filters" never said which union drives death (peers
   `>0`/seeds `0` ambiguous) — fixed: `BFsd` union = stored `seeders_est`,
   `BFpe` union = sanity/dedup only, never blocks tombstone;
   **[LB-26]** "or reuse sampler state" for the node cache is wrong (sampler
   tracks per-node sample state, not per-torrent closest-node lists) — fixed
   to dedicated per-torrent LRU only;
   **[FIXED 2026-10-06: 512→524→532]** trim headroom omitted bencode
   overhead (exact: 20 B keys + 512 B payload = 532 B, LB-29); **proof_sort.py** orphan (false "Cited conceptually" docstring,
   zero grep hits) now cited in §7/§11;
   **`/tmp/bep33_final_audit.py`** added as the 12-point independent re-proof
   oracle;
   **second review 2026-10-06 (LB-27…LB-32, proven in `/tmp/bep33_review_20261006.py`)**:
   **[LB-27]** bare-OR live clause re-polled UNKNOWN every 7d (win-4 wipe, same as LB-24) — gated to `seeders_est IS NOT NULL`;
   **[LB-28]** dead early-exit (even quorum-spans-families) is unsound — subset union undercounts full union — forbidden, live-only exit;
   **[LB-29]** 524 understated by 8 B — exact 532 B (`4:BFsd256:<256B>` = 266 B each);
   **[LB-30]** "saturation overcounts" conflated truncation — truncation UNDERCOUNTS, safety comes from truncated-est `>0` + saturated→UNKNOWN;
   **[LB-31]** saturated-aware (keys present, zeros==0) had no branch — takes the unaware path, never dead/live;
   **[LB-32]** `removed_keys.sightings` column added + responder→crawl `SeedAnnounce` event plumbing specified;
   **final review 2026-10-06 (proven in `/tmp/bep33_pass3_proof.py`, §11)**:
   PeerStore 6000-contradiction reworded to keep-100; six typo/self-contradiction
   strings eliminated (double-slash, literal backslash-u escape, stale-524
   reserve, leftover trim parenthetical, open LB-17 decision, CHECK-less
   shorthand); `LB-23` unaware/union halves disambiguated; §4 cross-ref names
   win-2 + win-3; §12 win-2 shortened with proof citation retained).

## 11. Proofs (executable; cited code in `/tmp/opencode/bep33proof/`, `/tmp/`, `/tmp/opencode/`)

All scripts were run 2026-10-06; outputs quoted are the actual runs.
Prior proofs re-ran clean on 2026-10-06 (§11 outputs below match); five
independent oracles were added in `/tmp/` to close gaps the original six left
(BEP byte-vector, formal math, SQL citations, final 12-point audit, plus the
second-review six-point oracle `/tmp/bep33_review_20261006.py`).

- `proof_backoff.py` — `FAIL_SQL` schedule. Output: `waits_sec: [300, 600,
  1200, 2400, 4800]`, `total_min: 155.0`, `give-up on 6th`. Proves §1.2
  "5,10,20,40,80 min; ~155 min ≈2.6h; gave_up on 6th". Correctness argument:
  `FAIL_SQL` (`crawler.rs:91-100`) computes `next = now + min(300·2^attempts,
  7d)` with pre-increment `attempts` (`attempts+1`, `gave_up = attempts+1 >=
  6`); attempts=0..4 give 300·1,2,4,8,16 = 300,600,1200,2400,4800s; the 6th
  failure (attempts 5→6) sets `gave_up`. Sum 9300s = 155min = 2.58h.
  Independently re-proven by `/tmp/bep33_check_math.py` (`backoff VERIFIED`)
  and `/tmp/bep33_check_sql.py` (`FAIL_SQL formula VERIFIED` — asserts the
  `power(2, least(attempts,30))` + `attempts+1 >= $4` strings exist).
- `proof_bloom.py` — BEP filter math (`m=2048,k=2`, SHA-1 `insertIP`,
  `len==256`, FP `(1-e^{-kn/m})^k`, estimator
  `log(c/m)/(k·log(1-1/m))`, OR-union). Output: `n=6000 fp≈0.9943`,
  `n=8000 fp≈0.9992` (cap sound); `empty→0.5` clamp artifact; `0
  zeros→refuses (log 0)` (saturated/unknown); `n=100→est 100.0`,
  `n=1256→est 1255.7` (inversion); `256 inserts→est 257.9`; `union zeros
  1689 ≤ 1857 → est 197.3 ≥ 100.2` (monotone). Proves §0 size/saturation/
  estimator/union and the §2/§10 saturation-safe-direction argument.
  Derivation: FP follows the standard Bloom occupancy — each of `kn` hashes
  hits a given bit with prob `1/m`, so `P(bit=0) = (1-1/m)^{kn} ≈ e^{-kn/m}`,
  `P(bit=1) ≈ 1-e^{-kn/m}`, all `k` hits `≈ (1-e^{-kn/m})^k`. At `m=2048,k=2`:
  `n=6000 → (1-e^{-5.86})^2 ≈ 0.9943` (5.8 zeros left), `n=8000 → 0.9992`
  (0.8 zeros) — hence the 6000 cap is load-bearing. Estimator inverts
  `zeros = m·(1-1/m)^{kn} ≈ m·e^{-kn/m}` for `n`. Empty filter (`m` zeros)
  clamps to `m-1` → `log((m-1)/m)/(k·log(1-1/m)) = 1/k = 0.5` (algebra:
  numerator = denominator/k), so code must special-case empty (else every
  empty swarm reports 0.5 seeders). Saturated (0 zeros) → `log(0)` undefined →
  must report saturated/unknown, never 0.
- `/tmp/bep33_check_vectors.py` **(new, strongest)** — byte-exact BEP test
  vector from first principles. Implements `insertIP` verbatim
  (`sha1(ip)`, `index1 = hash[0]|hash[1]<<8`, `index2 = hash[2]|hash[3]<<8`,
  `%=2048`, set bits), inserts `192.0.2.0/24` (256 addrs) + `2001:DB8::`–
  `::3E7` (`0x000–0x3E7` inclusive = 1000 addrs; total 1256), compares the
  256 B hex to the spec's published hex. Output: `len: 256 match: True`,
  `VECTOR MATCHES SPEC`, `zeros=619 est=1224.9309 spec_expects=1224.9308
  diff=0.0001`. This proves (a) our reading of the pseudocode (byte order,
  modulo, bit numbering `bloom[i/8] |= 1<<(i%8)`) is exactly spec-conformant,
  (b) `m=256·8`, `k=2`, `len==256` are correct, (c) the estimator on the real
  filter yields the spec's `1224.9308` (0.0001 rounding), closing the loop
  `proof_bloom.py` left open (it tested inversion via expected-zeros formula,
  not the real filter's zero count). Any implementation passing this vector
  is interoperable by construction.
- `proof_capacity.py` — scrape load. Output: `100k→0.17/s, 1M→1.65/s,
  5M→8.27/s`; at 48 RPCs/scrape `7.9 / 79.4 / 396.8 pps`. Proves LB-4/LB-19:
  1M torrents exceed the 25/s default without a node-list cache. Derivation:
  `rate = N/604800` (one scrape per 7d); `100k/604800=0.1653`, `1M=1.6534`,
  `5M=8.2672`; ×48 (α3 × 8 rounds × 2 families upper bound, `lookup.rs:27-29`
  + `lib.rs:157-169` fanout) = 7.9/79.4/396.8. Independently re-proven by
  `/tmp/bep33_check_math.py` (`capacity: full 48 exceeds 25/s, cached 8/12
  fits VERIFIED` — also shows cached 8 RPCs → 13.2 pps and early-exit 12 →
  19.8 pps at 1M both fit 25/s).
- `proof_log2.py` — XOR trick. 200k correlated near-pairs (`b = a +
  randint(0,1e6)`) + power-of-two boundaries,
  `mismatches: 0`. **[FIXED 2026-10-06: description]** Prior "200k random
  pairs" overstated independence; the script's pairs are correlated
  (independent uniform 50k to 1e18 plus exhaustive `1..199 × 1..199` live in
  `/tmp/bep33_check_math.py`). Proves §1.3 `(a XOR b)<(a AND b) ⇔ same floor(log2)` and
  the §7 no-churn decision. Formal proof (added): let `h(x)` = highest set
  bit. If `h(a)=h(b)=p`, then both have bit `p`=1 and agree on all bits
  `>p` (none), so `a^b` has bit `p`=0 (hence `<2^p`) while `a&b` has bit
  `p`=1 (hence `≥2^p`), so `(a^b)<(a&b)`. Conversely if `h(a)≠h(b)`, let
  `p=max(h(a),h(b))`; exactly one has bit `p`=1, so both `a^b` and `a&b` have
  bit `p` = 1 and 0 respectively → `(a^b)≥2^p > (a&b)`. Hence the SQL
  `CASE WHEN (old # new) > (old & new) THEN nextval ELSE keep`
  (`crawler.rs:23-25,164-169`) fires exactly on `floor(log2)` change.
  Exhaustively verified `1..199 × 1..199` plus 50k fuzz to 1e18 in
  `/tmp/bep33_check_math.py` (`log2 trick VERIFIED`). Zero-edge:
  `(0 XOR 1) < (0 AND 1)` is `1 < 0` = false while `floor(log2)` is undefined
  at 0, so the trick misfires only on `seen_count = 0` — unreachable in
  practice (`seen_count DEFAULT 1` in both tables, `schema.sql:31,95`, and
  `complete()` inserts `max(pending_seen, 1)` at `crawler.rs:450`; `CHECK >= 0`
  permits 0 but no path writes it). Proven in `/tmp/bep33_final_audit.py` §7.
  If a 0 ever appears the worst case is one stale `change_seq` bump, not
  data loss.
- `proof_trim.py` — 1024 B budget. `88×8+8×26+100≈1012 ≤1024`,
  `28×21+8×38+100≈992 ≤1024`, `+524 B filters ≈1536 >1024` (524 as computed in that script; exact 532 B per LB-29, same `TooLarge` conclusion). Proves §1.5
  counts and LB-18 `TooLarge` risk. Sizes: v4 peer `6+2=8`, v6 `18+3=21`
  (`krpc.rs:501-507`); v4 node 26 B, v6 node 38 B (`COMPACT_NODE_*_LEN`);
  filters `2×256+12≈524` (keys + bencode overhead). Hence full replies
  (100 values + 8+8 nodes + filters) cannot fit — `encode` returns `TooLarge`
  (`krpc.rs:552-553`) unless `BFsd`/`BFpe` join the trim budget and
  nodes-trim-before-values on scrape (LB-18 fix). Tail prints popularity curve
  `0→1.0, 1→1.045, 10→1.156, 100→1.301, 1000→1.450`, proving §7 weight
  mirroring (`1+0.15·log10(1+seen)`) is sublinear/sane — doubling seen never
  doubles rank. Re-proven in `/tmp/bep33_check_math.py` (`trim budgets
  VERIFIED`) and `/tmp/bep33_final_audit.py` §6 (repo constants
  `COMPACT_PEER 6/18`, `COMPACT_NODE 26/38` from `compact.rs:13-19`).
- `/tmp/opencode/bep33proof/proof_sort.py` **[FIXED 2026-10-06: now cited]**
  — weight-mirror sanity (`POP=0.15`, `0→1.0, 1→1.045, 10→1.156, 100→1.301`).
  Output matches `proof_trim.py` tail (which adds `1000→1.450`). Prior
  docstring "Cited conceptually" was false (zero `grep -rn proof_sort` hits);
  now cited here + §7. Proves `SEEDER_WEIGHT≈0.15` sublinear/sane. If kept,
  fix its docstring to "Cited in bep33.md §7/§11"; else delete as redundant
  with `proof_trim.py` tail + `/tmp/bep33_final_audit.py` §12.
- `proof_peerstore_cap.py` — `20000×100×48 B ≈ 96 MB` vs
  `20000×6000×48 B ≈ 5.76 GB`. Proves LB-5: grow-to-6000 without token cap
  OOMs; keep-100 saturates (see bloom FPs). `MAX_KEYS=20000`
  (`peer_store.rs:28`), `MAX_PEERS_PER_KEY=100` (`:30`), ~48 B/entry
  (`SocketAddr` ≤28 B + `Instant` 8 B + `Vec` overhead) conservative — real
  RSS higher with `HashMap`/`Slot` overhead, so 5.76 GB is a lower bound.
  At 100-cap, `n=100→fp 0.0087` fine but `n=1000→0.39` already saturating:
  viral swarms (>100 stored) estimate as lower bounds — threshold 0 stays
  safe (saturation overcounts, never undercounts live to dead).
- `/tmp/opencode/eff_calc.py` — efficiency wins arithmetic. Output:
  `100k→0.17/s, 1M→1.65/s` scrapes/s; `full(48rpc): 7.9 / 79.4 pps` vs
  `cached(8rpc): 1.3 / 13.2 pps` vs `early-exit-avg(12rpc): 2.0 / 19.8 pps`
  (100k / 1M); piggyback saves ~48 RPCs per pending key; size-ordered purge
  frees 9× bytes per early row under pareto 90/10; 30d UNKNOWN backoff saves
  38.3% of load at 50% unaware. Proves §12: cached + early-exit fits the 25/s
  bucket where full traversal does not. Each sub-claim independently
  re-proven in `/tmp/bep33_check_math.py`: pareto (`0.9B/0.1N ÷ B/N = 9
  VERIFIED` — top decile holds 90% bytes ⇒ 9× bytes per early row),
  unaware (`0.5·(1-7/30)=38.3% VERIFIED` — half the rows polled 30d instead
  of 7d), capacity (`8/12 RPCs fit, 48 does not VERIFIED`), piggyback (fetch
  already pays the `get_peers` traversal at `fetch.rs:602` with
  `GET_PEERS_TIMEOUT=15s`; adding `scrape=1` adds 0 RPCs, only ~524 B × ~20
  responses ≈ 10 KB vs ~48×300 B ≈ 14 KB + RTTs — 100% of that key's scrape
  RPCs saved).
- `/tmp/bep33_check_sql.py` **(new)** — SQL/code citations. Parses
  `20260901000001_schema.sql:92-101` → 8 pending cols
  (`dht_key,discovered_at,seen_count,attempts,next_attempt_at,lease_until,last_attempt_at,gave_up`),
  `:15-48` → 17 torrents cols, asserts `denylist IN (20,32)` and
  `dc3_key_denied(p_dht_key,p_v1,p_v2)` 3-arg signature, asserts
  `power(2, least(attempts,30))` + `attempts+1 >= $4` (`FAIL_SQL`),
  `find(dht_key).or_else(first)` (lowest-id, not v1-then-v2), and
  `deleted_at = NULL` (revive). Output: `ALL SQL OK`. Closes the "trust the
  line numbers" gap for §1.2's most error-prone claims.
- `/tmp/bep33_audit_fix.py` **(new, 2026-10-06 audit)** — three load-bearing
  proofs. Output: `est min 0.5000, max 7805.70` (raw estimator over
  `zeros=1..2047` never `≤0`, so threshold 0 needs the §0 empty→0 mapping);
  `single-family-first v4-only tombstones a v6-live swarm falsely after 2
  rounds` (executable kill-demo driving the §4/§12 both-families fix);
  `10s-overall < 15s-fetch < 30s-lookup contradicts lenient` (driving the §2
  per-RPC 10s + overall 60s split); plus re-derivations `unaware 38.3%` and
  `pareto 9x`. Run: `python3 /tmp/bep33_audit_fix.py`.
- `/tmp/bep33_audit_fix2.py` **(new, 2026-10-06 audit)** — four secondary
  proofs. Output documents: trim-priority inversion (BEP "less values +
  mandatory nodes" ⇒ values-first, never nodes-first — §5 fix); spacing is
  per-host (per-IP v4, per-/64 v6), so §9 "shared-spacing ≤1/s" now reads
  per-host; crash-lease stall (`64 rows × 7d` vs fetch 120s — §3 accepted
  limitation); `proof_log2.py` pairs are correlated `b = a + rand` (§1.3/§11
  description fix). Run: `python3 /tmp/bep33_audit_fix2.py`.
- `/tmp/bep33_final_audit.py` **(new, 2026-10-06 audit)** — twelve independent
  re-proofs in one run (`ALL FINAL AUDIT OK`): §1 estimator raw ∈
  `[0.5,7805.7]` (min @z=2047, max @z=1, so threshold 0 needs empty→0);
  §2 saturated `log(0)` ⇒ UNKNOWN; §3 OR idempotence (`F OR F == F`) +
  monotonicity (`zeros_union ≤ min(parts)` ⇒ `est_union ≥ max(ests)`,
  closing LB-23); §4 FP `(1-e^{-kn/m})^k` at 100/1000/6000/8000; §5 capacity
  (48→79.4 over 25/s, 12→19.8 and 8→13.2 fit at 1M); §6 trim with repo
  constants (`compact.rs:13-19`: peer 6/18+2/+3, node 26/38; 1012/992 fit,
  +524→1536 overflows in that script, exact +532→1544 per LB-29); §7 log2 exhaustive 1..199 + 20k fuzz plus zero-edge
  (`(0^1)<(0&1)` false, `fl(0)` undefined ⇒ `seen_count≥1` invariant
  load-bearing via schema `DEFAULT 1` + `max(...,1)`); §8 backoff
  `[300,600,1200,2400,4800]` sum 9300s; §9 two-interval claim
  (unaware-10d-ago not-due on 30d but due on 7d; `0.5·(1-7/30)=38.3%`);
  §10 dual-stack per-IP counting; §11 timeout split (10>4, 60>30/15);
  §12 pareto 9× + peerstore 96 MB vs 5.76 GB. Run:
  `python3 /tmp/bep33_final_audit.py`.
- `/tmp/bep33_review_20261006.py` **(new, second review 2026-10-06)** — six
  load-bearing re-proofs in one run (`ALL REVIEW PROOFS OK`): §A two-interval
  OR-swallow (buggy claim `True` = due on 7d, fixed gated claim `False` on
  unaware-10d-ago with 30d UNKNOWN interval); §B dead early-exit unsoundness
  (3-empty subset → est 0 dead vs full union → est 21.2 live — subset says
  dead while the swarm is live, so dead exit is forbidden, live-only exit);
  §C estimator log-base invariance (`z=619 → 1224.9309` under ln/log2/log10 —
  the ratio cancels, so the spec is base-agnostic and any base interoperates);
  §D exact bencode overhead (one filter entry `4:BFsd256:<256B>` = 2+4+4+256
  = 266 B, two = 532 B; 1012+532=1544 and 992+532=1524 both overflow — doc 524
  understated by 8 B, same TooLarge fix); §E truncation undercounts while
  saturation overcounts (truncated 100/6000 → est 100.0 ≪ 6000; saturated
  8000 → 0.8 zeros, fp 0.9992 → UNKNOWN — threshold-0 safe because truncated
  live still estimates `>0` and saturated maps to UNKNOWN-never-`0`); §F
  saturated-aware path (raw range `[0.5, 7805.7]` never `≤0`, so a zeros==0
  filter has no numeric estimate and must take the unaware path). Run:
  `python3 /tmp/bep33_review_20261006.py`.
- `/tmp/bep33_pass3_proof.py` **(new, final review 2026-10-06)** — nine
  machine-checked assertions over the live document and tree (`ALL PASS3
  PROOFS OK`): §1 PeerStore contradiction absent + keep-100 stated; §2 six
  typo/self-contradiction strings absent (double-slash, literal backslash-u
  escape, stale-524 reserve, leftover parenthetical, open LB-17 decision, CHECK-less
  pending shorthand); §3 `QueryError` span 69–84 and `Send` at :79 verified
  against `node.rs`; §4 every `LB-23` mention disambiguated (unaware-backoff
  vs OR-union halves); §5 win-2/win-3 map + §4 cross-ref naming both; §6
  `CHECK (seeders_est >= 0)` present in all three places (§3 SQL, §3 pending
  bullet, §8); §7 bencode math recomputed (266 B/filter, 532 B both) + 532 B
  reserve stated; §8 single audit-trail header; §9 §12 win-2 shortening keeps
  the §B proof citation and the 8–12 RPC / 19.8 pps numbers. Run:
  `python3 /tmp/bep33_pass3_proof.py`.

## 12. Efficiency wins (accepted; implements §2/§4/§6/§8 within budget)

The 25/s scrape bucket cannot sustain full traversals at 1M torrents
(79.4 pps — `eff_calc.py`). These seven wins close the gap; none changes
classification semantics.

1. **Piggyback on fetch (§8):** every fetch `get_peers` at `fetch.rs:602`
   sends `scrape=1` once the §2 sibling API lands. Pending keys — the bulk of
   new scrapes — then cost 0 extra RPCs. Saves ~48 RPCs per pending key.
2. **Node-list cache + live-only early exit (§4b; LB-28):** the §2 node cache
   seeds each scrape; stop after 3 agreeing NONZERO (live) responses, else
   full traversal (dead quorum NEVER exits early — subset union undercounts
   the full union; proof `/tmp/bep33_review_20261006.py` §B, full argument in
   §4 step 3). 48 → ~8–12 RPCs/scrape (13.2–19.8 pps at
   1M — inside 25/s).
3. **Single-family-first:** try v4 first (assumed dominant family — operator
   MUST verify against `v4-vs-v6 aware-response` metrics post-deploy; if the
   fleet observes v6-only swarms above noise, switch to parallel or alternate
   first-family); skip the v6 pass only when v4 already proves live
   (`est > threshold`). **[FIXED
   2026-10-06: prior "v6 only when v4 yields nothing aware" was unsafe]**
   "Nothing aware" includes v4-all-zero, which would skip v6 and tombstone a
   v6-live swarm after 2 rounds (proven in `/tmp/bep33_audit_fix.py`). Death
   always requires both families; the saving comes from skipping v6 only on
   proven-live, plus v6-only torrents still getting their full pass. Roughly halves RPCs for
   v4-reachable swarms (§10 skew bounded by the consecutive-failure rule
   *plus* the both-families death rule — consecutive failures alone do not
   save a systematically skipped family).
4. **Adaptive UNKNOWN intervals (§4b, §6):** UNKNOWN rows re-scrape every
   `scrape_unknown_interval_secs=30d`, live rows every 7d. Saves 38.3% of
   load at 50% unaware; reset on first aware response.
5. **Size-ordered purge (§8):** sweep deletes grace-expired rows `total_size
   DESC` — 9× bytes freed per early row under pareto 90/10. Index-drop timing
   unchanged, so recall behaviour is identical.
6. **Announce short-circuit (§4b):** `seed=1` announces on known rows refresh
   `last_scraped_at` / reset failures with zero RPCs. Only defers scrapes on
   positive liveness proof (plumbing: `SeedAnnounce` event responder→crawl worker, LB-32 §4b).
7. **DB batching:** `record_scrape` stays a stats-only batched write (no
   `nextval`, cf. `crawler.rs:23-25` log2 rule — a 1k-row scrape batch moves
   no feed position); `removed_keys` checks batch in `flush()`
   (`admission.rs:539-597`), never per-`handle()` (`:477`).

## Non-goals (explicit)

- Scrape-before-fetch gating (§8 rationale: ~2 traversals vs 1; revisit only
  if fetch-worker TCP time, not DHT budget, measures as the constraint).
- Slim stub rows (§4a rationale: no fetch-cost saving; predicate threads every
  reader).
- BEP 33 changed announce accounting / token omission at the entry cap:
  responder detail for mega-viral infohashes only; current token logic works
  without it. Explicitly **not** the 6000-enforcement mechanism (LB-5) —
  saturation is documented instead.
- `noseed=1` requests: no planned use (spec SHOULD-level, §0).
- Pure-v2 swarm scraping: v1/DHT-key first; hybrids ride along. `removed_keys`
  needs a 32 B follow-up (like `denylist IN (20,32)`).
- Tracker-first scraping (§0 BEP guidance): no tracker path exists, so DHT
  scrapes everything by conscious divergence, not oversight.

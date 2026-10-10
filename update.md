# Yield plan: raise torrents per day without adding workers

Scope: everything from the ideas list except 6 (v6 DHT stays out).
Baseline is the 2026-10-10 diag: `ok 176` vs `no_peers 1.9M` vs
`fetch_failed 1241`; admitted `sample 2.28M / get_peers 19k / announce 1.8k`;
`queue_full` blocked 737k; `peer_store_keys` 2156; DHT timeouts 184k.
Once peers exist, ~12% fetch ok — the loss is finding peers, and admissions
are 99% sampler junk with NULL estimates. So this plan filters and
prioritizes; it does not add fetch workers.

Rule for all steps: one-hour proxy gates. `ok/day` (~7/hour) is too rare
to gate on — it stays a scoreboard only. Every gate below uses counts in
the 10k–1M/hour range, which settle within one hour of live traffic.
Ship or revert each step on its proxy; never wait for `ok/day`.

## 1. Shed sampler first — DONE (shipped in 0.2.0)

Problem: `priority = source != GetPeers` (`admission.rs:576`) makes Sample
equal to Announce. Gate (`crawler.rs:319-330`): depth >= 5M drops GetPeers
only; depth >= 10M drops everything including announces. Sampler volume
drives the queue toward Closed, then announces die with it (depth counts
gave_up rows too, but the 1h purge holds those at ~1.3M — sampler is the fill).
The 737k
`queue_full` drops already paid a DB round-trip (the known/alias checks plus
`OBSERVE_COUNT_ONLY_SQL` in `observe_chunk`, `crawler.rs:1309-1372`).

Change:
- Only `Announce` is priority. Sample is non-priority but still admitted at
  once (the sighting is our own sampler's, not a stranger's claim needing the
  two-network corroboration); GetPeers keeps its two-network rule as
  non-priority. Parked: a third tier (sample gated at 0.5x cap, GetPeers at
  1x, Announce at 2x) — more code, decide after the two-tier version reads out.
- Early shed in `handle()` (`admission.rs:545`) using the last flush's cached
  flag (plain field — `handle` and `flush` run on the same task, so no atomic
  is needed): polled from `pending_depth()` at every flush, even empty ones
  (an engaged shed starves the batch, so gating the poll on batch work would
  latch it on forever). When depth is at or above `max_pending`, drop sampler
  discoveries before they enter the batch (no DB write). Shed keys don't enter
  the dedup set, so recovery is instant; re-sightings just re-hit `handle`
  (CPU only, no DB). GetPeers keys keep their two-network tracker; sampled
  sightings still admit at once (own sampler needs no corroboration) and clear
  any get_peers tracking for the key.
- No new metric: the shed volume shows as `blocked{reason=shed_sampler}` on the
  existing counter (a new label value, not a new metric).

1-hour gate: `blocked{reason=queue_full}` ≈ 0 (was 737k in the baseline window);
`blocked{reason=shed_sampler}` shows the absorbed volume; depth < 5M
(`max_pending`, `deploy/config/dhtcrawler4.toml:58`) and not climbing;
announce admitted flat vs the prior hour while sample admitted drops steeply.
Risk: starving useful sampler keys — depth-gate the shed (shed only on cached
depth high) and back off by lowering the shed threshold gradually.

## 2. Live-first claim with fallback (estimate-aware claim) — DONE (shipped in 0.3.0)

Problem: claim orders `attempts ASC, seeders_est DESC NULLS LAST`
(`crawler.rs:107`) but attempts leads: fresh keys are always NULL by
construction (no backfill, `crawler.rs:74-78`), so ~99k NULL fresh keys
claim before any live retry. The free piggyback estimate
(`fetch.rs:790-799`, `note_fetch_estimate`) only helps retries order
within their tier. Unaware lookups leave NULL, never 0 (`fetch.rs:792`).

Repro (ephemeral PG, 100k rows: 99k NULL / 700 zero / 300 live, `psql`
via `/tmp/opencode/pgstep2sock:54399` — table + data still live there):
- T1 starvation: filtered (`seeders_est > 0`) bulks of 512 return
  300, then 0, then 0 — the whole live pool fits one bulk, workers idle
  after. Unfiltered bulks return 512 (0 NULL), 512 (124 NULL),
  512 (512 NULL): the third bulk is the pure-NULL diet behind today's
  1.9M `no_peers`. Verdict: a hard filter starves; a fallback is required.
- T2 predicate: `IS NOT NULL` bulk returns 512 = 300 live + 212 dead
  zeros. `Some(0)` is measured dead (aware responses, no seeds,
  `bloom.rs:146-160`, `lib.rs:147-154`). Verdict: predicate must be
  `seeders_est > 0`, never `IS NOT NULL`.
- T3 index, CORRECTED: the `> 0` claim uses `pending_claim` as an
  `Index Cond` (`EXPLAIN`: `Index Scan using pending_claim`, empty-pool
  scan 0.13 ms / 124 buffers at 105k rows). No new index needed for the
  claim. A partial index only earns its keep for a live-count gauge —
  and the gauge below doesn't need one.
- T4 leak: under a hard filter 104k NULL + 700 dead rows are never
  claimable; +5k NULL inflow → depth 105k, still 104.7k stranded.
  `purge_gave_up` never touches never-tried keys. Verdict: the fallback
  IS the NULL consumer — no separate lane, no step-3 dependency.
- T5 measurability: `PendingItem` carries no est (`types.rs:174-179`),
  `CLAIM_SQL:99-110` doesn't `RETURNING` it, so "NULL share of claims"
  is unmeasurable today; and a queue NULL-share gauge is a
  `Parallel Seq Scan` (`EXPLAIN` proven). Verdict: no queue gauge —
  add `seeders_est` to `RETURNING` and count live/null per bulk in the
  claimer (one-line change, free — repro T5b returns live_n/null_n per
  bulk with zero extra round trips).

Change (claimer-side only, no `ORDER BY` change, no migration, no depth gate):
- Live bulk first: same `CLAIM_SQL` + `AND q.seeders_est > 0`, `LIMIT 512`.
  If it returns short of the full bulk (`n`), top up with one
  unfiltered bulk. Fallback guarantees feeding, so no depth trigger and
  no starvation mode. Live retries jump ahead of the fresh-NULL flood;
  NULLs still flow when the live pool is dry.
- Instrument: `RETURNING p.seeders_est`, emit `live_n`/`null_n` per bulk
  next to `emit()` (`fetch.rs:727-734`). In-memory only, permanent.
  No `dc3_queue_depth`-style gauge query.

1-hour gate: `live_n` per bulk > 0 while the live pool exists; `null_n`
per bulk down vs the pre-change hour; `claim_chan_depth` still flickers
> 0 and pool idle flat (fallback proves no starvation);
`no_peers`/claim flat or down, `ok`/claim up.
Risk: none of the old ones remain — fallback removes the starvation and
leak risks; the only cost is one extra claim round trip per scan while
the live pool is dry (the scan already runs ~1.5/s).

## 3. Two-stage fetch (cheap lookup, expensive dial)

Problem: a metadata dial (TCP + handshake + 20s budget) costs orders of
magnitude more than the DHT lookup that `obtain()` (`fetch.rs:742-824`)
already runs first (hints → `scrape_peers` lookup → dial up to
`MAX_PEER_ATTEMPTS = 8`, `PARALLEL_ATTEMPTS = 3`, `fetch.rs:63-66`).

Change (fetch side — needs its own justification now: step 2's fallback
leaves no deferred backlog, so this step no longer "consumes" anything):
- NULL-est keys get lookup-only passes that populate the estimate via the
  existing piggyback (`fetch.rs:790-799`) and requeue; only keys with
  dialable peers or `est > 0` spend connection/byte budget (`connections`,
  `byte_budget`, `fetch.rs:832-845`).
- No new pool: reuse the existing claim lane with an estimate predicate.

1-hour gate: `NoPeers` share of outcomes down (worker time shifts to dials
with peers); `fetch_failed` and destination-throttle drops per claim flat
or down.
Risk: lookup-only passes still cost DHT packets (`max_packets_per_sec =
1000`, `deploy/config/dhtcrawler4.toml:45`); cap their share.

## 4. More dials + deeper lookups for high-est keys only — DONE (shipped in 0.4.0)

Prereq (done in step 2): every `PendingItem` already carries
`seeders_est` (`types.rs`, `crawler.rs` `RETURNING p.seeders_est`,
`claim_bulk` in `fetch.rs:191-212`), so this is fetch-side only — no
store change, no migration.

Problem: every key gets the same effort (`MAX_PEER_ATTEMPTS = 8`,
`PARALLEL_ATTEMPTS = 3`, 6s `GET_PEERS_TIMEOUT`, `fetch.rs:64-70`, 45s
`KEY_DEADLINE`, `fetch.rs:66`), so junk burns the same budget as a
10-seeder swarm. The queue is NULL-dominated by construction (fresh keys
are NULL until scraped), so uniform effort spends most dials where
expected value is lowest.

Change — effort buckets by estimate (one helper, `effort_for(est)`):
- `null` (None) and `dead` (Some(0); measured dead, never live): 3 dials
  max, one 6s lookup, no second lookup. `Some(0)` must behave like NULL —
  the old text left it undefined.
- `low` (1–4): today's effort exactly — 8 dials max, one 6s lookup, no
  second lookup. (The old text left 1–4 undefined; standard effort is the
  safe default.)
- `high` (>= 5, `HIGH_EST_MIN = 5`, starting value not derived — no
  est-split outcome data exists yet; move it on the gate): 8 dials max
  plus up to one second 6s lookup before giving up.
- Fan-out stays 3-wide for every bucket (`PARALLEL_ATTEMPTS` unchanged):
  only depth/retries scale, never concurrency, so per-key connection
  pressure never exceeds today's.
- Hints share the budget: hints are queued before the lookup
  (`fetch.rs:859-864`) and the single `attempts` counter covers
  hints + lookup peers, so "3 dials max" means 3 total, not 3 after
  hints. Peerless keys cost ~0 dials either way (no peers → no starts),
  so the 8→3 cut saves up to 62% of dial *budget* on keys with peers —
  measured dials drop less, because most junk already dies peerless.
- Second-lookup rule (high bucket only): after the first lookup completes,
  if no dial has succeeded and (`queue` empty and `running` empty, or every
  started dial failed) and time remains inside `KEY_DEADLINE`, issue exactly
  one more `scrape_peers(key, get_peers_timeout)`. Cap 2 lookups per key,
  always inside the existing `timeout(key_deadline, obtain(...))`
  (`fetch.rs:790`). No new timeout knob. Restructure required: today's
  `tokio::pin!(lookup)` + `lookup_done: bool` became
  a re-creatable future + `lookups_done: u8` (`fetch.rs:870-873`) — feasible because
  `PeerSource::scrape_peers` takes `&self` and returns a fresh future per
  call (`peers.rs:28-32`, `dc3-dht/src/lib.rs:255`), and `obtain()` has a
  single caller (`fetch.rs:790`, grep-proven), so no other path changes.
  Also update the `Deferred` bound comment ("attempt budget plus one
  lookup", `fetch.rs:384`) to plus two — the termination argument gains
  one progress step (repro-simulated: all six adversarial cases terminate
  within caps, including double-empty and limiter-refuses-all).
- The second piggyback write is safe: `note_fetch_estimate` is a plain
  `UPDATE pending SET seeders_est` (`crawler.rs:1105`), last-wins —
  repro-proven on the live PG (5 then 9 reads 9, row restored to NULL).
- Ceilings unchanged: 45s key deadline, 120s `CLAIM_LEASE` (`fetch.rs:56`).
- Code touches: `obtain()` takes the item's est (`fetch.rs:850`, caller
  `fetch.rs:790`), derives the bucket once, and uses effective
  `max_attempts` (estimate budget capped by `tuning.max_attempts`,
  `fetch.rs:850-855`) instead of `tuning.max_attempts` directly.
  `attempt()` unchanged. Destination-limiter skips still don't
  consume dial budget (only `Ok(permit)` increments `attempts`,
  `fetch.rs:884-888`) — keep that. No `metrics_server.rs` change: its
  pipeline test (`metrics_server.rs:231-236`) builds its own counter with
  its own labels, so production `emit()` labels can't break it — proven by
  the full suite staying green with the `est` label live (133/133).
- Metrics (both needed — the old text added only a dial counter, which
  can't split the gate "on high-est keys"): add `est` bucket label
  (`null`/`dead`/`low`/`high`, 4 values) to a new
  `dc3_fetch_dials_total{est}` (`fetch.rs:118`) incremented once per started dial next to
  the `try_acquire` success (`fetch.rs:884-888`), and the same `est` label
  on `dc3_fetch_total` in `emit()` (`fetch.rs:830-839`; the old
  `fetch.rs:727-734`/`728,774` refs are stale post-step-2). Label value is
  the claim-time `item.seeders_est` bucket on both counters — the
  mid-fetch piggyback writes the DB row, never the in-hand item, so
  claim-time keeps dials and outcomes joinable. 4×6 + 4 series, in-memory
  only, permanent. `ok per 10k dials` per bucket is then
  `fetch_total{outcome="ok",est} / dials{est}` — unmeasurable today
  because outcomes count keys and skips, not dials.

1-hour gate (diag diff, prior hour vs new hour): `fetch_failed` share
within `est="high"` down; ok per 10k dials in `high` up; dial *budget*
on `null`/`dead` down 62% with measured dials down less (peerless keys
unchanged at ~0); `no_peers` per claim flat or up (junk correctly finding
nobody faster); pool idle (`dc3_db_pool_idle`) and
`dc3_dht_timeouts_total` (`crawl.rs:377`) flat (no new saturation). All
series are in the diag (bare `^dc3_` grep). Volume caveat: `high` keys
are rare (300/105k in the repro mix) — if `est="high"` N/hour < ~1k,
the share is noisy; gate then on dial-direction + no saturation and
extend the read, don't ship on noise.
Risk: connection/destination pressure (`max_connections = 3072` in
`deploy/config/dhtcrawler4.toml`, `DEST_MAX_CONCURRENT = 2`,
`fetch.rs:74`); bounded because only `high` keys spend extra and fan-out
never rises. Rollback is one helper (uniform effort back).

## 5. Generous timeouts on retry only

Problem: one `FetchTuning` for both attempts (`connect 3s / handshake 4s /
peer 20s`, `fetch.rs:68,70,72`); slow intercontinental seeders die twice the
same way.

Change:
- Attempt 1 stays fast; attempt 2+ uses generous timeouts (e.g. connect
  3s→6s, handshake 4s→8s) inside the same 45s key deadline.
- Branch in `attempt()` (`fetch.rs:826-846`) on `item.attempts`.
- Add an `attempt` label (0/1) to `dc3_fetch_total` and an overrun counter
  on the key-deadline timeout (`fetch.rs:689`): attempts today exist only
  in a debug log (`fetch.rs:729-733`), so attempt-2 conversion and deadline
  overruns are both unmeasurable without them. In-memory only, permanent,
  tiny cardinality.

1-hour gate: attempt-2 `Failed→ok` conversion up (read with `fetch_failed`
on high-est keys down, not alone); deadline overruns ≈ 0.
Risk: longer tail holds `connections` permits; bounded by step 4 (only
high-est keys get the second, slower round).

## 6. Bigger hint retention

Problem: `HINT_KEYS = 100k`, `HINT_PEERS_PER_KEY = 8`, `HINT_TTL = 5min`
(`admission.rs:53,55,57`) against 2.7M samples/day; live `peer_store_keys`
2156. Hints evaporate before retry.

Change:
- Raise keys 100k→500k and TTL 5min→30min first; peers-per-key 8→16 only if
  memory allows. Watch `dc3_dht_peer_store_keys` (`crawl.rs:382`) and hint
  hit rate in `obtain()` (`fetch.rs:746-752`).

1-hour gate: `peer_store_keys` up; overall second-half `NoPeers` share down
(overall share — the per-attempt split arrives with step 5's label, which
ships before this step); no refused-new spike from the destination map
(`DEST_CAPACITY = 100k`, `fetch.rs:81-82` — grow it with the hints or new
IPs get refused).
Risk: RAM (5x keys is real).

## Order and stop rules

1. Step 1 (shed sampler) — biggest lever, admission-only, reversible. First
   deploy, gates the rest.
2. Steps 2–3 (estimate-aware claim + two-stage) — claim filter first, then
   the lookup behavior that consumes what it defers. One per hour.
3. Steps 4–5 (effort scaling + retry timeouts) — fetch-side, bounded by
   estimates. Step 4's dial counter and step 5's attempt label are
   permanent metric additions, not scaffolding.
4. Step 6 (hints) — last, only if retries still starve for peers.

Stop/rollback per step, judged at the 60-minute mark against the prior
hour: proxy misses, or `queue_full` returns, or pool/destination saturation
appears — revert the one knob, keep the rest. `ok/day` is scoreboard only.
Keep `MAX_FETCH_ATTEMPTS = 2` throughout; the parked MAX=1 experiment
revisits only after yield is stable.

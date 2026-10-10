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

## 1. Shed sampler first (queue_full drops the wrong keys today)

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

## 2. Stop fetching blind (estimate-aware claim)

Problem: claim orders `attempts ASC, seeders_est DESC NULLS LAST`
(`crawler.rs:107`) but still claims NULL-est keys when workers are free —
~99k of 100k queued are NULL-est. The free piggyback estimate
(`fetch.rs:790-799`, `note_fetch_estimate`) already populates `seeders_est`
on contact, so estimates improve over time if we let them.

Change (claim side only — the lookup behavior is step 3):
- When depth is high, `CLAIM_SQL` takes only `seeders_est IS NOT NULL`
  (or `> 0`); NULL-est keys stay queued for the lookup-only pass.
- Track NULL share as a gauge alongside `dc3_queue_depth`.

1-hour gate: NULL share of claims falls vs the pre-change hour; workers not
starved (`claim_chan_depth` still flickers > 0, idle time flat);
`no_peers`/claim flat or down.
Risk: starving workers if the filter is too strict — keep it depth-gated,
not absolute. Interacts with step 1 (less sampler junk = fewer NULLs).

## 3. Two-stage fetch (cheap lookup, expensive dial)

Problem: a metadata dial (TCP + handshake + 20s budget) costs orders of
magnitude more than the DHT lookup that `obtain()` (`fetch.rs:742-824`)
already runs first (hints → `scrape_peers` lookup → dial up to
`MAX_PEER_ATTEMPTS = 8`, `PARALLEL_ATTEMPTS = 3`, `fetch.rs:63-66`).

Change (fetch side — consumes the deferred keys step 2 leaves):
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

## 4. More dials + deeper lookups for high-est keys only

Problem: every key gets the same effort (8 dials, 3-wide, 6s lookup
`GET_PEERS_TIMEOUT`, `fetch.rs:60`, 45s `KEY_DEADLINE`, `fetch.rs:62`),
so junk burns the same budget as a 10-seeder swarm.

Change:
- Scale effort by estimate: NULL-est → 3 dials, one 6s lookup; `est >= 5`
  → full 8 dials plus one second deeper lookup before giving up.
- Keep `KEY_DEADLINE` (45s) and lease (120s, `fetch.rs:52`) as the ceilings.
- Add a dial-attempt counter next to `emit()` (`fetch.rs:727-734`): the
  current counters (`fetch.rs:728,774`) count outcomes and throttle skips,
  not dials, so "ok per dial" is unmeasurable without it. In-memory only,
  permanent, nearly free.

1-hour gate: `Failed` (peers found, fetch lost) share on high-est keys down;
ok per 10k dials up; pool idle and DHT timeouts flat (no new saturation).
Risk: connection/destination pressure (`max_connections = 3072`,
`DEST_MAX_CONCURRENT = 2`, `fetch.rs:74`); gate by estimate so only
proven swarms spend it.

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

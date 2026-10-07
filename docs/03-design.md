# 03 — Design

This design implements the requirements R1–R17, R19–R20 from
[01-first-principles](01-first-principles.md). Each section names the requirements it
serves. Terms are defined in [00-horismos](00-horismos.md).

Revision 2 (2026-09-16) folds in an independent design review; its findings are
listed in §16. Details the implementation added since, such as extra error variants,
read bounds and the `budget_exhausted` flag, are recorded in the sections they
affect. Where a crate's own documentation is more specific, the crate is
authoritative.

## 1. Shape of the system

```
                 ┌────────────────────────── crawl ───────────────────────────┐
 Mainline DHT ⇄  │ dc3-dht node (v4 + v6)                                     │
   (UDP)         │  ├ answers ping/find_node/get_peers/announce/sample (R2)   │
                 │  ├ BEP 51 sampler ──► Discovered(key) ─┐                   │
                 │  └ announces / get_peers seen ────────►┤                   │
                 │                                        ▼                   │
                 │         admission (dedup, gating) → observe batch          │
                 │                                        │                   │
                 │   PostgreSQL ◄── pending (lease queue) ┘                   │
 peers ⇄         │   fetch workers: claim → get_peers → address chokepoint    │
   (TCP)         │     → dc3-peer BEP 9 fetch → verify → parse → store    │
                 └────────────────────────────────────────────────────────────┘
                 ┌──── index ─────┐          ┌──────────── web ────────────────┐
 PostgreSQL ────►│ tail change_seq│─► Tantivy│ axum + askama, no JS, strict CSP│◄─ reverse proxy ◄─ visitors
                 │                │  index ─►│ search → hydrate from Postgres  │
                 └────────────────┘          └─────────────────────────────────┘
```

A single binary, `dhtcrawler3`, provides three long-running roles, `crawl`, `index`
and `web`, plus admin subcommands (R20). It also has `all`, which runs the three roles
together for small installs.

Each role connects to PostgreSQL with its own least-privilege database role (R12). The
`all` command opens three connection pools, one per role, each with that role's
credentials.

Every role serves `/metrics`, `/healthz` and `/readyz` on its metrics listener (R19).

## 2. Workspace

```
crates/
  dc3-core      shared types: DhtKey, InfoHashV2, AnyKey, magnet links, text sanitising
  dc3-bencode   bounded bencode decoder/encoder with raw spans            (R7)
  dc3-torrent   info-dict parsing (v1/v2/hybrid), verification            (R4 R6 R8)
  dc3-dht       KRPC, routing table, BEP 42 IDs, tokens, BEP 51 sampler   (R1-R4)
  dc3-peer      BEP 3/10/9 metadata fetch; test seeder                    (R5 R7)
  dc3-store     PostgreSQL schema, migrations, lease queue, repositories  (R15)
  dc3-search    Tantivy schema, CJK tokenizer, query builder, generations (R16 R17)
  dc3-web       axum app, templates, security middleware                  (R9-R12)
  dhtcrawler3   the binary: config, CLI, pipeline wiring, end-to-end tests (R19 R20)
fuzz/           cargo-fuzz targets (separate workspace)                   (R7)
```

Dependency direction: `core` ← `bencode` ← {`torrent`, `dht`, `peer`}; `core` ←
{`store`, `search`} ← `web` ← `dhtcrawler3`. There are no cycles.

Rules for every crate:
- `#![forbid(unsafe_code)]`.
- Library code never panics on input. Every `unwrap` or `expect` in library code carries a comment proving it cannot fail.
- Errors use `thiserror`.
- Every numeric limit is a named constant or configuration value. Magic numbers are not allowed.
- **Logs never contain peer, node or visitor IP addresses, or search text** (R11). Addresses may appear only at `trace` level, which is disabled in release builds.

## 3. Hard limits (R7, R17, A6)

| Limit | Value | Where |
|---|---|---|
| UDP receive buffer | 2 048 bytes; a datagram that fills it is dropped | dht |
| Outgoing KRPC datagram | ≤ 1 024 bytes (BEP 32); `nodes`, `values` and `samples` are trimmed to fit | dht |
| KRPC bencode depth / items / string | 8 / 512 / 2 048 | dht |
| KRPC transaction ID length | 1–8 bytes accepted; we send 2 | dht |
| Query timeout | 4 s; no retries (BEP 5) | dht |
| Inbound per-host rate | 4 packets/s, burst 8 (IPv4 address or IPv6 /64); excess dropped, so replies to one host stay under libtorrent's 5 packets/s ban threshold | dht |
| Inbound rate-limit map | 100 000 IPs (LRU) | dht |
| Outbound per-IP spacing | ≥ 1 s between queries to one IP | dht |
| Outbound spacing map | 100 000 IPs; entries older than 1 s may be evicted | dht |
| Global outbound query budget | `crawl.max_packets_per_sec` (default 250) | dht |
| Responder budget (replies) | 500 packets/s and 64 000 bytes/s; queries beyond it are dropped unanswered | dht |
| Peer store | 20 000 keys × 100 peers per key; 45 min expiry; memory only | dht |
| Routing-table entries per IP | 1; no two entries in one bucket from the same /24 (v4) or /64 (v6) | dht |
| Sampler frontier / visited map | 50 000 / 1 000 000 entries; unexpired visited entries are never evicted | dht |
| External-IP votes | one per /24 (v4) or /48 (v6); 30 min expiry; ≥ 10 votes and > ⅔ agreement; ID change ≤ once per hour | dht |
| Admission dedup set | 2 generations × 2 000 000 keys, rotated every 30 min | dhtcrawler3 |
| `get_peers`-only keys | queued only after sightings from ≥ 2 distinct /24 (v4) or /48 (v6) sources within one dedup generation | dhtcrawler3 |
| Pending queue | `crawl.max_pending` (default 5 000 000); above it only Sample and Announce keys are admitted; at 2 × `max_pending` no new key is admitted | dhtcrawler3, store |
| Announce keys per source | 20 new keys per host (IPv4 address or IPv6 /64) and 50 per /24 or /48 per 10 min; one peer entry per IP per key, at most 4 per /64; a full peer store refuses new keys | dht |
| Samples per source | 20 per reply (from the expected node ID only) and 200 per /24 or /48 per 10 min | dht |
| Hint map | 100 000 keys × 8 peers; 5 min expiry | dhtcrawler3 |
| TCP connect / handshake / fetch per peer / fetch per key | 5 s / 5 s / 30 s / 60 s | peer, dhtcrawler3 |
| Extended (ID 20) message | 16 KiB + 1 KiB; 64 KiB for the extended handshake; buffered | peer |
| Other peer messages | read and discarded without buffering, ≤ 4 MiB each (a bitfield for 2²⁵ pieces); longer closes the connection | peer |
| `metadata_size` | 1 ..= `crawl.max_metadata_bytes` (default 8 MiB) | peer |
| Metadata bytes in flight | `crawl.max_inflight_metadata_bytes` (default 256 MiB), a byte semaphore acquired per received piece | peer, dhtcrawler3 |
| Concurrent peer connections | `crawl.max_connections` (default 256) | dhtcrawler3 |
| Per destination IP | ≤ 2 concurrent connections and ≤ 10 attempts per minute; refused or timed-out destinations are skipped for 10 min (map: 100 000 IPs) | dhtcrawler3 |
| Metadata bencode depth / items | 64 / 1 000 000 | torrent |
| Visited text | 4 characters per metadata byte, clamped to 1–16 MiB; beyond it the torrent is rejected (`TooMuchText`, fail closed). Padding paths and a hybrid's v1 list are visited too | torrent |
| Detail lookups (store) | at most 256 KiB of path text per `get_by_key`, then `files_truncated` | store |
| File-tree depth | 64 | torrent |
| Files parsed / stored | 200 000 / 2 000 (the rest are counted and flagged as truncated) | torrent |
| Name / path length | 1 024 / 4 096 characters after sanitising | torrent |
| `files` search field | 64 KiB of text | search |
| Prefix expansions | 200 terms per field | search |
| Search query | ≤ 200 characters, ≤ 12 words, page ≤ 50, 20 results per page (API ≤ 50), 5 s timeout, ≤ 16 concurrent searches | search, web |
| HTTP request body | 4 KiB | web |
| Web rate-limiter map | 100 000 prefixes, 10 min idle expiry; refilled buckets are forgotten first; when full, new clients share one of 64 overflow buckets chosen by IPv4 /16 or IPv6 /32 | web |
| HTTP request head | must arrive within 10 s of the connection opening (or of the next request's first byte); trickled bytes do not extend it | web |

## 4. dc3-core

```rust
pub struct DhtKey(pub [u8; 20]);      // Display: 40 lowercase hex. FromStr: 40 hex (any case) or 32 base32.
pub struct InfoHashV2(pub [u8; 32]);  // Display: 64 lowercase hex. FromStr: 64 hex.
impl InfoHashV2 { pub fn truncated(&self) -> DhtKey }
pub enum AnyKey { V1OrDht(DhtKey), V2(InfoHashV2) }   // FromStr: 64 hex → V2, otherwise the DhtKey rules
pub fn magnet_link(v1: Option<&DhtKey>, v2: Option<&InfoHashV2>, display_name: Option<&str>) -> Option<String>;
    // xt=urn:btih:<hex> and/or xt=urn:btmh:1220<hex>; dn percent-encoded, ≤ 200 chars; no trackers.
    // Returns None when both hashes are absent.
pub mod text {
    pub fn sanitize_display(s: &str, max_chars: usize) -> String; // NFC; drop C0, DEL and C1 controls, bidi controls, zero-widths, non-characters; collapse whitespace; truncate
    pub fn sanitize_path_component(s: &str) -> Option<String>;     // None for "", ".", ".."; '/' and '\\' become '_'
    pub fn is_bidi_control(c: char) -> bool;
}
```

## 5. dc3-bencode (R7)

A bencode parser owned by the project, rather than a third-party crate. The reasons:
- We need **raw spans**, so infohashes are computed over the exact bytes received.
- We need **prefix decoding**, because a `ut_metadata` data message is a dict followed by raw bytes.
- We need **hard limits**. A source review found that two popular crates, `serde_bencode` 0.2.4 and `bt_bencode` 0.8.2, recurse with no depth guard (the overflow was not reproduced).

The public API (`decode`, `decode_prefix`, `Value`, `Dict` with `raw_value`, `OwnedValue`,
`encode`, `Limits::{KRPC, PEER_MESSAGE, METADATA}`) is in the crate.

**Rejected input:**
- leading zeros in integers and string lengths;
- `-0`, `ie`, and integers outside the i64 range;
- non-string or duplicate dictionary keys;
- truncated input;
- trailing bytes (for `decode`);
- exceeding any limit.

**Accepted input:** unsorted keys. Hashing uses raw spans, so this leniency is harmless.

Decoding and encoding are iterative, so nesting depth can never exhaust the stack.

## 6. dc3-torrent (R4, R6, R8)

```rust
pub enum Verified { V1, V2Truncated }
pub fn verify(key: &DhtKey, info: &[u8]) -> Option<Verified>;   // SHA-1 == key, else SHA-256[..20] == key
pub fn parse_info(info: &[u8]) -> Result<TorrentMeta, ParseError>;
pub fn parse_info_visit(info: &[u8], visit: &mut dyn FnMut(&str)) -> Result<TorrentMeta, ParseError>;
    // Calls `visit` with the sanitised name and with every sanitised path parsed
    // (up to MAX_FILES_PARSED), including paths not kept in `files`.
```

`TorrentMeta` has these fields:
- `name`;
- `info_hash_v1` and `info_hash_v2`;
- `total_size` and `file_count` (both excluding padding);
- `files` (at most 2 000, sorted) and `files_truncated`;
- `piece_length`;
- `private`.

The parsing rules are those in the crate docs:
- **Text:** `*.utf-8` keys first; otherwise valid UTF-8, then the `encoding` label, then GB18030, then lossy.
- **v1:** a `length` or `files` list.
- **v2:** an iterative `file tree` walk.
- **Hybrids:** both hashes are recorded, and the file list comes from the v2 tree.
- **Padding:** hidden from listings and totals.
- **Sizes:** checked arithmetic.

## 7. dc3-dht (R1–R4)

### Public API (summary; the crate is authoritative)
```rust
pub struct DhtConfig { bind_v4, bind_v6, bootstrap, state_file, max_packets_per_sec, sampler,
                       sampler_concurrency, read_only, allow_private_addrs, client_version, tuning: DhtTuning }
pub enum Source { Sample, Announce, GetPeers }
pub struct Discovered { pub key: DhtKey, pub source: Source, pub peer: Option<SocketAddr>, pub from: IpAddr }
pub struct Dht;   // cheap-to-clone handle
pub enum Error { Config(String), Bind { addr, source }, NoSocket }
pub struct DhtStatsSnapshot { v4: FamilyStats, v6: FamilyStats, queries: QueryCounts, sampler_early, sampler_visited_full, responder_dropped, .. }
    // FamilyStats: packets in/out, drops by DropReason (rate_limited, oversized, malformed, unsolicited,
    // filtered, responder_budget, throttled, send_error), samples, routing-table size
impl Dht {
    pub async fn start(cfg: DhtConfig, sink: mpsc::Sender<Discovered>) -> Result<Dht, Error>;
    pub async fn get_peers(&self, key: DhtKey, timeout: Duration) -> Vec<SocketAddr>;
    pub async fn announce(&self, key: DhtKey, port: u16) -> usize;
    pub fn stats(&self) -> DhtStatsSnapshot;
    pub fn local_addrs(&self) -> Vec<SocketAddr>;
    pub fn own_addrs(&self) -> Vec<IpAddr>;   // local and voted external addresses (for the address chokepoint)
    pub fn good_nodes(&self) -> usize;          // readiness
    pub async fn shutdown(self);
}
pub fn is_dialable(addr: SocketAddr, allow_private: bool) -> bool;   // the address chokepoint, see below
```
`sink` is a bounded channel. When it is full, discoveries are dropped and counted.

### Behaviour
- **Sockets and IDs.**
  - One socket per address family. The IPv6 socket sets `IPV6_V6ONLY`.
  - Each socket has its own routing table (BEP 32) and its own node ID.
  - BEP 32 recommends one ID for both families, but BEP 42 derives the ID from each family's external address, so the two IDs necessarily differ. That also matches BEP 45's rule of one ID per socket address.
  - `bind_v6` defaults to `[::]:6881`. If the host has no global IPv6 address, the IPv6 socket is disabled with a warning.
- **External IP and BEP 42.**
  - The first ID is random (or the persisted one).
  - The external IP is chosen by a vote over the top-level `ip` field of **responses to our own queries**. Each /24 (v4) or /48 (v6) casts at most one vote, and votes expire after 30 min.
  - A candidate wins with at least 10 votes and more than two-thirds of current votes.
  - If our ID is not BEP 42-valid for the winner, a valid ID is derived and the routing table re-bootstraps, at most once per hour.
  - IDs and the winning IP are persisted.
  - The five BEP 42 vectors (computed by the BEP's example code, not its prose) are unit tests.
- **Routing table.**
  - BEP 5 rules: K=8, and only the bucket holding our own ID splits.
  - Tracks good, questionable and bad state, keeps a replacement cache, and refreshes buckets after 15 min.
  - One entry per IP, and no two from the same /24 or /64 in a bucket.
  - When replacing, prefers BEP 42-valid nodes.
  - Never admits `ro`=1 senders, our own ID, our own addresses, or non-dialable addresses.
- **Responder** (disabled when `read_only`; every query we send then carries `ro`=1):
  - `ping` → `id`.
  - `find_node` → the 8 closest nodes to **`target`**, as `nodes` and/or `nodes6` according to `want`.
  - `get_peers` → `token`, `nodes` (and `nodes6` if wanted), and a random sample of stored `values`, trimmed so the reply fits in 1 024 bytes. This is roughly ≤ 88 IPv4 or ≤ 28 IPv6 values with one node list.
  - `announce_peer` → the token is checked against the current and previous secret for the source IP; the port is range-checked and `implied_port` honoured. The peer is stored and `Discovered{Announce, peer}` is emitted. A bad token gets error 203.
  - `sample_infohashes` → up to 20 random stored keys, `num`, `interval` (21 600 s in production) and `nodes`.
  - An unknown method carrying `target` or `info_hash` is answered like `find_node`; anything else gets error 204.
  - Every response carries the top-level `ip` and `v`.
  - An inbound `get_peers` emits `Discovered{GetPeers, from}`.
  - Replies draw on the separate responder budget (§3). Beyond it, queries are dropped before any processing (load shedding): no routing update, no discovery, no stored announce.
- **Tokens.** The first 8 bytes of SHA-1(secret ‖ IP bytes). The secret rotates every 5 min, and the previous secret stays valid.
- **Transactions.** Keyed by (2-byte ID, remote endpoint). Unknown responses are ignored.
- **Rate limits** are as in §3. No reverse DNS.
- **Bootstrap.**
  - Configured hosts (all A/AAAA records) plus saved contacts.
  - Exponential backoff (1 s → 5 min) while there are fewer than 8 good nodes.
  - Bootstrap routers are used only while the table is under-filled.
- **Sampler (BEP 51).**
  - A bounded frontier of candidates, fed by routing-table nodes and by `nodes`/`nodes6` in every response.
  - A bounded **visited** map from a node, identified by its endpoint (and its ID once known), to its earliest next sample time.
  - **Only expired entries are evicted.** When the map is full of unexpired entries, the sampler admits no new nodes: it waits, or refills the frontier. It increments `dc3_dht_sampler_visited_full_total`. It never forgets an unexpired interval.
  - **On a response:** emit the samples, then set next time = now + max(`interval`, 300 s).
  - **On a reply without `samples`:** the node does not support BEP 51; skip it for 6 h.
  - **On a timeout:** skip the node for 1 h.
  - A sample sent earlier than the recorded time would be counted in `dc3_dht_sampler_early_total`, which must stay 0.
- **Address chokepoint.** `is_dialable` is the single filter for every address the crawler would dial or put in a routing table. That covers compact lists, announce senders and the hint map. It:
  - canonicalises IPv4-mapped and IPv4-compatible IPv6 addresses to IPv4;
  - rejects IANA special-purpose ranges: unspecified, loopback, private, CGNAT, link-local, documentation, benchmarking, multicast, broadcast, reserved, ULA, and 6to4/Teredo/NAT64 addresses that wrap such an address;
  - rejects port 0.

  With `allow_private_addrs`, only unspecified addresses and port 0 are rejected. Callers also reject our own addresses (`own_addrs`).
- **Peer store.** Bounded (§3), memory only, never persisted, and exposed only through protocol answers (R11).
- **Test support.** `DhtTuning` holds every timing and threshold, with production defaults. It includes `limits_by_endpoint` (test-only; default `false`), which keys the per-IP routing, rate, vote-diversity and own-address rules on IP:port. This lets many nodes share `127.0.0.1` in tests. Production keys these rules on the IP address, so any port of one of our own IPs counts as ours.
- **Peer-store sharing.** The 100-peers-per-key limit is shared by both address families.

## 8. dc3-peer (R5, R7)

```rust
pub struct FetchLimits { pub connect: Duration, pub handshake: Duration, pub total: Duration,
                         pub max_metadata: usize, pub byte_budget: Option<Arc<tokio::sync::Semaphore>> }
pub async fn fetch_metadata(peer: SocketAddr, key: DhtKey, limits: &FetchLimits) -> Result<Vec<u8>, FetchError>;
pub mod seeder { pub async fn serve(..); pub async fn serve_with(.., Misbehaviour); }  // for tests
```

The fetch sequence:
1. Connect with a timeout.
2. Send the handshake with `reserved[5] |= 0x10` and `reserved[7] |= 0x01`, and a random peer ID beginning `-DC0100-`.
3. Read exactly 68 bytes. Require `pstr` to match, the infohash to equal the key, and `reserved[5] & 0x10`.
4. Send the extended handshake `{m: {ut_metadata: 1}, v, reqq: 250}`.
5. Loop over frames:
   - Messages other than ID 20 are **read and discarded without buffering** (≤ 4 MiB each).
   - Extended messages are buffered within their cap.
   - From the extended handshake, take `m.ut_metadata` (1..=255) and `metadata_size` (within the limit).
   - Request pieces, with at most 4 outstanding.
   - For each data message: `decode_prefix` the dict, then check `msg_type` = 1, the piece index is in range and not a duplicate, `total_size` = `metadata_size`, and the length is exactly 16 KiB (except the last piece, which must be the exact remainder).
   - Before a piece is kept, acquire its bytes from `byte_budget`, if one is set. Permits are held until the fetch ends.
   - An **incoming request** (`msg_type` = 0) is answered with a reject for the same piece, as BEP 9 requires of peers without the full metadata.
   - `msg_type` = 2 (reject) fails the fetch. Unknown `msg_type` values are ignored.
6. Assemble the pieces and verify them (SHA-1, or truncated SHA-256).

Not implemented yet: MSE/PE encryption and uTP (§15).

## 10. dc3-store (R15)

The migrations embedded in the crate are the specification.

**Tables:**
- **`torrents`**:
  - `id`, `dht_key` (unique), `info_hash_v1` (unique), `info_hash_v2` (unique);
  - `name`, `total_size`, `file_count`;
  - `files jsonb` (lz4 where available), `files_truncated`, `piece_length`;
  - `seen_count`, `first_seen_at`, `last_seen_at`;
  - `change_seq` (unique index);
  - `deleted_at`.
- **`pending`**: `dht_key` (PK), `discovered_at`, `seen_count`, `attempts`, `next_attempt_at`, `lease_until`, `gave_up`.
- **`audit_log`**, **`stats_daily`**, and **`settings`** (owner-managed key/value).

**Keys.**
- A key is **known** if it equals any stored row's `dht_key`, `info_hash_v1`, or the first 20 bytes of `info_hash_v2`.

**Change feed.** Every insert or update that the index must see runs in one transaction, in this order:
1. `SELECT pg_advisory_xact_lock_shared(CHANGE_LOCK_KEY)`, as the first statement;
2. any per-row locks;
3. `change_seq = nextval('change_seq')`. The sequence uses `CACHE 1`.

Writer sessions set `idle_in_transaction_session_timeout`.

The indexer takes a **high-water mark** at most once per poll interval, and only when
its previous batch was not full. To take it, it sets `lock_timeout = 200ms`, takes the
same lock exclusively, reads `last_value` (0 while `is_called` is false), and commits
at once. On a lock timeout it keeps its previous mark.

Every sequence number at or below the mark belongs to a *finished* (committed or
rolled-back) transaction. So no committed change is skipped, and gaps in the sequence
are expected.

**Popularity.** An observation increments `seen_count`. It bumps `change_seq` only when
`floor(log2(seen_count))` changes.

**Queue:**
- `claim(n, lease)`: `UPDATE … WHERE dht_key IN (SELECT … FOR UPDATE SKIP LOCKED)`, ordered by `next_attempt_at`.
- `renew(key, lease)`: extends a lease.
- `fail(key)`: `next_attempt_at = now() + 5 min × 2^(attempts before this failure)`, which gives waits of 5, 10, 20, 40 and 80 min. `gave_up` is set on the 6th failure, about 2.6 h after the first attempt.
- `purge_gave_up(older_than)`.
- `pending_depth()`.

**`complete(key, meta)`** runs in one transaction:
1. Delete the pending row.
2. Update the row whose `dht_key`, `info_hash_v1` or `info_hash_v2` matches, keeping `first_seen_at`, or insert a new row.
3. Add the pending row's `seen_count`, bump `change_seq`, and increment `stats_daily.fetched`.

**Roles** are created by `deploy/postgres/init`. Grants are applied by a migration. The web role has no write grants at all.

| Role | Grants |
|---|---|
| `dc3_crawler` | SELECT/INSERT/UPDATE on `torrents`, `pending`, `stats_daily`; INSERT on `audit_log`; DELETE on `pending`; USAGE on `change_seq` |
| `dc3_indexer` | SELECT on `torrents` and the `change_seq` sequence (the high-water mark reads `last_value` directly) |
| `dc3_web` | SELECT on `torrents` and `stats_daily`. **No** INSERT, UPDATE or EXECUTE. |
| `dc3_owner` | owns everything; runs migrations and admin commands |

All queries use bound parameters.

**Read bounds.**
- `changes_since` returns at most 1 000 rows per page, and also stops at a byte cap, so a short page does not prove the range is drained. Callers loop until an empty page.
- `get_many` returns at most 10 file rows per record for result lists. `get_by_key` returns the full stored list.
- Public statistics count only visible rows.

**File-list validation.** The shape of `torrents.files` is checked by a trigger that runs only when `files` is written, so popularity updates never detoast the list.

## 11. dc3-search (R16, R17)

**Fields:**

| Field | Type | Contents |
|---|---|---|
| `id` | fast, indexed, stored | |
| `name`, `files` | text, `dc3` tokenizer, positions | `files` holds concatenated paths, capped at 64 KiB |
| `name_cjk1`, `files_cjk1` | text, no positions | every CJK character as a unigram |
| `size`, `created`, `seen` | fast | |

**The `dc3` tokenizer:**
- NFKC, then lowercase.
- Runs of letters and digits; punctuation separates tokens.
- A CJK run emits overlapping bigrams (a lone CJK character emits itself).
- Other runs are ASCII-folded.
- Tokens over 64 bytes are dropped.

**Query builder.** Hand-written; Tantivy's query-language parser is not used.
- Words may be negated (`-word`) or quoted (`"phrase"`).
- Each positive word must match `name` (wrapped in a `BoostQuery` × 3) **or** `files`.
- A multi-token word becomes a phrase query.
- A **one-character CJK word** searches the `*_cjk1` fields.
- The last non-CJK word of ≥ 2 characters also matches as a prefix (≤ 200 expansions per field), unless the query ends with a space.
- A query with no positive words is an error.

**Ranking.** BM25 × (1 + 0.15 · log10(1 + `seen`)). Other sort orders are `newest`, `size` and `seen`.

**Generations.** The index root holds numbered generation directories and a `CURRENT`
file naming the live one. It is replaced atomically: write a temporary file, then
rename.
- Readers watch `CURRENT` and reopen when it changes.
- `index --rebuild` refuses to start while the live generation's writer lock is held. It then:
  1. builds a new generation from checkpoint 0 while the old one keeps serving;
  2. switches `CURRENT` once it has caught up;
  3. deletes the old generation after a grace period.
- The checkpoint is stored in each generation's commit payload.

**Index stamp.** `SearchIndex::stamp()` snapshots the open segment set plus delete
opstamps — exactly what `reload_if_changed` compares — with no I/O, and
`stamp_matches` compares a stored stamp without cloning. The web search cache
(§12) versions all entries with one global stamp; any commit invalidates on the
next request. The generation number is promotion-only and is not used: it misses
the common intra-generation commits.

**Indexer loop** (in the binary):
1. Take the high-water mark (§10).
2. Read up to 1 000 rows with `checkpoint < change_seq ≤ mark`.
3. Delete rows that are tombstoned. Upsert the rest.
4. Commit with the batch's last `change_seq`.
5. Repeat without sleeping if the batch was full. Otherwise sleep for `index.poll_interval_ms`.

## 12. dc3-web (R9–R12)

```rust
pub struct WebConfig { listen, base_url, site_name, hsts, trusted_proxies: Vec<IpNet>,
                       search_cache_entries: usize, search_cache_ttl: Duration, .. }
pub fn app(state: AppState) -> axum::Router;
pub async fn serve(cfg: WebConfig, store: Store, index: SearchHandle,
                   shutdown: impl Future<Output = ()> + Send + 'static) -> Result<(), WebError>;
```

**Routes:**

| Route | Purpose |
|---|---|
| `GET /` | search box and index statistics, from an in-memory snapshot refreshed by one background task at most every 60 s; requests never trigger counts |
| `GET /search?q=&p=&sort=` | results |
| `GET /t/{key}` | torrent detail; 40-hex DHT or v1 key, or 64-hex v2 |
| `GET /api/v1/search`, `GET /api/v1/torrents/{key}` | JSON API |
| `GET /about`, `/privacy` | information pages |
| `GET /robots.txt`, `/.well-known/security.txt` | crawler and security contact files |
| `GET /static/style.css` | stylesheet, embedded in the binary |
| `GET /healthz`, `GET /readyz` | liveness and readiness |

**Search query cache.** `GET /search` and `GET /api/v1/search` share a
server-side cache of raw index results, keyed by `(text, sort, page, per_page)`
— page and sort variants are separate index queries, so they are separate
entries. Only non-empty successes are stored; hydration (`get_many` +
`order_page`) still runs on every request, so deletions and the seeder boost
stay fresh. Entries expire after `web.search_cache_ttl_secs` (fixed from
insert: hits refresh LRU recency, never expiry), the least-recently-used entry
is evicted past `web.search_cache_size`, and any index commit clears the whole
cache — the stamp is the open segment set plus delete opstamps (§11), not the
promotion-only generation number, so intra-generation commits invalidate on the
next request. Concurrent identical misses share one index search (singleflight).
Either knob at zero disables the cache. HTTP stays `Cache-Control: no-store`:
the cache is server-side only. Memory is roughly 1–2 KB per entry, so the
100-entry default is ≈ 200 KB and the 10 000-entry cap ≈ 20 MB.

**Rendering:**
- askama with auto-escaping. `|safe` is never used.
- Names are wrapped in `<bdi>`.
- JSON is built with `serde_json`.
- Magnet links are built server-side.

**Security headers** on every response:
- `Content-Security-Policy: default-src 'none'; style-src 'self'; img-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'`
- `X-Content-Type-Options: nosniff`
- `Referrer-Policy: no-referrer`
- `Permissions-Policy: camera=(), microphone=(), geolocation=()`
- `Cross-Origin-Opener-Policy: same-origin`
- `Cross-Origin-Resource-Policy: same-origin`
- `X-Frame-Options: DENY`
- `Strict-Transport-Security` when `web.hsts`.

No `Server` header is sent.

**Client address.**
- It is the socket peer, unless the peer is in `web.trusted_proxies`.
- In that case, all `X-Forwarded-For` header lines are joined in order and read from **right to left**, and the first entry not in `web.trusted_proxies` is used.
- An unparseable entry stops the scan, and the last trusted address is used. The scan never skips an entry.
- The leftmost-entry approach (tower_governor's `SmartIpKeyExtractor`) must not be used.
- `check-config` warns when `trusted_proxies` is empty but requests arrive from private addresses.
- The shipped compose file trusts the `frontend` network's subnet.

**Rate limits.**
- Buckets per IPv4 /32, and per IPv6 /64, /56 and /48 **at the same time**. A request needs a token from every bucket that applies.
- Pages: 3/s, burst 30. API: 2/s, burst 20.
- The map is bounded (§3). When it is full, new clients share one overflow bucket instead of evicting live entries.
- A rate-limited request gets 429 with `Retry-After`.

**Rate-limit scaling.** The class quota applies to an IPv4 /32 and an IPv6 /64. An IPv6
/56 bucket gets 4 × the quota, a /48 bucket 16 ×, and a /32 bucket 64 ×. Without this, the wider buckets
(charged by every request in them) would make the narrower ones meaningless.

**Other limits.** These are constants, not configuration:
- request timeout 10 s, answered with 503;
- body limits (§3), with a declared oversized body refused before any handler runs;
- path plus query ≤ 4 KiB (414);
- at most 512 requests in flight (503 with `Retry-After: 1`);
- at most 768 open connections, with idle connections closed after 150 s;
- at most 16 concurrent detail lookups;
- detail listings capped at 128 000 path characters.

**Second checks.** Names and paths are sanitised again before display.

**JSON.** API JSON escapes `<`, `>`, `&`, `'`, U+2028 and U+2029, so it is safe to embed
in HTML.

**CSRF.** There are no POST endpoints; every request is a GET.

**Logging.** The application logs method, route template, status and latency only.
**The reverse proxy must log no more.** The shipped proxy example (`deploy/Caddyfile`)
has no access log. For nginx, use a format without `$remote_addr` or `$request`, and set
`error_log … crit`. See 04 §2.

## 13. dhtcrawler3 (the binary) (R19, R20)

```
dhtcrawler3 [--config FILE] <COMMAND>
  migrate | crawl | index [--rebuild] | web | all
  all [--migrate]
  stats | check-config
  healthcheck <URL>            # GET URL, exit 0 on 2xx (the distroless image has no curl)
```

**Configuration.** `deploy/config/dhtcrawler3.toml` is authoritative. It has these sections:
- `[database]`: `host`, `port`, `name`, `user`, `password_file`, `max_connections`, and an optional `url` that overrides host, port and name.
- `[database.crawler]`, `[database.indexer]`, `[database.web]`: optional `user` and `password_file` overrides, used by `all`.
- `[crawl]` (including `max_pending` and `max_inflight_metadata_bytes`).
- `[index]`.
- `[web]` (search cache: `search_cache_size`, `search_cache_ttl_secs`).
- `[metrics]` (`listen`).
- `[log]`.

Unknown keys are an error. Environment variables `DC3_<SECTION>__<KEY>` override the file.
List values are comma-separated.

**Admission (crawl):**
1. `Discovered` events pass through the address chokepoint for `peer`.
2. A peer that passes is kept in the hint map.
3. The key is checked against the dedup set. `GetPeers`-only keys also need the source-diversity rule (§3).
4. A batch is observed every second or every 1 000 keys.
5. **A key enters the dedup set only after the observation transaction that recorded it commits.** A failed batch is retried with backoff.

**Fetch workers.** There are `crawl.fetch_workers` workers (default 64). Each worker:
1. `claim(1, 120 s)`, renewing the lease if it still holds the item after 90 s;
2. collects peers from the hint map and from `dht.get_peers` (15 s), passing every peer through the chokepoint, rejecting our own addresses, and applying the per-destination limits (§3);
3. tries up to 8 peers, 3 at a time, within the connection semaphore and the byte budget;
4. runs `verify`, then `parse_info_visit`;
5. finishes with `complete` or `fail`, or gives up on BEP 27 private torrents.

**Metrics and health.**
- Each role serves `/metrics`, `/healthz` (the process is alive) and `/readyz` on `metrics.listen`.
- Readiness means:
  - **crawl:** the database is reachable and there are ≥ 8 good routing nodes;
  - **index:** lag is under 5 min;
  - **web:** the database is reachable and the index is open.
- Metric names are in [04-operations §6](04-operations.md#6-metrics). DHT and discovery metrics carry a `family` label.

**End-to-end tests** (in `crates/dhtcrawler3/tests/`):
1. **No database or network.** 16 DHT nodes on `127.0.0.1` with `allow_private_addrs` and `tuning.limits_by_endpoint`, plus the test seeder. The crawler discovers a key by BEP 51, finds the seeder, and fetches, verifies and parses the torrent.
2. **With `DATABASE_URL` set.** Discovery → pending → fetch → torrents → index → web search.

## 14. Deployment and supply chain (R12, R13)

**Build and image.**
- `rust-toolchain.toml` pins 1.98.1. `Cargo.lock` is committed, and every build uses `--locked`.
- The binary is **self-contained**: it links dynamically only against glibc.
- The runtime image is `gcr.io/distroless/cc-debian13:nonroot`. Base images are pinned by digest.
- Build paths are remapped so artifacts do not embed the builder's paths.

**`deploy/docker-compose.yml`:**
- `postgres:18.6` on an internal network.
- A one-shot `migrate` service that finishes before `crawl`, `index` and `web` start.
- `crawl`, which publishes UDP 6881 on IPv4 and IPv6. Its `egress` network has `enable_ipv6: true`.
- `index`.
- `web`, on `127.0.0.1:8080`.
- Every role: read-only root filesystem, no capabilities, `no-new-privileges`, memory and pids limits, secrets as files.
- Every role has a healthcheck. Metrics listen on the internal `backend` network only.
- `deploy/Caddyfile` is an example TLS proxy with no access log.

**CI** (`.github/workflows/ci.yml`):
- `fmt`, `clippy -D warnings`, and `test` with PostgreSQL;
- `cargo deny` and `cargo audit`;
- an image build with SBOM and provenance;
- a 60 s fuzz smoke run for each target: `bencode_decode`, `krpc_message`, `compact_lists`, `peer_frames`, `parse_info`, `search_query`.

Actions are pinned to commit SHAs, and permissions are read-only. Dependabot covers cargo, GitHub Actions and Docker.

**Release** (`.github/workflows/release.yml`, on a version tag):
- builds the image and binaries;
- publishes them to GHCR with GitHub artifact attestations (build provenance) and an SPDX SBOM.

Operators verify with `gh attestation verify` (04 §7).

**Repository hygiene.** Nothing in the repository is a compiled binary.

## 15. Deliberately deferred

- MSE/PE and uTP for metadata fetches.
- BEP 33 scrape.
- Torznab.
- A classifier.
- An onion service.
- An admin web UI.
- Automated second-builder reproducibility comparison.
- Low-severity review items deferred on 2026-09-17:
  - `want` is sent on every dual-stack query, not only while bootstrapping;
  - the IPv6 socket binds `[::]` rather than a chosen global address;
  - intervals above 21 600 s are capped;
  - 6to4, Teredo and NAT64 forms of one IPv4 host count as different hosts;
  - per-IP spacing is reserved before the global budget wait;
  - our own addresses are unknown until the external-IP vote completes;
  - hybrid v1/v2 file lists are not cross-checked for consistency.

## 16. Review record

An independent review on 2026-09-16 raised these points. All were adopted above:
- proxy logging;
- a single address chokepoint with per-destination caps;
- `X-Forwarded-For` parsing and multi-prefix rate limits;
- `get_peers` reply trimming;
- non-extended peer frames;
- test addressing;
- the self-contained binary wording;
- release attestation;
- request body size;
- web role privileges;
- change-feed lock ordering and timeouts;
- the sampler's visited-map eviction;
- `get_peers`-only admission and the responder budget;
- external-IP voting;
- index generations;
- `complete` key semantics;
- IPv6 enabled by default;
- fuzz targets;
- the in-flight byte budget;
- CJK unigrams;
- named limits;
- measurable criteria;
- dedup after commit;
- the cached home-page statistics;
- per-role health;
- lease renewal;
- backoff wording.

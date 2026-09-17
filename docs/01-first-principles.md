# 01 — First principles: from definitions to requirements

This document takes the definitions in [00-horismos](00-horismos.md) and derives, step
by step, what dhtcrawler3 must do and must not do. Each requirement cites the axioms
it follows from, so every design choice in [03-design](03-design.md) can be traced
back to a reason.

## 1. The four causes of the system

Aristotle explains a made thing through four causes. Applied to a DHT search engine:

| Cause | Question | Answer for dhtcrawler3 |
|---|---|---|
| **Final** (*telos*) | What is it for? | To let a person find, **by words**, the DHT keys of torrents that exist in the Mainline DHT, so a BitTorrent client can obtain them. |
| **Formal** (*eidos*) | What is its structure? | A pipeline: discover keys → queue → fetch metadata → verify → parse → filter → store → index → answer queries. |
| **Material** (*hylē*) | What is it made of? | dhtcrawler2: Erlang R16, MongoDB 2.4, Sphinx/coreseek, third-party HTTP torrent caches. dhtcrawler3: Rust, PostgreSQL 18, Tantivy, Linux containers. |
| **Efficient** (*kinoun*) | What brings it about? | The operator who deploys it; its three roles (crawl, index, web); the DHT nodes and peers that answer it. |

"Update" and "version" (§8 of 00) mean the **final** cause is kept while the
**material** cause may change completely. Nothing requires reusing Erlang code. The
function is what has to survive.

## 2. Axioms

These are the starting propositions. Each is either true by the definitions in 00 or
an observed fact with its evidence cited.

| # | Axiom | Basis |
|---|---|---|
| **A1** | **Function.** The system exists to perform the final cause above. A part that does not serve it is unnecessary. | Definition of *function* and *better* (00 §8). |
| **A2** | **Strangers write the input.** Every DHT packet, peer message, torrent name, file path and HTTP request comes from parties the operator does not know. | Anyone can publish a torrent with any name, or send any UDP packet (00 §9, *untrusted input*). |
| **A3** | **Reciprocity.** The DHT is a shared resource that other people's computers provide. Lasting access depends on following its rules. | BEP 5 duties; libtorrent bans nodes that send more than 5 packets/s; BEP 42 deprioritises spoofed IDs; BEP 51's rationale says passive `get_peers` harvesting "incentivizes bad behavior such as spoofing node IDs and attempting to pollute other nodes' routing tables" (00 §4). |
| **A4** | **Minimality.** Data that is not stored cannot leak. Code that is not run cannot be exploited. A dependency that is not taken cannot be subverted. | Logic. |
| **A5** | **Verifiability.** A component that cannot be checked cannot be trusted. | dhtcrawler2 shipped precompiled `.beam` files, and DLLs with no source. The `.beam` files could be cleared only by extracting their embedded debug information and comparing it with the source, which few users would ever do (02 §4). |
| **A6** | **Finitude.** Every resource (memory, stack, CPU, sockets, disk, bandwidth, a neighbour's patience) is finite, so any unbounded demand can exhaust it. | Logic. dhtcrawler2 has an unbounded gunzip, a recursive bencode parser with no depth limit, an unbounded announce peer store, and atoms created from file keys (02 §3). |
| **A7** | **Responsibility.** The operator acts under law and answers for what the site shows and stores. | 17 U.S.C. §512(d); 18 U.S.C. §2258A; CJEU C-610/15 *Stichting Brein v Ziggo* (2017: indexing torrent metadata can itself be a communication to the public); CJEU C-582/14 *Breyer* and the GDPR (00 §9). |
| **A8** | **Failure is normal.** Processes crash, peers vanish, networks drop packets. Work that cannot resume after a failure is lost. | dhtcrawler2 deletes a queue item *before* processing it, so any crash loses that item (02 §3). |
| **A9** | **Observation precedes correction.** A fault that is not measured cannot be found or fixed. | Logic. |

## 3. What the function needs, and nothing more

From **A1**, each of these capabilities is **necessary**. Remove any one and a person can
no longer find torrents by words. (They are called *capabilities* to keep them apart
from the system's one *function*, its final cause.)

| # | Necessary capability | Why it is necessary |
|---|---|---|
| F1 | **Discover** DHT keys | Nothing can be found that is not first known. |
| F2 | **Obtain** each key's metadata | A key carries no words. The words (name, file paths) are in the metadata. |
| F3 | **Verify** the metadata | Without it, whoever answers can attach any words to any key (A2). |
| F4 | **Store** the metadata durably | Without it, the index is lost on restart (A8). |
| F5 | **Index** the words | Scanning tens of millions of records per query is too slow (definition of *index*). |
| F6 | **Answer** word queries | This is the purpose itself. |
| F7 | **Present** a magnet link | This is what lets the person act on the result. |

By **A1 + A4**, these are **not** needed and are therefore excluded: downloading
content, thumbnails or previews, storing peer lists, user accounts, comments,
advertising, analytics, JavaScript in pages, curated "top movies" lists, and any
third-party web service at runtime. The only outside dependencies are the DHT itself
(including several independent bootstrap nodes, used only while the routing table is
under-filled) and DNS to resolve them.

Two capabilities are not part of the final cause but are required by other axioms:

| # | Capability | Required by |
|---|---|---|
| F8 | **Govern**: refuse denied keys and terms, accept reports, process takedowns | A7 |
| F9 | **Observe**: expose metrics and health | A8, A9 |

## 4. Derived requirements

"Legacy" is what dhtcrawler2 does, with evidence in [02-legacy-audit](02-legacy-audit.md).

| # | Requirement | From | Legacy |
|---|---|---|---|
| **R1** | Discover keys mainly with **BEP 51** `sample_infohashes`, from **one** honest node per IP address with a **BEP 42** node ID. Also record keys from announces and lookups the node legitimately receives. Never spoof IDs or run Sybil fleets. | A1, A3 | 50 static IDs spread evenly across the key space on 50 ports (a horizontal Sybil); no BEP 51 or 42. |
| **R2** | Be a **compliant BEP 5 node**: answer `ping`, `find_node` (by `target`), `get_peers` (with real tokens), `announce_peer` (checking tokens) and `sample_infohashes`, and send KRPC errors. | A3 | Tokens are 32 zero bytes and always accepted; `find_node` answers about the sender instead of the target. |
| **R3** | Respect each node's `interval`. Keep per-node send rates well under 5 packets/s. Enforce a global packets-per-second budget. Do no reverse DNS. Use several bootstrap nodes. | A3, A6 | No outbound limits; one hard-coded bootstrap node; a DNS failure crashes the node. |
| **R4** | Support **IPv4 and IPv6** (BEP 32) and **v1, v2 and hybrid** torrents (BEP 52). | A1 | IPv4 only; v1 only. |
| **R5** | Obtain metadata **only from peers** (BEP 9/10). Never fetch from third-party caches. | A1, A2, A4 | Only plain-HTTP fetches from `torcache.net`, `torrage.com` and `bt.box.n0808.com`, none of which serves torrents any more: `torcache.net` is parked (HTTP 410), `torrage.com` returns 404, and `bt.box.n0808.com` returns HTTP 400. |
| **R6** | **Verify** metadata (SHA-1 or truncated SHA-256 equals the key) **before** parsing its contents. | A2, A5 | Never verified. |
| **R7** | **Bound every parser and buffer**: datagram size, message length, `metadata_size`, bencode depth, string length, element count, file count, name length. Return errors; never panic on input. Fuzz the parsers. | A2, A6 | Recursive, unbounded bencode; unbounded gunzip; unbounded peer message length (in the unused BEP 9 prototype). |
| **R8** | Treat all metadata text as hostile. Decode legacy encodings, normalise (NFC), strip control and bidi characters, cap lengths, sanitise path components (`.`, `..`, empty). | A2 | Raw bytes stored and displayed. |
| **R9** | **Escape all output** with an auto-escaping template engine. Build JSON with a serializer. Send a strict CSP and **no JavaScript**. | A2, A4 | Stored XSS (torrent names) and reflected XSS (search keyword); JSON built by string concatenation. |
| **R10** | Load **no third-party** scripts, styles, fonts, analytics or ads. | A4 | (btdig.com loads Histats and reCAPTCHA.) |
| **R11** | **Never store peer IPs.** Do not log a visitor's IP with their query, in the application **or** in the reverse proxy the deployment ships. Keep rate-limiter state in memory only. Persist no IP addresses except at most 300 DHT routing contacts (no timestamps, no link to any key). | A4, A7 | Visitor IP and query written to a log file on every search. |
| **R12** | **Minimal exposure.** The database is reachable only on an internal network, with authentication. The web server binds to localhost by default and sits behind a reverse proxy. Metrics use a separate internal listener. Each process gets least-privilege database credentials. | A4, A6 | MongoDB without auth; HTTP on `0.0.0.0` with directory listing; replica-set key committed to the repo. |
| **R13** | **Verifiable supply chain.** Build only from source. Commit the lockfile and build with `--locked`. Pin the toolchain and base images. Commit no binaries. Run `cargo deny` and `cargo audit` in CI. Build container images as non-root with a read-only root filesystem. | A5 | Precompiled binaries committed; dependencies at git `HEAD`; DLLs shipped without source. |
| **R14** | Use a **memory-safe language** for everything that parses network input. | A2, A5, A6 | Erlang is memory-safe, but the optional rmmseg NIF (a Windows C++ DLL, used only with the non-default `text_seg=rmmseg`) would run in-process on attacker-controlled names. |
| **R15** | **Durable, resumable processing.** Use a database queue with leases and exponential backoff, idempotent upserts keyed by infohash, and a search index checkpoint committed atomically with the index. | A8 | Delete-before-process queue; lost hashes; Sphinx checkpoint not aligned with flushes. |
| **R16** | **The index is a derived projection** of PostgreSQL. It can always be deleted and rebuilt. | A8, A5 | Sphinx doc IDs were a separate counter, so the index could not be reliably rebuilt. |
| **R17** | **Multilingual search**: CJK without dictionaries (bigrams), file-name search, prefix matching, BM25 ranking, bounded queries (length, terms, page depth, time). | A1, A6 | Mongo `text` command (removed in later MongoDB versions), or an "all substrings" split that grows as the cube of name length. |
| **R18** | **Governance.** Enforce a denylist of keys at ingest, at indexing and at query time. Filter CSAM terms at ingest (name and every path), at indexing, and at query time, matching whole tokens after NFKC, case folding and confusable folding. Do not index private torrents. Provide a report form and a takedown CLI, and keep an audit trail. | A7 | None. |
| **R19** | **Observability.** Expose Prometheus metrics per pipeline stage, liveness and readiness endpoints for every role, and structured logs without personal data. | A8, A9 | Text stats files, with rates in the wrong units. |
| **R20** | **Operable.** One self-contained binary (dynamically linked only against glibc) with subcommands, one config file with environment overrides, `docker compose up`, automatic migrations, and documented limits. | A1, A8 | Windows `.bat` launchers and hand-edited Erlang term files. |

## 5. What "better" means, measurably

By the definition of *better* (00 §8), dhtcrawler3 is a better version of
dhtcrawler2 when it performs the same function more fully and more reliably, and does
less harm. Each claim below names the test or metric that checks it. Metric names are
defined in [04-operations §6](04-operations.md#6-metrics).

**More fully** (covers more of the function)

| Claim | Checked by |
|---|---|
| Discovers keys over IPv4 **and** IPv6 using BEP 51, which surveys the DHT instead of waiting for traffic. | `dc3_discovered_total{source="sample",family}` > 0 for both families within 1 h of start. |
| Indexes v1, v2 and hybrid torrents. | `dc3-torrent` tests; the end-to-end test. |
| Finds CJK names by any substring (single characters included), and finds file names and prefixes. | `dc3-search` tests. |
| Does not depend on any third-party cache that can disappear. All three of dhtcrawler2's caches no longer serve torrents. | There is no HTTP client in the crawl path (code review). |

**More reliably** (performs it with fewer failures)

| Claim | Checked by |
|---|---|
| A crash loses no work already in the `pending` queue, because leases expire and the work is retried. At most the last second of in-memory discoveries is lost. | `dc3-store` lease tests. |
| No input crashes a process. Parsers are bounded, fuzzed and property-tested. | proptests in every parser crate; the CI fuzz smoke run finds no crash. |
| The search index can be rebuilt from the database at any time, while search keeps serving. | `index --rebuild` generation test. |
| Queries have hard limits on length, words, page depth and time. | `dc3-search` and `dc3-web` limit tests. |
| The whole pipeline is tested without touching the public network. | The end-to-end crawl on a private in-process DHT, including a peer that serves metadata over BEP 9 (03 §13). |
| Search stays fast and the index stays fresh. | p95 of `dc3_search_seconds` < 1 s at 10 M documents; `dc3_index_lag` < 60 s in steady state. |

**Less harm** (causes fewer bad effects)

| Claim | Checked by |
|---|---|
| Pages cannot carry XSS: output is auto-escaped, there is no JavaScript, and a strict CSP backs this up. | `dc3-web` hostile-name rendering tests; header tests. |
| No peer IP is ever written to disk. The only persisted IP addresses are at most 300 DHT routing contacts, which carry no timestamps and no link to any key; BEP 5 asks nodes to keep them. No visitor IP is stored with a query, either by the application or by the shipped reverse-proxy configuration. | Schema review; `deploy/Caddyfile`; log-format tests. |
| The node is a compliant DHT citizen: one BEP 42 identity per address, real tokens, rate limits, and respect for `interval`. | `dc3-dht` tests; `dc3_dht_sampler_early_total` stays 0. |
| Denied keys are refused at three points (ingest, indexing, query). CSAM terms are refused at ingest (name and every path), at indexing, and at query time. Extending the term list and running `policy rescan` removes existing matches. | `dc3-store`, `dc3-policy` and end-to-end tests. |
| Pages load no third-party code, so a compromised CDN or ad network cannot inject into them. | CSP header test (`default-src 'none'`). |
| Nothing in the repository is a binary that users must trust without being able to check it. Releases carry build attestations. | Repository contents; `gh attestation verify` (04 §7). |

## 6. Two decisions this derivation settles

**Rewrite rather than port.** Porting keeps the *material* cause, and the material is
what is broken:
- The HTTP caches no longer serve torrents.
- The MongoDB 2.4 APIs the code uses (the `text` command, `$query`/`$orderby`, inserts into `system.indexes`, `OP_QUERY`) have been removed from MongoDB.
- The Sphinx fork (coreseek) is tied to an index build pipeline that parses console output.
- R16B-era Erlang uses deprecated APIs (`erlang:now/0` and the `random` module). They still exist but emit warnings, and the build sets `fail_on_warning`.
- The DHT library breaks BEP 5 in several places.

The *formal* ideas that are sound are kept and listed in 02 §5:
- separate discovery from resolution;
- merge duplicate sightings into a popularity count;
- key everything by infohash;
- treat the index as a derived projection;
- index file names as well as torrent names.

**Rust.** R7 and R14 call for a memory-safe language with good fuzzing tools, and R13
and R20 call for a single self-contained binary with a small supply chain. Rust meets
all of these. What distinguishes it from the alternative that meets them equally, Go
(bitmagnet; Bleve for in-process search), comes down to two facts:
- **Search:** Tantivy provides BM25, positional phrase queries and custom tokenizers in-process, so no second datastore service is needed (R12).
- **Memory safety:** `#![forbid(unsafe_code)]` lets the project show that its own network parsers are memory-safe (R7, R14).

None of the published Rust DHT crates we checked (mainline 8.0, librqbit-dht 9.0,
dht-crawler 0.2) implements BEP 51. The KRPC layer is therefore written in-house, on top
of a bounded bencode parser that the project owns.

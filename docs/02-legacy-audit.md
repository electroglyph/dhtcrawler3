# 02 — Audit of dhtcrawler2 and of what happened to btdig

Method, 2026-09-16:
- Severity follows the likely harm to an operator or visitor if the defect were exploited or triggered, and how easily. **High**: a remote party can compromise the host, the data or visitors with little effort. **Medium**: real harm, but it needs specific conditions or gives limited impact. **Low**: minor harm or hard to reach.
- Five independent readers each read one subsystem in full. Sources were the upstream `src` branch (`82f14c4`, 2013-08-24), the `kevinlynx/kdht` DHT library, and this repo's `master` branch, which holds precompiled `.beam` files, launch scripts, templates and data files, but no Erlang source.
- A separate skeptic re-checked every defect rated critical or high against the code. Severities below are the skeptics' **corrected** ratings.
- Five research tracks checked the btdig incident, the BEP specifications, comparable projects, operating requirements and the Rust ecosystem. A fact-checker re-verified each track.

## 1. What dhtcrawler2 is

Four Erlang programs share one MongoDB 2.4 server that runs **without authentication**:

1. **crawler.** Runs 50 `kdht` DHT nodes on UDP ports 6776–6825. Their node IDs are spread evenly across the key space and saved, so they stay fixed across restarts. It records the infohash of every `get_peers` query it receives, merges duplicates in memory, and writes them to `dht_hash.hash` with a request count. It only counts `announce_peer` messages and discards their infohashes.
2. **hash_reader.** Takes one hash at a time off the queue, **deleting it before processing**. If the hash is already known, it increments a popularity counter. Otherwise it downloads the `.torrent` over **plain HTTP** from three third-party caches (`torcache.net`, `bt.box.n0808.com`, `torrage.com`), gunzips and parses the file, and stores name, size and file list in `torrents.hashes`.
3. **http front end.** OTP `inets` on `0.0.0.0:8000`, using `mod_esi` for pages and a JSON API. HTML is built with `io_lib:format("~s")` and **nothing is escaped**. Search uses the MongoDB 2.4 `text` command, or optionally Sphinx/coreseek.
4. **Optional tools.** A Sphinx index builder, a torrage "sync list" mirror, and local `.torrent` importers.

The BEP 9 peer-metadata code (`src/bt/`) is an unfinished prototype that nothing calls.
The system has **never fetched metadata from peers.**

## 2. What happened to btdig

**Verified (reproduced 2026-09-16 from one Canadian connection against both backend IPs; checks without the full browser header set saw no redirect):**
- Since about **2026-07-10**, `btdig.com` sends some browser-like visitors (users report that it varies by region) to scam and ad domains (`thoseeducation.com`, `stolidityseashellshotter.com`). It does this with a `Refresh` header, a meta refresh and a JavaScript redirect all at once, and adds an extra `x-server: p20` header.
- The redirect is **cloaked**. It fires only when the request carries a browser User-Agent together with `Accept: text/html` and `Sec-Fetch-Dest: document`. `curl` sees the normal page.
- The registrar record was updated on 2026-07-10. The domain was not transferred or re-registered (Tucows, created 2007, locked).
- Around the same time, the site dropped English and switched its default language to Korean.
- Earlier incidents:
  - keyword-triggered redirects to adult, ad and FBI pages (2023–2025);
  - "Welcome to nginx" pages, and a WordPress-style "Error establishing a database connection" page of unknown cause, in some regions;
  - redirects of Jackett traffic to `domflow.it` (2021–2023).
- The homepage still loads a third-party Histats analytics script (seen 2026-09-16).
- UK ISPs block btdig.com under a 2014 court order. No domain seizure was found.
- The `btdig/dhtcrawler2` fork differs from Kevin Lynx's upstream only in its README. Its only publicly listed member account now returns 404 (possibly suspended), and a user comment says nobody can moderate its issue tracker.

**Not established:**
- Whether the redirects were **injected by an attacker** or **added by the operator**. The evidence points both ways, and the operator has said nothing.
- Any compromise of **third-party** sites or self-hosted dhtcrawler2 instances. None was found.
- What btdig.com runs. Its stack is not public. One user says the public source is "very outdated compared to what's actually on btdig.com", which suggests at most a heavily changed descendant. The site's "fork me" ribbon links to this fork, but that proves nothing about what it runs.

**What this means for the design:** whoever is responsible, btdig.com now **acts
against its visitors**. The durable lessons are the controls that would have made this
impossible, or at least visible:
- no third-party script;
- a strict CSP with no inline script;
- signed, reproducible deployments;
- registrar and registry locks, DNSSEC, and hardware-key MFA on the registrar account;
- synthetic checks from several regions that send real `Sec-Fetch-*` headers.

The first two are built into dhtcrawler3. The third is built in as source-only, locked,
digest-pinned builds, with GitHub build attestations on releases (03 §14). An automated
second-builder reproducibility check is future work. The last two are operator tasks,
listed in the [deployment guide](04-operations.md).

## 3. Verified defects in dhtcrawler2

### Security

| Sev. | Defect | Where |
|---|---|---|
| High | **Stored XSS.** Torrent names and file names (attacker-chosen) are written into HTML unescaped. | `http_handler.erl:157-183` |
| High | **MongoDB with no authentication.** The driver has no auth code (`TODO: add auth/2`), and the documented `mongod` command sets no `--auth` or `--bind_ip`. | `README.md:18`, `deps/mongodb` |
| High | **Metadata over plain HTTP from domains that no longer serve it, never verified.** `torcache.net` is parked, `torrage.com` returns 404 and redirects its root elsewhere, and `zoink.it` (commented out) was re-registered in 2015 by a registrant named "EZCLOUD LIMITED", the company name linked to the 2015 EZTV takeover. Anyone who controls one of these hosts, or sits on the network path, can supply content that is gunzipped, parsed and shown to visitors. | `tor_download.erl:177-192` |
| Medium | **Reflected XSS.** The search keyword is echoed unescaped into the search box's `value` attribute, the `<h4>` heading and, in Sphinx mode, the page links. | `page.temp:61`, `http_handler.erl:106,118,146` |
| Medium | Metadata is never checked against the infohash. | `hash_download.erl:126` |
| Medium | Unbounded `zlib:gunzip` of network responses (decompression bomb). | `tor_download.erl:162` |
| Medium | JSON built by string concatenation, with no escaping. | `api.erl:110` |
| Medium | Replica-set keyFile secret committed (identical in `key1.txt` and `key2.txt`). | `tools/db-replset/` |
| Medium | Dependencies pinned to git `HEAD` (four over SSH, `giza` over HTTPS), with no lockfile. `giza` is a personal fork; `kdht` is the author's own library. | `rebar.config:3-8` |
| Medium | `api.erl` uses `export_all`, so every 2- and 3-argument function is reachable as `/e/api:<fn>`. | `api.erl:12` |
| Medium | DHT tokens are always 32 zero bytes and always accepted. | `kdht dht_net.erl:343-347` |
| Medium | BSON decoding turns field names into atoms, and file entries are keyed `file1…fileN`, so the atom table grows toward exhaustion. | `db_store_mongo.erl:207` |
| Medium | Unescaped CDATA lets torrent names break the Sphinx xmlpipe2 XML. | `sphinx_doc.erl:71` |
| Medium | Ships opaque precompiled binaries for users to download and run, with no signatures (see §4). | `README.md:9` |
| Medium | Expensive uncached endpoints reachable without authentication (`top/3` sorts the whole collection on an unindexed field). | `http_handler.erl:37` |
| Medium | The kdht bencode decoder is recursive with no depth or size limits; the announce peer store is unbounded, with one timer per infohash. | `kdht bencode.erl`, `kdht storage.erl:64` |
| Low | `binary_to_term` without `[safe]` on the state file; `$where` JavaScript selector built by string interpolation (not reachable from HTTP); TLS options without verification (unused). | various |
| Low | Every search writes the visitor's IP and keyword to a plaintext log. | `http_handler.erl:29` |

### Correctness and reliability

| Sev. | Defect | Where |
|---|---|---|
| High | `now_day_seconds` crashes once per leap year and is not a real Unix epoch. Dates from 2014 on are off by one day about half the time. | `time_util.erl:29` |
| High | A stale downloader PID turns any single reader crash into a shutdown of the whole `hash_reader` tree. | `tor_download_stats.erl:67` |
| Medium | The queue deletes items before processing them. Failures are lost, with no retry or backoff. | `hash_reader_common.erl:26` |
| Medium | Only the first 1 MiB streamed chunk is read, and the HTTP status is ignored. | `tor_download.erl:85` |
| Medium | Insert replaces the whole document, resetting popularity and creation time. | `db_store_mongo.erl:122` |
| Medium | "Simple" segmentation stores every substring, which grows as the cube of name length. A name run with no separators of about 318 CJK characters (or about 453 ASCII letters and digits) exceeds MongoDB's 16 MB document limit. | `string_split.erl:22` |
| Medium | The Sphinx loader permanently skips torrents that share a `created_at` second. | `sphinx_torrent.erl:74` |
| Medium | A single bootstrap node. A DNS failure crashes the node; an unreachable host causes a tight retry loop. | `kdht dht_state.erl:221` |
| Medium | Transaction-ID matching breaks after 65 536 queries on one node. | `kdht dht_net.erl:329` |
| Medium | `get_peers` responses with more than one peer value are mishandled. | `kdht dht_net.erl:97` |
| Medium | A full bucket drops new nodes without trying to replace bad ones. | `kdht bucket.erl:115` |
| Low | `find_node` answers about the sender's ID instead of `target`. | `kdht dht_net.erl:203` |
| Low | No v2 torrents, no encoding handling beyond `*.utf-8` keys, multi-file size stored as 0. | `torrent_file.erl` |

## 4. Were the shipped binaries tampered with?

**No evidence of tampering.** The auditor extracted the abstract code from every
`.beam` file on `master` (`beam_lib`, debug_info) and compared it with the `src`
branch:
- Every module that has source is semantically identical to that source.
- The dependency `.beam` files match their public upstream commits.
- Three modules ship with no source: `transfer`, `tor_location` and `sphinx_builder2`. They were decompiled and are older, benign versions of existing tools.
- The Windows DLLs `rmmseg_win32.dll` (sha256 `101091a2…c596`) and `rmmseg_win64.dll` (sha256 `95156db8…2cd0`) have no source in either branch. Their PDB paths point to a local build of rmmseg-cpp, so these exact binaries cannot be audited.

The problem is the **distribution model**. Almost no user would have done this check,
and nothing prompted them to: there were no signatures and no reproducible build. A
future malicious push would have gone unnoticed. dhtcrawler3 removes all committed binaries
(R13).

## 5. Ideas worth keeping

- **Separate discovery from resolution** with a durable queue, so each stage fails and scales on its own.
- **Merge duplicate sightings** into a popularity count, so only unknown keys trigger network work.
- **Key everything by infohash**, which makes writes idempotent upserts.
- **Keep the search index as a projection** of the database, and **hydrate results from the database**, so a stale index affects only recall.
- **Index file names** as well as torrent names, and **hide padding files**.
- **Prefer `name.utf-8` and `path.utf-8`** when a torrent provides them.
- **A per-hash deadline** separate from per-connection timeouts.
- **Persist the node ID and a small set of contacts** for warm restarts.
- **Pluggable segmentation**, replaced with dictionary-free bigrams.

## 6. External names in the old code, and their status on 2026-09-16

| Name | Role in dhtcrawler2 | Status |
|---|---|---|
| `torcache.net` | primary `.torrent` source | Parked (ParkingCrew); returns 410 |
| `torrage.com` | `.torrent` source and sync lists | Behind Cloudflare; `/torrent` and `/sync` return 404; root redirects to `bayimg.com` |
| `bt.box.n0808.com` | `.torrent` source | Alibaba Cloud IP; returns openresty 400; owner unknown |
| `zoink.it` | disabled source | Re-registered 2015 by a registrant named "EZCLOUD LIMITED"; redirects to `eztvx.to` |
| `dht.transmissionbt.com:6881` | only bootstrap node | Live |
| `codemacro.com` | author's blog in README | For sale |
| `coreseek.cn` | Sphinx fork | Still registered to its original company; project activity not checked |
| rebar deps (`git@github.com:…`, `HEAD`) | build | `mongodb/mongodb-erlang` archived; `giza` is a personal fork; `kdht` is Kevin Lynx's own library |

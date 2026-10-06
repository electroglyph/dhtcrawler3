# dhtcrawler3

A secure, standards-compliant **BitTorrent DHT search engine**, written in Rust.

It joins the Mainline DHT as a well-behaved node and discovers torrents with BEP 51
`sample_infohashes`. It fetches each torrent's metadata directly from peers (BEP 9/10)
and verifies it against the infohash. It then stores the metadata in PostgreSQL and
serves a fast, multilingual search site that runs no JavaScript.

## New: BEP 33 seeder scrapes

Stored torrents are re-polled for their live seeder count, and dead swarms are
tombstoned instead of lingering in the index:

- **Scrape worker** (`[crawl]` `scrape_*` keys): claims due rows oldest-first,
  runs `scrape=1` traversals on a dedicated packet budget, and classifies each
  swarm as live (estimate above threshold), dead (aware zero over **both**
  families), or unknown (kept, never killed). Tombstones are conditional on the
  row being unchanged since the claim, so a concurrent fetch always wins.
- **Seeder display and ranking**: detail pages, search rows and the JSON API
  show fresh estimates; `sort=seeders` re-sorts a page by estimate and
  relevance order gains a sublinear seeder boost. Stale estimates are hidden
  and rank as if missing; the search index itself never churns on scrapes.
- **Liveness-ordered fetch queue**: fetch lookups send `scrape=1` and keep
  the free estimate for retry ordering (`pending.seeders_est`); unaware
  lookups leave `NULL`, never zero.
- **Removal memory**: tombstoned keys sit out admission on a 7 → 30 → 90-day
  escalating cooldown (a `seed=1` announce refreshes the scrape clock without
  a lookup instead of forcing a fetch loop).
- **Metrics**: `dc3_scrape_total{outcome}`, `dc3_scrape_zero_seeder_share`,
  `dc3_scrape_unaware_share`, `dc3_scrape_tombstones_total`,
  `dc3_purge_tombstoned_total`, `dc3_scrape_due_depth`,
  `dc3_removed_keys_count`.


dhtcrawler3 replaces [dhtcrawler2](https://github.com/kevinlynx/dhtcrawler2) (Erlang,
2013), the code published by the `btdig` GitHub organisation. It is a rewrite, not a
port. The old code downloaded metadata over plain HTTP from third-party caches that no
longer exist, and it never verified that metadata. It also rendered attacker-chosen
torrent names into pages without escaping, and ran MongoDB without authentication.
[docs/02-legacy-audit.md](docs/02-legacy-audit.md) has the full audit, including what was
verified about the 2026 redirect incident on btdig.com.

## How this was designed

The design is derived step by step, starting from definitions:

1. [**00 — Horismos**](docs/00-horismos.md): every term, defined by genus and differentia, down to stated primitives.
2. [**01 — First principles**](docs/01-first-principles.md): the system's four causes, nine axioms, and requirements R1–R20 derived from them, with a measurable meaning of "better".
3. [**02 — Legacy audit**](docs/02-legacy-audit.md): what dhtcrawler2 actually does, its verified defects, and what happened to btdig.
4. [**03 — Design**](docs/03-design.md): architecture, crate interfaces, schema, and every hard limit.
5. [**04 — Operations**](docs/04-operations.md): deployment, the pre-launch checklist, administration, and metrics.

## What makes it better

| | dhtcrawler2 | dhtcrawler3 |
|---|---|---|
| Discovery | 50 static node IDs listening passively (a Sybil fleet) | One BEP 42 node per address, BEP 51 sampling, full BEP 5 responder |
| Metadata | Plain HTTP from dead third-party caches, unverified | BEP 9 from peers, SHA-1 / SHA-256 verified before parsing |
| Protocols | IPv4, v1 torrents | IPv4 + IPv6 (BEP 32), v1 + v2 + hybrid (BEP 52) |
| Parsing | Recursive, unbounded | Iterative, bounded, fuzzed; memory-safe language |
| Web | Unescaped HTML (stored and reflected XSS) | Auto-escaped templates, no JavaScript, strict CSP |
| Search | MongoDB 2.4 text command or Sphinx | Embedded Tantivy, BM25, CJK bigrams, prefix and file-name search |
| Data | MongoDB without auth; visitor IPs logged with queries | PostgreSQL with least-privilege roles; no peer or visitor IPs stored |
| Reliability | Queue deleted before processing | Leased queue, backoff, idempotent writes, rebuildable index |
| Governance | None | Denylist, CSAM term filter, report form, takedown CLI, audit log |
| Supply chain | Precompiled binaries, dependencies at git `HEAD` | Source-only builds, lockfile, pinned images, cargo-deny/audit |

## Quick start

```sh
scripts/gen-secrets.sh
cd deploy && docker compose up -d --build
# then open http://127.0.0.1:8080
```

Read [docs/04-operations.md](docs/04-operations.md) **before** exposing an instance to
the internet.

## Development

The workspace is under `crates/`, and each crate has one job (see
[docs/03-design.md §2](docs/03-design.md#2-workspace)).

```sh
cargo test --workspace --locked          # needs a working native toolchain
scripts/cargo-docker.sh test --workspace # or run inside the pinned Rust image
```

Database tests need PostgreSQL. Set `DATABASE_URL` to a superuser connection; the tests
create their own throwaway databases.

## Security

See [SECURITY.md](SECURITY.md) to report a vulnerability.

## License

MIT. dhtcrawler3 is a new implementation, but it follows the design of Kevin Lynx's
dhtcrawler2, and his copyright notice is kept in [LICENSE.txt](LICENSE.txt).

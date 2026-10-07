# dhtcrawler4

A **BitTorrent DHT search engine**, written in Rust.

It joins Mainline DHT as a well-behaved node (BEP 5/32/42/43/51), discovers torrents with `sample_infohashes`, fetches metadata directly from peers (BEP 9/10, v1/v2/hybrid) with infohash verification, stores it in PostgreSQL, and serves multilingual search (embedded Tantivy, no JavaScript).

dhtcrawler4 is a fork of [dhtcrawler3](https://github.com/poonasor/dhtcrawler3) (itself a Rust rewrite of Kevin Lynx's 2013 Erlang dhtcrawler2, not a port). See [CHANGELOG.md](CHANGELOG.md) for the full fork diff.

## What changed since the dhtcrawler3 fork

- **Removed content moderation:** report form, CSRF, `dc3-policy` crate, blocked-terms list, denylist/block pages, report/denylist tables, policy fuzz target.
- **Added BEP 33 seeder scrapes:** bloom filter + estimator, `scrape=1` traversals on a dedicated budget, scrape worker with conditional tombstones, liveness-ordered fetch queue, seeder counts in web/API with `sort=seeders`, 7→30→90-day removal memory.
- **Hardened DHT / parsing / fetch:** stricter BEP 42 and routing-table rules, IPv6 `/48` rate limits, bounded iterative bencode, stricter torrent validation (padding, v2 tree, `pieces`), deadline + retry handling for metadata fetch.
- **Fixed search / store / web:** prefix-expansion budgets, index-total pagination, leased-queue and admission fail-closed fixes, Origin-checked POSTs, stricter `base_url`/config validation, secret redaction, least-privilege DB roles.
- **Added search query cache:** `GET /search` and `GET /api/v1/search` share a server-side cache of raw index results keyed by `(text, sort, page, per_page)` (`web.search_cache_size = 100`, `web.search_cache_ttl_secs = 3600`; either zero disables). Only non-empty successes are stored, entries expire on a fixed TTL, evict least-recently-used past capacity, and clear on any index commit; concurrent identical misses share one index search.
- **Perf + tests:** ~35 alloc/scan eliminations, `cargo fmt`, opt-in live-network tests, BEP 33 vectors, updated e2e coverage. See [docs/](docs/).

## License

MIT. dhtcrawler4 follows the design of Kevin Lynx's dhtcrawler2, and his copyright notice is kept in [LICENSE.txt](LICENSE.txt).

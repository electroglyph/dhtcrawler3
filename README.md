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


## License

MIT. dhtcrawler3 is a new implementation, but it follows the design of Kevin Lynx's
dhtcrawler2, and his copyright notice is kept in [LICENSE.txt](LICENSE.txt).

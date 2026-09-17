# 04 — Operating dhtcrawler3

This guide is for whoever deploys and runs an instance. The software implements
requirements R1–R20 ([01-first-principles](01-first-principles.md)). This guide covers
the parts only an operator can do.

## 1. Quick start (single host)

```sh
scripts/gen-secrets.sh                      # random DB passwords in deploy/secrets/
cd deploy && docker compose up -d --build   # db → migrate → crawl, index, web
docker compose logs -f crawl                # watch it join the DHT
open http://127.0.0.1:8080
```

What is exposed:
- **UDP 6881** is the only public port.
- **The web UI** listens on `127.0.0.1:8080`.
- **PostgreSQL** is on an internal Docker network with no published port.

Each role connects with its own database user:

| Role | Database access |
|---|---|
| `dc3_crawler` | reads and writes the torrent and queue tables; adds `csam-auto` and `private` denials |
| `dc3_indexer` | read-only |
| `dc3_web` | read-only, plus one function, `submit_report`, which stores a report and may auto-hide (bounded, audited) |
| `dc3_owner` | runs migrations and admin commands |

Resource guide: each 10 million indexed torrents need very roughly 40–80 GB of disk. The
crawler needs about 1 GB of RAM. PostgreSQL wants at least 2 GB.

## 2. Before exposing the site publicly

Work through this list before the site is reachable from the internet.

**Transport and proxy**
- [ ] Put a TLS-terminating reverse proxy in front of `127.0.0.1:8080`. `deploy/Caddyfile` is a working example.
- [ ] Set `web.base_url` to the public origin.
- [ ] Set `web.hsts = true` once the site is HTTPS-only.
- [ ] Keep `web.trusted_proxies` limited to the address the proxy's connections arrive from.
  - **Behind Docker's published port**, that is the `frontend` network's subnet, which the shipped compose file already trusts. It is not `127.0.0.1`.
  - If the rate-limit metrics show one client doing all the traffic, the setting is wrong.
  - `dhtcrawler3 check-config` warns about the common mistakes.
- [ ] **Proxy logging must not record visitors.** Search text travels in the URL, so an access log with the client address and the request line recreates the legacy privacy defect.
  - **Caddy:** omit the `log` directive, as the example does.
  - **nginx:** `log_format dc3 '$time_iso8601 $request_method $uri $status $request_time';` (`$uri` excludes the query string), `access_log … dc3;`, and `error_log … crit;`.

**Contacts and legal**
- [ ] Set `web.contact_email` and `web.dmca_agent`. Both are shown on `/legal` and in `/.well-known/security.txt`.
- [ ] **US:** register a DMCA designated agent with the U.S. Copyright Office. The registration expires after 3 years, so set a reminder.
- [ ] **EU/UK:** get legal advice on the DSA and the Online Safety Act. Designate points of contact, and a legal representative if needed.
- [ ] **CSAM:** register with NCMEC's CyberTipline (US), or the equivalent hotline where you operate. Decide who reviews reports, and how quickly.
- [ ] Extend the blocked-term list (`policy.terms_file`). The built-in seed is intentionally short. IWF members can use the IWF Keywords List. After any change to the list, run `policy rescan` (§3).
- [ ] Decide the CSAM auto-hide budget, `web.autohide_per_hour` (default 60). Reports beyond it are stored but do not hide anything until an admin acts.

**Domain and DNS.** The btdig.com incident is the reason for these steps; see [02-legacy-audit §2](02-legacy-audit.md#2-what-happened-to-btdig).
- [ ] Enable registrar lock **and** registry lock.
- [ ] Use hardware-key MFA on the registrar and DNS accounts.
- [ ] Enable DNSSEC.
- [ ] Add a CAA record.
- [ ] Make sure the in-zone NS records match the glue records.

**Monitoring.** Monitor the following and alert on changes:
- WHOIS, NS and DS records;
- certificate transparency logs for your domain;
- lookalike domain registrations.

**Synthetic checks against cloaking**
- [ ] From at least two countries or ASNs, fetch the home page and a search page every few minutes. Send real browser headers (`User-Agent`, `Accept: text/html`, `Sec-Fetch-Dest: document`, `Sec-Fetch-Mode: navigate`), and also fetch **without** them.
- [ ] Alert on any `Refresh` header, `<meta http-equiv=refresh>`, `<script>` tag, off-origin redirect, unexpected response header, or a difference between the two variants. dhtcrawler3 pages never contain script, so **any** script tag means compromise.

**IPv6**
- [ ] Docker must have IPv6 enabled for the `egress` network, which the compose file requests.
- [ ] After an hour, `dc3_dht_routing_nodes{family="v6"}` should be above 0. If the host has no global IPv6 address, the crawler logs a warning and runs IPv4-only.

**Never add**
- [ ] No third-party scripts, analytics, fonts or ads (R10). The CSP blocks them anyway. Keep it that way.

## 3. Day-to-day administration

All administration is done from the command line. There is no admin web UI to attack.

```sh
# inside the deployment
docker compose run --rm -e DC3_DATABASE__USER=dc3_owner \
  -e DC3_DATABASE__PASSWORD_FILE=/run/secrets/db_password migrate <command>
```

| Task | Command |
|---|---|
| Take down a torrent (DMCA, abuse) | `deny add <infohash> --reason dmca --note "notice #123"` |
| Undo a mistaken takedown | `deny remove <infohash>` |
| Review reports | `reports list --status open` |
| Act on a report | `reports resolve <id> --action deny` or `--action dismiss` |
| Overall numbers | `stats` |
| Re-check stored torrents after changing the term list | `policy rescan` |
| Rebuild the search index | see below |
| List takedowns | `deny list --limit 100` |
| Resolve a report with an explicit reason | `reports resolve <id> --action deny --reason dmca` |
| Run everything in one process (small installs) | `all --migrate` |

**Rebuilding the index.** Run the rebuild inside the index service, so it uses the real index volume:

```sh
docker compose stop index
docker compose run --rm index index --rebuild
docker compose start index
```

Search keeps working during a rebuild. The new index is built beside the old one, and
the web role switches over only when the new one has caught up. The rebuild refuses to
start if the index service is still running.

`deny add` does four things in one transaction:
- records the key;
- wipes the stored name and file list;
- removes the torrent from the queue;
- writes an audit-log entry.

The indexer then drops the torrent from search within seconds. The key stays blocked if
it is seen again.

A report with reason **csam** hides the torrent at once, pending review. Three limits apply:
- only the first open CSAM report for a torrent hides it;
- a torrent an admin has already reviewed and cleared is not hidden again automatically;
- auto-hides stop after `web.autohide_per_hour`, and the count is kept in `dc3_autohide_budget_exhausted_total`.

Every auto-hide writes an audit-log row. **Dismissing** the report un-hides the torrent
and marks it reviewed. **Denying** it removes the torrent. You must also report
apparent CSAM to your national hotline. dhtcrawler3 never downloads content, so the only
material to report is the torrent's metadata and infohash.

## 4. What is and is not stored

| Stored (PostgreSQL) | Never stored |
|---|---|
| DHT key, v1/v2 infohashes, sanitised name, file paths and sizes (first 2 000), sizes and counts, first/last seen time, sighting count | Peer IP addresses (kept in memory only while fetching, at most 45 min in the DHT peer store) |
| Queue of keys waiting for metadata, with retry state | Torrent content, `.torrent` files, trackers, comments |
| Denylist, reports (reason, message, optional contact), audit log | Visitor IP addresses (rate limits are in memory only) |
| Daily counters | Search queries (logs record route and status only) |

The crawl state file (`state_dir/dht-state.json`) holds our node IDs and a few hundred
DHT node addresses, so a restart does not hammer the bootstrap servers.

## 5. Being a good DHT citizen

The defaults are chosen so that other DHT users are not burdened:
- one node per address family, with a BEP 42 ID once the external IP is known;
- `sample_infohashes` intervals are respected;
- no more than 250 packets/s in total, and at least 1 s between queries to the same host;
- every standard query is answered.

If you raise `crawl.max_packets_per_sec`, raise it gradually and watch
`dc3_dht_timeouts_total`. A rising timeout rate usually means your own network (NAT
table, ISP) is the bottleneck. Home routers often cope poorly above a few hundred
packets per second.

Do **not** run many instances behind one IP to "go faster". It gives no real benefit,
because libtorrent keeps only one routing entry per IP, and it is the Sybil behaviour
BEP 51 was written to make unnecessary.

## 6. Metrics

Each role serves `/metrics`, `/healthz` (liveness) and `/readyz` (readiness) on `metrics.listen` (default
`127.0.0.1:9100`). In the compose deployment this listener binds to the internal
`backend` network only, so Prometheus must join that network to scrape it.

| Area | Metrics |
|---|---|
| DHT | `dc3_dht_good_nodes`, `dc3_dht_discovered_dropped_total`, `dc3_dht_peer_store_keys`, `dc3_dht_packets_in_total{family}`, `dc3_dht_packets_out_total{family}`, `dc3_dht_packets_dropped_total{family,reason}`, `dc3_dht_queries_received_total{method}`, `dc3_dht_timeouts_total`, `dc3_dht_routing_nodes{family}`, `dc3_dht_samples_total{family}`, `dc3_dht_sampler_early_total` (must stay 0), `dc3_dht_sampler_visited_full_total`, `dc3_dht_responder_dropped_total` |
| Pipeline | `dc3_discovered_total{source,family}`, `dc3_admitted_total{source}`, `dc3_queue_depth`, `dc3_fetch_total{outcome}` (ok, no_peers, fetch_failed, parse_error, blocked, private, denied, store_error), `dc3_blocked_total{reason}` (peer_address, denylisted, queue_full, blocked_term, private), `dc3_destination_skipped_total{reason}` (busy, rate_limited, negative_cache, map_full) |
| Index | `dc3_index_lag` (sequence numbers behind), `dc3_index_lag_seconds`, `dc3_index_docs` |
| Web | `dc3_http_requests_total{route,status}`, `dc3_search_seconds`, `dc3_rate_limited_total{route}`, `dc3_blocked_queries_total`, `dc3_reports_total{reason}`, `dc3_autohide_total`, `dc3_autohide_budget_exhausted_total` |

Access logs: the web role writes one line per request at `info` level under the log
target `dc3_web::access`. Each line has the method, route template, status and latency,
and never an address or query.

Suggested alerts:
- `dc3_dht_sampler_early_total` > 0;
- the rate of `dc3_fetch_total{outcome="ok"}` falls below half of last week's rate;
- `dc3_index_lag_seconds` > 300;
- `dc3_autohide_total` rises (someone needs to review reports).

## 7. Upgrading

1. `git pull`.
2. Read `CHANGELOG.md`.
3. `docker compose up -d --build`. The `migrate` service applies schema changes before the other roles start.

If you use published release images instead of building locally, verify them first:

```sh
gh attestation verify oci://ghcr.io/<owner>/dhtcrawler3:<version> --repo <owner>/dhtcrawler3
```

The search index is derived data. If a release changes the index schema, run
`index --rebuild`. Keep backups of the PostgreSQL volume. The index volume never needs
backing up.

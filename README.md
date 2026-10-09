# dhtcrawler4

A **BitTorrent DHT search engine**, written in Rust.

It joins Mainline DHT as a well-behaved node (BEP 5/32/42/43/51), discovers torrents with `sample_infohashes`, fetches metadata directly from peers (BEP 9/10, v1/v2/hybrid) with infohash verification, stores it in PostgreSQL, and serves multilingual search (embedded Tantivy, no JavaScript).

dhtcrawler4 is a fork of [dhtcrawler3](https://github.com/poonasor/dhtcrawler3) (itself a Rust rewrite of Kevin Lynx's 2013 Erlang dhtcrawler2, not a port). See [CHANGELOG.md](CHANGELOG.md) for the full fork diff.

- [Running a server on Ubuntu without Docker](#running-a-server-on-ubuntu-without-docker)
- [Running a server on Ubuntu](#running-a-server-on-ubuntu)
- [Updating the running server](#updating-the-running-server)
- [Troubleshooting](#troubleshooting)

## What changed since the dhtcrawler3 fork

- **Removed content moderation:** report form, CSRF, `dc3-policy` crate, blocked-terms list, denylist/block pages, report/denylist tables, policy fuzz target.
- **Added seeder scrapes (BEP 33):** the crawler now asks peers how many seeders each torrent has on its own bandwidth budget, shows seeder counts in search with `sort=seeders`, drops dead torrents, and remembers removed keys for 7→28→90 days.
- **Server-sized crawl throughput:** fetch workers claim 8 keys at a time and record each batch in a single database transaction (falling back to one key at a time when a batch is rejected), with tighter fetch timeouts, a bigger scrape budget, 160 sampler workers, and a larger database connection pool plus gauges to watch it.
- **Store durability:** deleted torrents stay reported-as-deleted so slow indexers still converge, database permissions repair themselves on migrate, refetched torrents come back to life with their liveness data intact, and counters can't overflow.
- **Cookie theme switch, still no JavaScript:** header dark/light toggle via `POST /theme` (`theme` cookie, dark default). Removed `/.well-known/security.txt` and the `web.base_url` setting (old configs must drop that key).
- **Hardened DHT / parsing / fetch:** stricter peer identity and routing-table rules, IPv6 rate limits, memory-safe parsing of torrent files, and deadline + retry handling for metadata fetch.
- **Fixed search / store / web:** bounded search-prefix work, correct pagination, queue and admission fixes that fail safe instead of stalling, crashed workers stop the role instead of being silently swallowed, shutdowns always finish instead of hanging, stricter config checks, secret redaction, least-privilege database users.
- **Added search query cache:** `GET /search` and `GET /api/v1/search` share a server-side cache of raw index results keyed by `(text, sort, page, per_page)` (`web.search_cache_size = 100`, `web.search_cache_ttl_secs = 900`; either zero disables). Only non-empty successes are stored, entries expire on a fixed TTL, evict least-recently-used past capacity, and clear on any index commit; concurrent identical misses share one index search.
- **Perf + tests:** ~35 alloc/scan eliminations, `cargo fmt`, opt-in live-network tests, BEP 33 vectors, live-Postgres batch-equivalence tests, updated e2e coverage.

## Running a server on Ubuntu without Docker

Docker is not required. The binary is an ordinary Linux executable; without
Docker you provide PostgreSQL, the config, and three systemd units yourself.
This is more hands-on than the Compose path below (an afternoon rather than
an hour, and unattended reboots need the ordering below to be right), but
nothing here is exotic if you know Postgres and systemd. Everything assumes a
fresh Ubuntu 24.04 machine with a public IPv4 address (IPv6 optional) and a
DNS name pointing at it.

Sizing is the same as the Docker path: at least 4 GiB RAM, tens of GiB of
disk to start (100+ GiB comfortable — the database and index grow with the
corpus), and UDP 6881 reachable from the internet.

### 1. Service user and directories

Everything runs as an unprivileged system user. Config lives in
`/etc/dhtcrawler4`, passwords in a `0700` subdirectory only that user can
read, state in `/var/lib/dhtcrawler4`:

```sh
sudo useradd -r -m -s /usr/sbin/nologin dhtcrawler4
sudo install -d -m 0755 -o root -g root /etc/dhtcrawler4
sudo install -d -m 0700 -o dhtcrawler4 -g dhtcrawler4 /etc/dhtcrawler4/secrets
sudo install -d -m 0755 -o dhtcrawler4 -g dhtcrawler4 /var/lib/dhtcrawler4
sudo install -d -m 0755 -o dhtcrawler4 -g dhtcrawler4 /var/lib/dhtcrawler4/index
```

Generate the four role passwords (there is no superuser password to manage —
superuser access stays peer-authenticated, step 3 — one line each, no
trailing-newline issues since `read_password_file` tolerates exactly one):

```sh
for name in dc3_owner_password dc3_crawler_password dc3_indexer_password dc3_web_password; do
  head -c 60 /dev/urandom | base64 -w 0 | sudo tee /etc/dhtcrawler4/secrets/$name > /dev/null
done
sudo chown dhtcrawler4:dhtcrawler4 /etc/dhtcrawler4/secrets/*
sudo chmod 0600 /etc/dhtcrawler4/secrets/*
```

Back up this directory. Losing the passwords means losing the database.

### 2. PostgreSQL 18

The schema needs PostgreSQL 14+ features (newest used: SQL-standard function
bodies and `lz4` column compression, which degrades gracefully), so Ubuntu's
stock PostgreSQL 16 would almost certainly work — but CI and the Docker image
run 18.6, so install 18 from the official PGDG repo to match exactly what is
tested:

```sh
sudo apt-get update
sudo apt-get install -y postgresql-common
sudo /usr/share/postgresql-common/pgdg/apt.postgresql.org.sh -y
sudo apt-get install -y postgresql-18
sudo pg_lsclusters   # 18/main should be online
```

The service roles connect over TCP with `scram-sha-256`, which is the PGDG
default — confirm `/etc/postgresql/18/main/pg_hba.conf` contains:

```
host    all    all    127.0.0.1/32    scram-sha-256
```

### 3. Roles and database

`deploy/postgres/init/10-roles.sh` creates the `dc3` database and the four
least-privilege roles (`dc3_owner` for migrations, one service role each for
crawl/index/web, with connection limits and idle-transaction timeouts). It
runs unchanged outside Docker, but `psql` reads the secret files client-side,
so run it as the `postgres` OS user with throwaway copies it can read:

```sh
cd dhtcrawler4   # your checkout
sudo install -d -m 0700 -o postgres -g postgres /tmp/dc3sec
sudo install -m 0600 -o postgres -g postgres /etc/dhtcrawler4/secrets/* /tmp/dc3sec/
sudo -u postgres env \
  DC3_OWNER_PASSWORD_FILE=/tmp/dc3sec/dc3_owner_password \
  DC3_CRAWLER_PASSWORD_FILE=/tmp/dc3sec/dc3_crawler_password \
  DC3_INDEXER_PASSWORD_FILE=/tmp/dc3sec/dc3_indexer_password \
  DC3_WEB_PASSWORD_FILE=/tmp/dc3sec/dc3_web_password \
  bash deploy/postgres/init/10-roles.sh
sudo rm -rf /tmp/dc3sec
```

Superuser access stays peer-authenticated (`sudo -u postgres psql`), which
needs no password; the service roles use their passwords over TCP.

### 4. Build and install the binary

Only Rust is needed — the Postgres driver is pure Rust, so no `libpq` or
OpenSSL headers; `build-essential` is just for the linker:

```sh
sudo apt-get install -y build-essential git curl
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
source "$HOME/.cargo/env"
git clone https://github.com/electroglyph/dhtcrawler4
cd dhtcrawler4
cargo build --release --locked -p dhtcrawler4   # toolchain 1.99.0 auto-installs
sudo install -m 0755 target/release/dhtcrawler4 /usr/local/bin/dhtcrawler4
```

### 5. Configure

Copy the documented example config and point it at the local database:

```sh
sudo install -m 0644 -o root -g root deploy/config/dhtcrawler4.toml /etc/dhtcrawler4/dhtcrawler4.toml
sudoedit /etc/dhtcrawler4/dhtcrawler4.toml
```

Change in `[database]`: `host = "127.0.0.1"` (leave port 5432, name `dc3`).
On a server, also raise `max_connections`: it sizes each role's database
pool and the default 16 is lean for 512 fetch workers (the Docker deployment
runs crawl at 96 via `DC3_DATABASE__MAX_CONNECTIONS`). Keep the sum across
roles comfortably under PostgreSQL's own `max_connections` — the stock
default of 100 leaves little headroom once you raise the crawl pool (the
Docker deployment sets 200).
The per-role users and password files come from the environment in each
systemd unit (step 6), so one file serves all roles. Leave `hsts = false`
(the default) unless you terminate TLS in front of the site. The `[crawl]`
`state_dir` and `[index]` `path` defaults already match the directories from
step 1. One key has no usable default: the example ships
`trusted_proxies = []`, which only warns on loopback but is rejected when
the site listens on a real interface (as it does here on port 80) — the
unit below supplies it via `DC3_WEB__TRUSTED_PROXIES` instead, so the file
can keep the empty default.

Validate (prints the config back without secrets, plus warnings):

```sh
sudo -u dhtcrawler4 dhtcrawler4 --config /etc/dhtcrawler4/dhtcrawler4.toml check-config
```

Then apply the migrations as the owner:

```sh
sudo -u dhtcrawler4 env \
  DC3_DATABASE__USER=dc3_owner \
  DC3_DATABASE__PASSWORD_FILE=/etc/dhtcrawler4/secrets/dc3_owner_password \
  dhtcrawler4 --config /etc/dhtcrawler4/dhtcrawler4.toml migrate
```

### 6. systemd units

Three role services plus the migration as a oneshot. The units mirror the
Compose hardening (no new privileges, read-only system, private `/tmp`) and
memory limits; `TimeoutStopSec=70` on the crawler because shutdown waits for
in-flight fetches.

`/etc/systemd/system/dhtcrawler4-migrate.service`:

```ini
[Unit]
Description=dhtcrawler4 database migrations (oneshot)

[Service]
Type=oneshot
User=dhtcrawler4
Group=dhtcrawler4
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
Environment=DC3_DATABASE__USER=dc3_owner
Environment=DC3_DATABASE__PASSWORD_FILE=/etc/dhtcrawler4/secrets/dc3_owner_password
ExecStart=/usr/local/bin/dhtcrawler4 --config /etc/dhtcrawler4/dhtcrawler4.toml migrate
```

`/etc/systemd/system/dhtcrawler4-crawl.service`:

```ini
[Unit]
Description=dhtcrawler4 DHT crawler
After=network-online.target postgresql@18-main.service dhtcrawler4-migrate.service
Wants=network-online.target postgresql@18-main.service

[Service]
Type=exec
User=dhtcrawler4
Group=dhtcrawler4
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
ReadWritePaths=/var/lib/dhtcrawler4
MemoryMax=2G
Environment=DC3_DATABASE__USER=dc3_crawler
Environment=DC3_DATABASE__PASSWORD_FILE=/etc/dhtcrawler4/secrets/dc3_crawler_password
Environment=DC3_LOG__FORMAT=json
ExecStart=/usr/local/bin/dhtcrawler4 --config /etc/dhtcrawler4/dhtcrawler4.toml crawl
TimeoutStopSec=70
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
```

`/etc/systemd/system/dhtcrawler4-index.service` — same, with
`MemoryMax=1G`, `DC3_DATABASE__USER=dc3_indexer`,
`DC3_DATABASE__MAX_CONNECTIONS=4`, the indexer password file, and `index`
as the command.

`/etc/systemd/system/dhtcrawler4-web.service` — same, with `MemoryMax=512M`,
`DC3_DATABASE__USER=dc3_web`, the web password file, the bind privilege for
port 80 (everything still runs as the unprivileged user — no root, no
`setcap` on the binary, which would be wiped by every rebuild), and these
(they override the TOML, so the file can keep defaults):

```ini
AmbientCapabilities=CAP_NET_BIND_SERVICE
Environment=DC3_WEB__LISTEN=0.0.0.0:80
Environment=DC3_WEB__TRUSTED_PROXIES=127.0.0.1
```

Enable and start (migrations were already applied by hand in step 5):

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now dhtcrawler4-crawl dhtcrawler4-index dhtcrawler4-web
systemctl status dhtcrawler4-crawl dhtcrawler4-index dhtcrawler4-web
curl -s http://127.0.0.1/healthz
```

Unlike Compose nothing health-gates the startup order — the units start after
Postgres is *started*, not *ready*. `Restart=on-failure` covers a crash, and
the web role keeps serving stale stats rather than dying if the first stats
load fails, but after a reboot glance at `journalctl -u dhtcrawler4-web` to
confirm it connected.

### 7. Firewall, TLS, and everything after

From here the Docker path below applies unchanged: same firewall rules (step 4
there), same host Caddy setup for HTTPS (step 6 there), same expectations
while the index fills (step 7), and the same no-access-log rule for whatever
terminates HTTP. Day-to-day differences:

```sh
journalctl -u dhtcrawler4-crawl -f          # JSON logs; queries never logged
systemctl restart dhtcrawler4-web
```

- **Metrics:** each role listens on `127.0.0.1:9100` per `[metrics]`
  (localhost only — scrape over an SSH tunnel, never publish the port).
- **Totals:** `sudo -u dhtcrawler4` with the *owner* role's `DC3_DATABASE__*`
  env (the web role cannot read the pending queue) plus
  `dhtcrawler4 --config /etc/dhtcrawler4/dhtcrawler4.toml stats`.
- **Rebuild the search index:** `systemctl stop dhtcrawler4-index`, then run
  `index --rebuild` as the service user with the indexer's env, then start
  it again.
- **Upgrade:** `git pull`, rebuild, `install` the binary, run the migrate
  unit (`systemctl start dhtcrawler4-migrate`), restart the three roles.
- **Back up:** `sudo -u postgres pg_dump dc3` (peer auth, no password
  needed) plus `/etc/dhtcrawler4/secrets/`. `/var/lib/dhtcrawler4/index`
  and DHT state are disposable — the index rebuilds from the database.

## Running a server on Ubuntu

The fastest deployment is Docker Compose on Ubuntu 24.04: PostgreSQL plus
three `dhtcrawler4` roles (crawl, index, web) on isolated networks. The site
serves plain HTTP on host port 80 by default; Caddy on the host for HTTPS is
optional (step 6). Everything below assumes a fresh Ubuntu 24.04
machine with a public IPv4 address (IPv6 optional but recommended) and a DNS
name pointing at it, e.g. `search.example.org`.

### 0. Size the machine

Minimum that works without OOM kills (these are the compose `mem_limit`s):

| Service    | RAM   | Disk                        |
| ---------- | ----- | --------------------------- |
| crawl      | 2 GiB | a few MiB of DHT state      |
| index      | 1 GiB | grows with the corpus       |
| web        | 512 MiB | —                         |
| PostgreSQL | 2 GiB | grows with the corpus       |

Use a host with at least 6 GiB RAM. Disk: tens of GiB to start; the database
and the search index grow as torrents are discovered, so leave room (100+ GiB
is comfortable). One UDP port must be reachable from the internet: **6881**.

### 1. Install Docker

```sh
sudo apt-get update
sudo apt-get install -y ca-certificates curl gnupg git
sudo install -m 0755 -d /etc/apt/keyrings
curl -fsSL https://download.docker.com/linux/ubuntu/gpg \
  | sudo gpg --dearmor -o /etc/apt/keyrings/docker.gpg
echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.gpg] \
  https://download.docker.com/linux/ubuntu $(. /etc/os-release && echo "$VERSION_CODENAME") stable" \
  | sudo tee /etc/apt/sources.list.d/docker.list > /dev/null
sudo apt-get update
sudo apt-get install -y docker-ce docker-ce-cli containerd.io docker-compose-plugin
sudo usermod -aG docker "$USER"
# Log out and back in so the docker group applies.
```

The compose network `egress` has `enable_ipv6: true` (the DHT runs over IPv4
and IPv6, BEP 32), which needs Docker Engine 27+ and an IPv6-enabled daemon.
If the host has no IPv6, delete that one line from
`deploy/docker-compose.yml` to run IPv4-only; the crawler also disables its
IPv6 socket automatically (with a warning) when there is no global IPv6
address.

### 2. Check out the source and create secrets

```sh
git clone https://github.com/electroglyph/dhtcrawler4
cd dhtcrawler4
./scripts/gen-secrets.sh
```

This writes five random 40-character passwords to `deploy/secrets/` (one for
the Postgres superuser, one for each of the `dc3_owner`, `dc3_crawler`,
`dc3_indexer`, `dc3_web` roles). The directory is mode 0700 and ignored by
git. Re-running the script is safe: existing files are kept. Back this
directory up — losing the passwords means losing the database.

### 3. Enable HSTS (only with HTTPS)

Skip this step for the default plain-HTTP setup. If you terminate TLS in
front of the site yourself, set:

```yaml
DC3_WEB__HSTS: "true"                             # HTTPS only
```

`hsts` makes the site send `Strict-Transport-Security`, so only enable it
once HTTPS actually works — over plaintext it does nothing at best, and at
worst tells browsers to refuse the site. Everything else already has sane
defaults: the web UI serves host port 80 (plain HTTP), PostgreSQL
has no published port at all, and each role connects with its own
least-privilege database user (table privileges are granted by the
migrations, which run as `dc3_owner`).

### 4. Open the firewall

Plain `iptables` (nft backend) is present on minimal images; ufw usually is
not. Run these in order — SSH first, default-deny last, or you lock yourself
out:

```sh
sudo iptables -A INPUT -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT
sudo iptables -A INPUT -i lo -j ACCEPT
sudo iptables -A INPUT -p tcp --dport 22 -j ACCEPT    # SSH: keep this
sudo iptables -A INPUT -p tcp --dport 80 -j ACCEPT    # web UI
sudo iptables -A INPUT -p tcp --dport 443 -j ACCEPT   # only needed for HTTPS
sudo iptables -A INPUT -p udp --dport 6881 -j ACCEPT  # DHT
sudo iptables -P INPUT DROP
```

Make it survive reboots:

```sh
sudo apt-get install -y iptables-persistent
sudo netfilter-persistent save
```

Port 80 is intentionally public now; everything else stays closed.
(Docker forwards published ports through its own chains, so the rules above
guard the host's own ports; container traffic is unaffected.)

### 5. Start everything

```sh
cd deploy
docker compose up -d --build
```

Startup order is enforced with health gates: `db` (Postgres 18 initialises
the data directory, creates the `dc3` database and the four roles via
`postgres/init/10-roles.sh`) → `migrate` (applies the SQL migrations in
`crates/dc3-store`) → `crawl`, `index`, `web`. The crawl role publishes
UDP 6881; the web role publishes host port 80 (plain HTTP).

Check it came up:

```sh
docker compose ps
docker compose logs --tail=30 migrate   # should end with success, then exit
curl -s http://127.0.0.1/healthz        # the HTTP stack answers
curl -s http://127.0.0.1/ | head -c 300
```

`/healthz` means "the process answers HTTP". `/readyz` additionally means
"the database answers" — the web role serves it on the site port and every
role serves it on its metrics port 9100 (reachable only inside the backend
network). The compose healthchecks use exactly these (crawl/index poll the
metrics `/readyz`, web polls `/healthz`).

### 6. HTTPS via Caddy (optional)

The default is plain HTTP on port 80. For HTTPS instead, free host port 80
and 443 for Caddy by reverting the web service to localhost-only:

```yaml
# deploy/docker-compose.yml, service `web`
ports:
  - "127.0.0.1:8080:8080"
```

```sh
cd deploy
docker compose up -d
```

Then install Caddy on the host (not in Compose, so certificate state
survives container rebuilds):

```sh
sudo apt-get install -y debian-keyring debian-archive-keyring apt-transport-https
curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' \
  | sudo gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' \
  | sudo tee /etc/apt/sources.list.d/caddy-stable.list
sudo apt-get update
sudo apt-get install -y caddy
```

Copy `deploy/Caddyfile` to the system location with your domain in it, and
check it parses (the file has no access log on purpose — query text is part
of the URL, so logging requests would store visitors' IPs next to their
searches; do not add a `log` directive):

```sh
sed 's/search\.example\.org/your.domain/' deploy/Caddyfile | sudo tee /etc/caddy/Caddyfile > /dev/null
sudo caddy validate --config /etc/caddy/Caddyfile --adapter caddyfile
```

The apt package already ships a systemd unit (`caddy.service`) that runs
Caddy as the unprivileged `caddy` user (it gets only the bind privilege for
ports 80/443), so there is no unit to write — just enable it:

```sh
sudo systemctl enable --now caddy
systemctl status caddy --no-pager
journalctl -u caddy --since '5 min ago' --no-pager   # expect "certificate obtained successfully"
```

Caddy fetches the certificate automatically on first start, provided DNS
already points at the host and ports 80/443 are open (step 4). If it fails,
the journal shows the ACME error — usually DNS not propagated yet or port 80
blocked; fix the cause and `sudo systemctl restart caddy`. Certificate files
and ACME state live in `/var/lib/caddy` on the host (no container to wipe
them); include that directory in backups alongside the database.

Verify end to end, then turn on HSTS:

```sh
curl -sI https://your.domain | head -5      # HTTP/2 200, no Server header
curl -s http://your.domain/ -o /dev/null -w '%{redirect_url}\n'   # → https://
```

Future config changes: edit `/etc/caddy/Caddyfile`, re-run `caddy validate`
as above, then `sudo systemctl reload caddy` (zero-downtime; `restart` only
if reload fails). Browse to `https://your.domain` — the home page shows zero
torrents at first. That is normal (next step). Set `DC3_WEB__HSTS: "true"`
(step 3) once HTTPS works.

### 7. What happens next (be patient)

1. The crawl role bootstraps into Mainline DHT from the seed nodes in
   `[crawl].bootstrap`, then discovers infohashes with `sample_infohashes`.
2. Fetch workers download metadata directly from peers (BEP 9/10) and verify
   it against the infohash; verified rows land in PostgreSQL.
3. The index role polls the change feed every second and commits new
   search-index generations as rows arrive; the web role picks them up
   within about 5 s.
4. Home-page stats (`Torrents`, `Added today/yesterday`) refresh every 60 s.

Expect the first searchable torrents within minutes and a steadily growing
index over hours to days. The default crawl budget (`max_packets_per_sec =
1000`, 512 fetch workers) is sized for a server; lower it in
`deploy/config/dhtcrawler4.toml` on a home connection.

### 8. Day-to-day operation

```sh
cd deploy
docker compose logs -f crawl          # JSON logs; query text never logged
docker compose logs -f web
docker compose ps                     # includes healthcheck status
```

- **Metrics:** each role exposes Prometheus metrics on port 9100 inside the
  container (the compose file publishes none of them — scrape via an SSH
  tunnel, e.g. `ssh -L 9100:localhost:9100 host`, never by publishing the
  port).
- **Database pool pressure:** `dc3_db_pool_size` vs `dc3_db_pool_idle` on the
  crawl role. Idle pinned near zero means the pool is saturated — raise
  `DC3_DATABASE__MAX_CONNECTIONS` (compose sets 96) and PostgreSQL's
  `max_connections` (compose sets 200) together, never just one side.
- **Totals:** `docker compose exec crawl stats` (the image bakes in the
  binary and config path, so only the subcommand is needed)
- **Config check (prints config without secrets):** same with
  `check-config`.
- **Rebuild the search index from scratch:** stop the index role first, then
  `docker compose run --rm index index --rebuild`.
- **Upgrade:** `git pull`, `docker compose up -d --build`. Migrations run
  automatically on every start via the `migrate` service; the Tantivy index
  on its volume survives restarts and is rebuilt only when you ask.
- **Back up:** the `pgdata` volume (the database) and `deploy/secrets/`
  (the passwords). The `index` and `state` volumes are disposable — the
  index rebuilds from the database and DHT state re-bootstraps.
- **Stop everything:** `docker compose down`. Volumes survive; add `-v` only
  if you mean to wipe the database and index.

### Without Docker

See [Running a server on Ubuntu without Docker](#running-a-server-on-ubuntu-without-docker)
above for the full bare-metal setup.

## Updating the running server

Code changes (and the TOML baked into the image) take effect on a rebuild.
Database migrations run themselves; your data lives in volumes and is never
touched by a rebuild.

### Standard update (Docker)

```sh
git stash push -- deploy/docker-compose.yml   # park your local edits, if any
git pull
git stash pop                                 # re-apply them
cd deploy
docker compose up -d --build
docker builder prune -f                     # drop the superseded build cache
docker image prune -f                       # drop the superseded image
```

What happens, in order: the image rebuilds from the new tree; `migrate`
runs (a no-op when the schema is current); `crawl`, `index` and `web`
recreate behind the same dependency gates as first install (each waits for
`migrate`, then starts together).
`pgdata`, `state` and `index` volumes survive — only containers restart, so
the web UI is down for seconds. Then verify:

```sh
docker compose ps                             # all healthy, migrate exited 0
docker compose logs --tail=20 crawl web
curl -s http://127.0.0.1/healthz
```

Rules of thumb for what needs a rebuild:

- **Code or `deploy/config/dhtcrawler4.toml` changed** → `--build` (the TOML
  is copied into the image).
- **Only `deploy/docker-compose.yml` env changed** (knobs, secrets
  rotation) → plain `docker compose up -d` recreates with the new
  environment, no build.
- **Only secrets rotated** → replace the files in `deploy/secrets/` and
  `up -d` (containers read them at startup).

If `git stash pop` reports a conflict (upstream touched the same lines you
edited), open `deploy/docker-compose.yml`, keep your values for the
conflicted hunks, then `up -d --build` as usual.

### Before you upgrade: back up

Migrations run forward only — there is no downgrade path for the schema.
If a release ever misbehaves, rolling the *code* back is easy but the
*database* stays migrated, so snapshot first when the CHANGELOG mentions
migrations:

```sh
cd deploy
docker compose stop crawl index web   # quiesce writers; db keeps running
docker run --rm -v dhtcrawler4_pgdata:/data -v "$PWD":/backup ubuntu \
  tar czf /backup/pgdata-backup.tar.gz -C /data .
docker compose start crawl index web
```

(The volume is named `<project>_<name>`; confirm with
`docker volume ls | grep pgdata`.) Copy the tarball and `deploy/secrets/`
off the host. To restore: `down`, fresh `pgdata` volume, extract the
tarball into it, `up -d --build`.

Reclaim build disk now and then: `docker builder prune -f` drops the
superseded build cache (gigabytes per rebuild) and
`docker image prune -f` drops the superseded images (never prune volumes
unless you mean to wipe the database and index — that flag is `-v`, keep
it away from routine cleanup). For hands-free cleanup, run the prune weekly
from cron (as a user in the `docker` group, or root's crontab):

```sh
# weekly sunday 04:00: prune unused build cache and images
0 4 * * 0 docker builder prune -f && docker image prune -f
```

## Troubleshooting

Slow indexing, a full disk, or a role that won't turn healthy — start with
one command that collects everything worth looking at. Run it from
`~/dhtcrawler4/deploy` and keep the file it writes (`/tmp/dc3-diag.txt`);
paste it when asking for help, or hand it to an AI — it contains every
number the checks below reason about.

```sh
cd ~/dhtcrawler4/deploy
{
  echo '### docker ###'
  docker system df
  docker stats --no-stream
  docker compose ps
  echo '### disk ###'
  sudo sh -c 'du -sh /var/lib/docker/volumes/dhtcrawler4_*/_data'
  # If that path does not exist, the daemon uses another data root; use the
  # mountpoint it reports instead:
  # docker volume inspect dhtcrawler4_pgdata --format '{{.Mountpoint}}'
  echo '### database ###'
  docker compose exec -T db sh -c 'PGPASSWORD=$(cat /run/secrets/pg_superuser_password) psql -U postgres -d dc3 -c "SELECT * FROM stats_daily ORDER BY day DESC LIMIT 3;" -c "SELECT count(*) AS torrents FROM torrents;" -c "SELECT count(*) FILTER (WHERE NOT gave_up) AS queued, count(*) FILTER (WHERE gave_up) AS gave_up FROM pending;" -c "SELECT pg_size_pretty(pg_database_size(current_database())) AS db_size;" -c "SELECT relname, pg_size_pretty(pg_total_relation_size(relid)) AS size FROM pg_stat_user_tables ORDER BY pg_total_relation_size(relid) DESC LIMIT 6;"'
  echo '### crawl metrics ###'
  docker compose exec -T db bash -c 'exec 3<>/dev/tcp/crawl/9100 && printf "GET /metrics HTTP/1.0\r\nHost: crawl\r\n\r\n" >&3 && grep -E "^dc3_fetch_total|^dc3_dht_(routing_nodes|good_nodes|samples_total|timeouts_total|queries_received_total)" <&3'
} 2>&1 | tee /tmp/dc3-diag.txt
```

Two things in there need explaining. The database lives in a container
with no published port, so the SQL goes through the `db` service with the
superuser password from the secrets file. The crawl container is
distroless (no shell, no curl), so the metrics are fetched by borrowing
the `db` container's shell to knock on `crawl:9100` over the internal
network. `sudo` is needed only for the `du` line: the shell must expand
the `*` as root, hence the quotes. Without Docker, the equivalents are
plain `psql`, `du` on the data/index directories, and
`curl localhost:9100/metrics`.

How to read the output:

- **Nothing indexed after a night?** Compare `discovered` vs `fetched`
  vs `fetch_failed` in `stats_daily`. Failed dwarfing fetched means
  workers burn attempts on dead peers — normal up to a point (the DHT is
  mostly graveyards); a success rate near zero with millions queued is
  the crawler working as designed, just slowly. Then look at
  `dc3_fetch_total`: `no_peers` dominating means lookups find nobody
  holding the keys (stale keys); `fetch_failed` dominating means peers
  exist but TCP connects/handshakes die (firewalled internet, or
  timeouts too tight).
- **Is the DHT side healthy?** `good_nodes` in the hundreds with
  `timeouts_total` a small fraction of `samples_total` is fine, and the
  table keeps filling for the first days. `queries_received_total`
  climbing (especially `get_peers`) proves inbound UDP 6881 works — if
  it sits near zero, the internet cannot reach the node: fix the
  firewall/security group, no config knob compensates. All-zero `v6`
  lines just mean no working IPv6 (expected when it is disabled on the
  host; harmless).
- **Disk filling up?** `docker system df` first: gigabytes of reclaimable
  build cache is normal after repeated `--build` updates — `docker
  builder prune -f` drops it. Of the volumes, `pgdata` is always the
  big one: the pending queue holds millions of undiscovered keys (that
  is the `queued` count, not a problem by itself) and constant row
  updates churn WAL. The `index` volume stays small until the torrent
  count is large; `state` (DHT routing snapshot) is kilobytes.
- **Out of headroom?** `docker stats` shows it: crawl CPU/mem near its
  2 GiB cap, or the DB pool idle gauge pinned at zero, means the crawl
  budget outgrew the machine — lower `fetch_workers`/`max_connections`
  or grow the host. Plenty of idle CPU/RAM means the opposite: the
  knobs, not the hardware, are the limit.
- **A role unhealthy?** `docker compose ps` names it;
  `docker compose logs --tail=30 <role>` (JSON lines, query text never
  logged) usually names the cause: DB unreachable, bad TOML key, port
  already bound.

## License

MIT. dhtcrawler4 follows the design of Kevin Lynx's dhtcrawler2, and his copyright notice is kept in [LICENSE.txt](LICENSE.txt).

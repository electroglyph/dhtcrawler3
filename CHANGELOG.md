# Changelog

## Unreleased — dhtcrawler3 0.1.0

A complete rewrite of dhtcrawler2 in Rust. See `docs/` for the reasoning.

- New: a BEP 5/32/42/43/51 DHT node that discovers torrents with `sample_infohashes`.
- New: BEP 9/10 metadata fetching from peers, with SHA-1 and SHA-256 verification.
- New: v2 and hybrid torrent support (BEP 52).
- New: PostgreSQL storage with a leased work queue and least-privilege roles.
- New: an embedded Tantivy search index with dictionary-free CJK bigrams.
- New: a web front end with auto-escaped templates, no JavaScript and a strict CSP.
- New: a denylist, CSAM term filter and takedown CLI.
- Removed: the visitor report form, the CSAM auto-hide and the open-report queue (takedown CLI, denylist and audit log remain).
- Removed: the precompiled Erlang `.beam` files, the Windows DLLs and `.bat` launchers, the MongoDB and Sphinx support, and every fetch from the third-party torrent caches (`torcache.net`, `torrage.com`, `bt.box.n0808.com`). The legacy source remains in git history and on the upstream `src` branch.
- Security review (2026-09-17): five review lenses (web, DHT network, DHT protocol, parsers, database). 20 findings were reproduced with failing tests and fixed, including: a CSAM-filter bypass through padding files and hybrid v1 file lists; slowloris; IPv6 rate-limit exhaustion; per-/64 DHT limits; announce and sample key injection; routing-table squatting; an endless re-fetch loop; and the crawler role's ability to un-hide reported torrents. Seven low-severity items are deferred (docs/03-design.md §15).

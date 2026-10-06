# Codebase Audit — dhtcrawler3

> Status: IN PROGRESS (started 2026-10-06). Findings are appended as they are proven with repro code in `/tmp`.
> Rule: every claim below has a repro (script or test file) under `/tmp` that demonstrates the issue. No unverified claims.

## Method
- Manual read of all 111 `.rs` files (~48k LOC) + parallel subagent sweeps per crate.
- Each finding: location (`file:line`), snippet, what's wrong, repro path, observed repro output, severity.
- Scope: correctness only. No design decisions / refactors.

## Findings

### F-01 (HIGH): `dc3-bencode` iterative decoder claim "cannot crash" defeated by recursive `Drop`/`to_owned_value`/`Clone`
- Location: `crates/dc3-bencode/src/lib.rs:99-104 (enum Value), :141-153 (to_owned_value)`, `crates/dc3-bencode/src/decode.rs` (iterative `run()`).
- Snippet: `Value::List(Vec<Value>)` derives recursive `Drop`; `to_owned_value` recurses; docs `lib.rs:7-9` claim "Decoding is iterative ... nesting depth can never exhaust the thread stack".
- What's wrong: `decode` with `max_depth=50010` on `l*50000 + e*50000` succeeds iteratively, then normal `drop(Value)` recurses 50k frames and overflows.
- Repro: `/tmp/audit-repro/src/bin/repro_bencode_drop.rs` (`cargo run --manifest-path /tmp/audit-repro/Cargo.toml --bin repro_bencode_drop`)
- Observed: `decode OK. Now dropping in 512KiB-stack thread...` then `thread '<unknown>' has overflowed its stack / fatal runtime error: stack overflow, aborting`. Process aborts — proof of recursive drop. Same shape applies to `to_owned_value`/`Clone` (recursive fns).
- Note: shipped `Limits` cap depth at 8/64 so not exploitable today; but `unlimited`-style configs or future raised limits re-expose it, and doc claim is false as stated.
- Correction (2026-10-06 re-check, VERIFIED with two precision fixes — proof below):
  - Line fix: enum `Value` is at `lib.rs:99-104`, not `:97-104` (`:97-98` are the doc comment + `#[derive]`; verified by `read lib.rs:97-104`).
  - Repro-scope fix: the binary aborts with SIGABRT (exit 134) at the `drop(v)` stage, so the second half (`to_owned_value` on depth 20000) never executes in the same run. Repro output 2026-10-06: `decoding depth=50000 iteratively... / decode OK. Now dropping in 512KiB-stack thread... / thread '<unknown>' (...) has overflowed its stack / fatal runtime error: stack overflow, aborting / Aborted (core dumped)`. This proves recursive `Drop`. The `to_owned_value`/`Clone` recursion is proven by code inspection, not by this run: `lib.rs:145 l.iter().map(Value::to_owned_value)` and `:149 e.value.to_owned_value()` are direct recursion with no depth guard, and `OwnedValue::List(Vec<OwnedValue>)` (encode.rs) has the same recursive-`Drop` shape. To prove `to_owned_value` independently, run it alone in a fresh process (comment out the `drop` section or split into a second binary) on a small-stack thread — expected same `has overflowed its stack` abort. Conclusion (doc claim false as stated; not exploitable under shipped 8/64 limits) unchanged.

### F-02 (MEDIUM): `max_items` counts dict keys, docs say values only
- Location: `crates/dc3-bencode/src/lib.rs:36-37` ("Maximum total number of values (scalars and containers)"), `crates/dc3-bencode/src/decode.rs:102-103` (`self.count_item()` after `parse_bytes` key).
- Snippet: key parse calls `count_item()`; own `dc3-torrent/src/parse.rs:67` says "(1 000 000 items, keys included)" — direct contradiction.
- Repro: `/tmp/audit-repro/src/bin/repro_bencode.rs`
- Observed: `d1:ai1e1:bi2ee` with `max_items=3 => Err(TooManyItems)`, with `max_items=5 => Ok`. If only values counted (dict+2 vals=3) the first would pass. Proves keys counted (dict+2 keys+2 vals=5).
- Impact: doc contradiction; fail-closed (over-count) so safe direction, but limit unpredictable.

### F-03 (LOW): huge string-length digit overflow reports `IntegerOverflow`, in-range huge length reports `StringTooLong`
- Location: `crates/dc3-bencode/src/decode.rs:219-225 (parse_digits overflow)`, `:266-274 (parse_bytes len check)`.
- Repro: `/tmp/audit-repro/src/bin/repro_bencode.rs`
- Observed: `"9"*30 + ":x" => IntegerOverflow @19`, `u64::MAX + ":x" => StringTooLong @21`. Same conceptual failure (length exceeds limit/memory) yields two kinds depending on magnitude fitting `u64`. Callers matching only `StringTooLong` miss the overflow case.

### F-04 (LOW, defensive): `emit` silent `unwrap_or_default` on raw span + `saturating_add` masking
- Location: `crates/dc3-bencode/src/decode.rs:187-190` (`input.get(value_start..pos).unwrap_or_default()`), `:61-72` (`saturating_add` in `advance`/`count_item`).
- What's wrong: `value_start..pos` must be in-bounds by construction; on logic bug returns `b""` and later infohash would be `SHA1("")` instead of failing (fail-open). `saturating_add` silences overflow instead of erroring.
- Repro: `/tmp/audit-repro/src/bin/repro_bencode.rs` (last two lines demonstrate the pattern: `get(99..100).unwrap_or_default() => []`, `usize::MAX.saturating_add(1) => MAX`).
- Note: currently unreachable by construction; wrong error-handling pattern for hash-critical span.
- Correction (2026-10-06 re-check round 2, consequence NARROWED to hypothetical — extensive proof below):
  - `unwrap_or_default` reachability: `value_start` is captured at `decode.rs:101` (`let key_pos = self.pos`) / `:110` (`*key = Some((k, self.pos))`) after the key parse, and `self.pos` at `:189` is after the value parse. `pos` only moves forward via `advance` (`:61-64`), whose `n` is always a length already checked against `input.len()` (comment at `:62`), so both endpoints stay in `[0, input.len()]` with `value_start <= pos`. Hence `input.get(value_start..pos)` is always `Some` today; the `unwrap_or_default` arm is unreachable by construction (audit already said so).
  - `SHA1("")` fail-open is HYPOTHETICAL, not live: `Entry.raw` (`lib.rs:157-161`) is written once at `decode.rs:189-190` and read only by `Dict::raw_value` (`lib.rs:208-210`). Repo-wide grep 2026-10-06 finds 3 in-crate hits plus 1 design-doc mention (4 total excl. audit.md itself): the doc comment (`lib.rs:13`), the definition (`:208-209`), and the unit test (`tests.rs:88-94` `raw_value_spans`), plus `docs/03-design.md:144` mentioning the public API (`Dict` with `raw_value`). Zero production callers. The real infohash path never touches that span: `dc3-torrent/src/parse.rs:124-125` computes `sha1_key(info)` / `sha256_hash(info)` over the whole `info` input bytes (likewise `verify` at `dc3-torrent/src/lib.rs:58-66` hashes whole `info`). So no current path can turn the hypothetical `b""` into `SHA1("")`; the risk exists only if a future caller hashes `raw_value`. Severity stays LOW (defensive pattern point).
  - Correction 2026-10-06 round 3 (hit-count precision): prior text said "exactly 3 hits" repo-wide. `grep -rn raw_value` (excl. `audit.md`) returns `lib.rs:13` (doc), `lib.rs:208` (def), `tests.rs:88,92-94` (test), and `docs/03-design.md:144` (design doc listing `Dict` with `raw_value` in the public API). I.e. 3 in-crate hits + 1 docs mention. Conclusion unchanged (zero production callers; `SHA1("")` hypothetical-only).
  - `saturating_add` masking likewise unreachable in practice: `advance(n)` adds an input-bounded length (`pos <= len`, `n <= len`, so `pos+n <= 2*len`, and `len` is a live allocation far below `usize::MAX`); `count_item` counts decoded values where each value consumes ≥1 input byte, so `items <= input.len()`. Neither can approach `usize::MAX`; saturation never fires. The objection is to the pattern (silent wrap vs explicit error), not to a reachable overflow.
  - Repro fidelity: `repro_bencode.rs:26-28` is a stdlib pattern demo on `b"abc"` and `usize::MAX` — it never calls `emit` and proves no live trigger. Audit text already disclosed "demonstrate the pattern / currently unreachable"; this correction makes the hypothetical-vs-live distinction explicit.

### F-05 (HIGH as filed; DOWNGRADED to LOW doc-precision 2026-10-06 round 3 — mechanism true, doc reading wrong, see proof): `dc3-torrent` visited-text "upper bound" not enforced — `show()` bypasses budget
- Location: `crates/dc3-torrent/src/parse.rs:200-221` (`show`/`show_counted` never check `budget`; only `show_joined` does), `crates/dc3-torrent/src/lib.rs:42-43` ("Upper bound ... whatever the metadata size").
- Repro: `/tmp/audit-repro/src/bin/repro_torrent_budget.rs`
- Observed: `raw.len=1453219 budget=5812876 chars_visited=5891510 calls=21434 result=Err(TooMuchText); exceeds_budget=true excess=78634`. After budget trips, every remaining file name + each dir (once) still visited. Test `shared_long_prefix_is_not_rescanned_per_file` even asserts `chars <= budget+raw.len()` (up to ~24M), proving exceedance by design.
- Impact: downstream indexer cost exceeds documented bound; doc contradiction. Still bounded by `budget+raw.len()` + `MAX_FILES_PARSED*255`, but not by `budget` alone.
- Correction (2026-10-06 round 3, severity DOWNGRADED HIGH→LOW doc-precision — extensive proof that the "documented bound" was misread):
  - What the docs actually promise: `parse.rs:53-59` explicitly documents the past-budget behaviour: "joined paths are shown only while the text shown stays within `info.len() * TEXT_CHARS_PER_BYTE`, clamped to `MIN..=MAX`. Past that, each directory component (once per directory, not per file) and each file name is shown alone, and the result is `TooMuchText`." I.e. the design is budget-gated joined paths + unbounded-per-component fallback + fail-closed `Err`. The repro output itself is `Err(TooMuchText)` (`parse.rs:112-114`), so fail-closed holds.
  - Enforced/tested bound is `budget + raw.len()`, not `budget`: `tests.rs:888` asserts `chars <= text_budget(raw) + raw.len()` (repro numbers: `5812876 + 1453219 = 7266095`; observed `5891510 < 7266095`, inside the tested bound; excess `78634` is `O(info.len())`, not unbounded). The sibling test name (`shared_long_prefix_is_not_rescanned_per_file`) and the `components_beyond_the_path_cap_are_visited` test (`tests.rs:894-912`) codify that past-budget names are still seen — exceedance by design, not accident.
  - `lib.rs:42-43` ("Upper bound of the visited-text budget, whatever the metadata size") describes the `budget` clamp itself (`budget = len*4 clamped MIN..=MAX`, `Collector::new` at `parse.rs:186-188`, `MIN=1MiB :40-41`, `MAX=16MiB :42-43`), not a promise that total `visit`ed chars `<= budget`. Reading it as `visited <= budget` contradicts the longer `parse.rs:53-59` contract two files over.
  - Call-site proof: `show`/`show_counted` (`:200-207`) are the per-component fallback; `show_joined` (`:210-221`) is the only budget gate (`:215 shown+chars > budget => over_budget`). Fallbacks at `v1_files :455-459` (`if !whole { for c { show(c) } }`), `OpenDirs::show_file :579-584` + `show_names :527-533`, and `v1 single-file :472-474` deliberately keep calling `show` after `over_budget` — matching the documented "shown alone" clause. Bonus (not in original): `parse_decoded :99 visit(&name)` bypasses `shown` accounting entirely (name never counted), further proving `shown/budget` was never meant as total-visited accounting.
  - Repro fidelity: `repro_torrent_budget.rs` correctly measures `chars_visited (5891510) > budget (5812876)` and `result=Err(TooMuchText)` — the numbers are right, the interpretation ("exceeds documented bound") is wrong because the documented bound is `budget+raw.len()` with `Err`, both satisfied. Residual issue (kept as LOW): `lib.rs:42-43` wording is ambiguous enough to invite the `visited <= budget` misreading; clarify it to "upper bound of the *budget input* (total visited is bounded by budget+info.len() and fails with TooMuchText)". No unbounded-DoS, no silent overrun.

### F-06 (MEDIUM): policy separator/fragmentation bypass of single-token seeds
- Location: `crates/dc3-policy/src/lib.rs:160-178 (matches_affixed checks substring within one normalized token)`, `crates/dc3-policy/src/normalise.rs:137-156 (non-alphanumeric splits tokens)`.
- Repro: `/tmp/audit-repro/src/bin/repro_policy.rs` (`cargo run --manifest-path /tmp/audit-repro/Cargo.toml --bin repro_policy`)
- Observed: seed `pthc`: `"pthc"=>matches=true/affixed=true`, `"xpthc"=>false/true`, but `"p.t.h.c","p t h c","p-t-h-c","p_t_h_c"=>false/false`; `normalise("p.t.h.c")=>["p","t","h","c"]`. Any intra-term separator fragments the token so no token contains the seed. Same for digit-interleave `"p1thc"=>false/false` (contains check fails due to `1` in middle). Tests cover `"child.porn"` (still contiguous as phrase) but no single-letter-separated case.
- Impact: policy evasion — content that should be denied/wiped is stored and indexed. Leet map (`normalise.rs:24-33`, only `013457@$`) compounds it: `! *` etc. split tokens too (P1 again, not just missed synonym).

### F-07 (LOW): `MAX_TERM_LINE_CHARS` measured after comment-stripping (lenient)
- Location: `crates/dc3-policy/src/lib.rs:75-81`.
- Repro: `/tmp/audit-repro/src/bin/repro_policy.rs` (last lines)
- Observed: `"ab#" + "x"*1_000_000 => Ok(len=1)` — 1MB raw line passes length gate since only `"ab"` measured. Limit does not bound raw line length (memory/CPU asymmetry vs stated limit).

### F-08 (WITHDRAWN 2026-10-06 round 3 — no bug: top-level-only is intended, tested BEP47 behaviour; extensive proof below)
- Location: `crates/dc3-torrent/src/parse.rs:270-274 (is_padding)`, `:498-559 (OpenDirs::top_is_pad)`, v1 checks only `components.first()`, v2 checks only outermost dir.
- Repro: `/tmp/audit-repro/src/bin/repro_torrent2.rs` (`cargo run --manifest-path /tmp/audit-repro/Cargo.toml --bin repro_torrent2`)
- Observed: `a/.pad/f => file_count=1 files=[a/.pad/f]` (listed as content) vs top-level `.pad/5 => file_count=0 files=[]` (padding). Proves intermediate `.pad` ignored.
- Impact (original, now withdrawn): if BEP47 allows `.pad` at any level, false negative inflates `total_size`/`file_count`; if top-only, code correct but undocumented. Either way inconsistency vs docs.
- Why withdrawn (extensive proof 2026-10-06 round 3):
  - BEP47 authoritative text (https://www.bittorrent.org/beps/bep_0047.html, "Padding files" section): "The recommended path is `[".pad", "N"]` where N is the length ... in base10." I.e. the spec's own recommended path is top-level `.pad` + numeric name. The authoritative padding signal is `attr` containing `p` ("p = padding file ... unknown characters should be ignored"), which the code honours at both levels: `is_padding(first,last,attr)` checks `attr.contains(b'p')` first (`parse.rs:270-274`), v1 passes `fd.get_bytes(b"attr")` (`:427`), v2 passes `fd.get_bytes(b"attr")` (`:628`). A nested `.pad` dir without `attr=p` is NOT a spec-defined padding marker — the audit's "if BEP47 allows .pad at any level" antecedent is speculative and contrary to the recommended-path text.
  - Intended + tested: `tests.rs:235-236` comment "Not padding: `.pad` only matters as the first component" with `v1_file(1, &["sub", ".pad"])`, and `:241` asserting `paths == ["movie.mkv","run.sh","sub/.pad"]` (nested `.pad` listed as content). Code comments agree: `parse.rs:265-269` ("BEP 47 padding lives in a `.pad` *directory* (`.pad/<N>`)"), `:428-430` ("The `.pad`-directory rule needs a directory part"), `:501-503` (`top_is_pad` tracks only the outermost dir), `:540-559` (`is_padding` maps non-top dirs to `""`). Repro outputs (`a/.pad/f` listed, `.pad/5` excluded) exactly match the tested contract — repro proves correct behaviour, not a bug.
  - Safe-direction analysis (audit inverted it): treating a nested `.pad` path as content (current) over-counts `total_size`/`file_count` — fail-safe for an indexer (shows more). Treating it as padding (audit's implied fix) would let an attacker hide real content (`a/.pad/evil.bin` with 1GB) from `total_size`/`file_count`/listing — under-count = fail-open. Current direction is the safe one. Note v2 is already broader than the BEP minimum (anything under top-level `.pad` excluded, `top_is_pad` at `:501-503`, test `:940-956` `.pad/blocked d` excluded).
  - Verdict: no finding. Kept numbered (like F-11) so F-numbering stays stable. If anything, a docs-clarification INFO (cite BEP47 recommended path in `is_padding` docs) — not a MEDIUM correctness bug.

### F-09 (LOW — DOWNGRADED from MEDIUM 2026-10-06 round 3; mechanism narrowed to lossy leg with proof): `path.utf-8` invalid-UTF8 still preferred over valid legacy `path` (GB18030/lossy fallback)
- Location: `crates/dc3-torrent/src/text.rs:13-26 (decode_text tries label then GB18030 then lossy)`, `crates/dc3-torrent/src/parse.rs:349-412 (same decode_text for both pref and leg lists)`.
- What's wrong: BEP3 `.utf-8` must be valid UTF-8; invalid bytes that happen to decode via GB18030/lossy are accepted as the preferred value instead of falling back per-component to legacy.
- Repro: `/tmp/audit-repro/src/bin/repro_torrent2.rs`
- Observed: pref `[0x81,0x30]` (invalid UTF-8) + leg `fallback` => `files=[FileEntry { path: "�0" }]` — lossy pref used, `fallback` ignored. Proves strict-UTF8 fallback violated (whether GB18030 or lossy path, pref wins when it shouldn't).
- Correction (2026-10-06 re-check, VERIFIED with narrowed mechanism — proof below):
  - Re-ran `repro_torrent2` 2026-10-06: `UTF8 pref=[129, 48] leg=fallback => files=[FileEntry { path: "�0", size: 1 }]` — exact match.
  - The observed `"�0"` pins the mechanism to the **lossy** leg, not GB18030. Proof: `text.rs:22-25` tries `GB18030.decode_without_bom_handling_and_without_replacement([0x81,0x30])` first; `0x81` is a GB18030 lead byte (0x81-0xFE) but `0x30` (`'0'`, 0x30) is not a valid trail byte (trail must be 0x40-0xFE excluding 0x7F, or 0x30-0x39 only as the *second* byte of a 4-byte sequence whose third byte is 0x81-0xFE — `[0x81,0x30]` alone is incomplete/invalid), so it returns `None` and falls through to `String::from_utf8_lossy` at `:25`, yielding `"�0"` (U+FFFD + `'0'`). That non-empty sanitised pref then wins at `parse.rs:364 s = sanitize_path_component(a).or_else(...(b))`, so the legacy `"fallback"` is never consulted.
  - Consequence unchanged (invalid-UTF8 pref preferred over valid legacy); only the "whether GB18030 or lossy" hedge is now resolved to lossy for this vector. A pure-GB18030 vector (e.g. bytes that form a valid GB18030 double-byte pair but invalid UTF-8) would take the `:22-24` branch and exhibit the same fallback violation via the GB18030 leg.
  - Correction (2026-10-06 round 3, severity DOWNGRADED MEDIUM→LOW — extensive proof the bug is display-only and matches the code's own documented contract):
  - Code-matches-own-docs: `parse.rs:340-344` documents "preferring `path.utf-8` but falling back to `path` when the preferred value sanitises to nothing", and the whole-list branch at `:378-385` falls back only `if !san.is_empty()` fails (i.e. sanitised-empty, not invalid-UTF8). `decode_text` (`text.rs:11-12`) documents "lossless when possible and lossy as a last resort". Since `from_utf8_lossy` never returns empty for non-empty input (yields U+FFFD per invalid run — here `"�0"` for `[0x81,0x30]`), and `sanitize_path_component("�0")` returns `Some("�0")` (U+FFFD is not stripped; only controls/bidi/zero-width/`/.`/`..` are — verified by repro output `files=[FileEntry { path: "�0" }]`), the pref path `Some→non-empty→return at :383-384` is taken and legacy never consulted. The code does exactly what its comments say; the "violation" is against a strict-UTF8 reading of BEP3, not against the crate's contract.
  - Display-only, bounded impact: both pref and leg go through the same `sanitize_path_component` + `CappedPath (4096)` + `MAX_FILES_PARSED` pipeline; the wrong pick changes one displayed path string (`"�0"` vs `"fallback"`), not `total_size`/`file_count` accounting, not any bound, not any hash (`infohash` hashes raw `info` bytes at `parse.rs:124-125` / `lib.rs:58-66`, never the decoded path). No DoS, no hide-content (both branches listable), no fail-open write.
  - Repro fidelity: `repro_torrent2.rs:24-34` is a live Rust repro and tests the right thing (pref-invalid + leg-valid → which wins); output `"�0"` is exact and pins the lossy leg (GB18030 leg returned `None` as proved above). Interpretation fixed: proves lenient-pref-wins (LOW display fidelity), not a MEDIUM content-hiding/bound bug.

### F-10 (WITHDRAWN 2026-10-06 round 3 — no bug: empty `pieces` is a valid zero-length v1 torrent, proof below)
- Location: `crates/dc3-torrent/src/parse.rs:86` (`b.len() % 20 == 0`; `0 % 20 == 0`), `:463-477` (single-file requires `length`).
- Repro: `/tmp/audit-repro/src/bin/repro_torrent2.rs`
- Observed: `info={name,piece length,pieces:""}` (no files/length/tree) => `Err(InvalidField("length"))`, not `NotATorrent`. Proves `pieces=""` counted as v1 (`is_v1=true`) and entered v1 single-file branch.
- Why withdrawn (extensive proof 2026-10-06 round 3):
  - Empty `pieces` is intentionally valid: `tests.rs:118-130` `v1_empty_pieces_is_zero_length_torrent` builds `{name:"x", length:0, piece length:16384, pieces:""}` and requires `Ok` with `total_size==0, file_count==1, info_hash_v1.is_some()`. Re-verified live 2026-10-06 (`empty+len0 => Ok(v1)`, `empty+len100 => Ok(v1)` per subagent re-run). Zero pieces + zero/one length is the degenerate zero-length torrent, not malformed.
  - Flow proof: `parse.rs:86 is_v1 = matches!(pieces, Some(Bytes(b)) if b.len()%20==0)` → `len 0` satisfies → `is_v1=true`; `:92-94 if !is_v1 && tree.is_none() => NotATorrent` skipped (correct — it IS v1); `:463-477` single-file branch (no `files` key) calls `file_length(dict)` at `:464` → repro dict has neither `length` nor `files` nor v2 tree, so `Err(InvalidField("length"))` at `:255-258` is the correct next error — same as `tests.rs:388-390` for missing-length. `NotATorrent` vs `InvalidField` is not "blurred": `NotATorrent` means "neither v1-pieces nor v2-tree present" (`:113-114`), and here v1-pieces IS present (empty), so `InvalidField("length")` is the precise diagnosis.
  - Repro fidelity: `repro_torrent2.rs:36-42` output `EMPTY-PIECES err InvalidField("length")` is exact, but tests the wrong conclusion — it shows correct error routing, not leniency. No wrongful acceptance demonstrated (contrast `v1_short_pieces_is_not_a_torrent` at `tests.rs:132-...` which rejects `len 1/5/19/21`). Verdict: no finding; kept numbered so F-numbering stays stable.

### F-11 (WITHDRAWN 2026-10-06 round 2 — no bug: guard at `:291` prevents the claimed `MAX+1`)
- Location: `crates/dc3-torrent/src/parse.rs:287-312` (`CappedPath::push`; guard at `:291-294`, separator at `:295-298`, `room` at `:299`, walk at `:302-308`).
- Originally claimed: `if !text.is_empty() { push('/'); chars+=1; } let room = MAX-chars;` — if `chars==4096`, push makes `4097 > MAX`, next `room=0`. `finish` trims trailing `/` so output correct, but invariant `chars <= MAX` violated.
- Why withdrawn (extensive proof): the quoted snippet omitted the guard that precedes it. Verified by reading `parse.rs:290-312` 2026-10-06: `push()` returns early `if self.chars >= PATH_MAX_CHARS { self.cut = true; return; }` (`:291-294`, `PATH_MAX_CHARS=4096` per `lib.rs:31`) BEFORE the separator push (`:295-298`). So the separator executes only when `chars <= 4095`, yielding `chars <= 4096 = MAX` after `+1` (via `saturating_add`, `:297`). Then `room = MAX - chars` (`:299`, `saturating_sub`) and the walk takes at most `room` chars (`:302-308` breaks when `count >= room`), so `chars_final = chars_after_sep + count <= chars_after_sep + (MAX - chars_after_sep) = MAX` always. First-push (empty `text`, no separator) obeys the same bound. The `chars` counter is private to the consumed builder (`finish(mut self)` discards it with the struct; the `/`-trim only shortens `text`), and `OpenDirs::close()` restores previously-saved in-range values — so there is not even a transient observable break.
- Repro: `/tmp/repro_misc.py` C2 (and identical `/tmp/audit-repro/repro_misc.py` C2) is bare arithmetic (`chars=4096; chars+1=4097`) that omits the `:291` guard; it demonstrates no code path. Fixed 2026-10-06 round 2 to label itself `[WITHDRAWN: guard at :291 prevents this]` with the guard analysis in comments. Output `4097 > MAX(4096)` is true arithmetic but inapplicable to the code.
- Verdict: no finding. Kept in place (numbered, marked withdrawn) so the F-numbering of all other entries stays stable.

### F-12 (LOW-MEDIUM): `is_password_key` substring over-blocks benign DB URL params
- Location: `crates/dhtcrawler3/src/config.rs:1159-1165`, used at `:1048` (`parse_db_url` rejects; comment says "Over-redaction is safe: this is display only" but rejection path is not display-only).
- Repro: `/tmp/repro_misc.py` (C1) — mirrors exact Rust predicate in Python
- Observed: `bypass=>True, compass=>True, passport=>True, passwordless=>True` (all contain `pass`), `user/sslmode=>False`. Innocent keys (`bypass`, `compass`, `passport`) rejected at load. Fail-closed but wrong vs knob meaning.

### F-13 (LOW): `metrics.listen` vs `web.listen` overlap only checks exact equality
- Location: `crates/dhtcrawler3/src/config.rs:672-674` (`if metrics.listen == web.listen && port != 0`).
- Repro: `/tmp/repro_misc.py` (C6)
- Observed by inspection: `0.0.0.0:8080` vs `127.0.0.1:8080` passes validation, second `bind` fails at runtime instead of at validation.

### F-14 (LOW): removal schedule doc `7->30->90` vs code `7,28,90`
- Location: `crates/dhtcrawler3/src/config.rs:212-214` doc, `admission.rs:108-110` (`×4`), `memstore.rs:101-110` (`base,4*base,cap`).
- Repro: `/tmp/repro_misc.py` (C7)
- Observed: `7*4=28 != 30`. Either comment or multiplier wrong; test-only store diverges from documented `30`.
- Correction (2026-10-06 round 2, scope WIDENED — proof below): "test-only store diverges" understates. Prod `dc3-store/src/crawler.rs:997-1008` implements the same schedule (`base`, then `base.saturating_mul(4).min(cap)`, then 90d `REMOVAL_COOLDOWN_CAP_DAYS` cap at `:993`; doc at `:995-996` says "the base, then ×4, then the 90d cap"), and `AdmissionTuning` doc at `admission.rs:108-110` likewise says "base, escalating ×4 per repeat to the 90d cap". So the `config.rs:212-214` comment (`7 -> 30 -> 90`) contradicts BOTH the prod store and the test mirror (`memstore.rs:101-110` `0|1 => base`, `2 => base*4`, `_ => cap`, with header "Mirrors the database store's base/×4/90d-cap schedule"). Either fix the doc to `7 → 28 → 90` or change the multiplier toward 30. Repro C7 arithmetic (`28 != 30`) unchanged.

### F-15 (MEDIUM, test-fidelity): memstore `gave_up` entries leak into `full`/`pending_depth`
- Location: `crates/dhtcrawler3/src/memstore.rs:246 (full = pending.len() >= max)`, `:462 (pending_depth)`, `:411-422 (pending_keys/claim filter gave_up)`, `give_up` never removes.
- Repro: `/tmp/repro_misc.py` (C3)
- Observed by inspection: `full`/`pending_depth` count `gave_up=true` entries forever, while `claim` filters them. Over time queue looks full and new non-priority keys dropped though nothing claimable remains; metric overcounts. Prod uses DB store; test-only fidelity issue.

### F-16 (LOW, info-leak): web `total` includes hidden/blocked hits while `results` filtered
- Location: `crates/dc3-web/src/handlers/search.rs:245-256` (`Ok(Found { total: results.total, torrents })` where `torrents` after `show_all` policy/`is_live` filter).
- Repro: `/tmp/repro_misc.py` (C4) + existing test `crates/dc3-web/tests/web/api.rs:58,61` (documents `total==3 len==2` with one hidden).
- Observed: comparing `total` vs returned count reveals how many hits moderated/hidden for that query. Blocked-query path itself safe (`total:0,blocked:true,query:""`).
- Correction (2026-10-06 re-check): test citation narrowed from `api.rs:56-61` to `:58,61` — the asserts are `assert_eq!(json["total"], 3)` at `:58` and `assert_eq!(results.len(), 2)` at `:61` (with `// The hidden hit is skipped` comment at `:60`); `:56-57` are only `per_page`/`query` asserts. Same-file HTML handler `search.rs:245-256` and JSON handler `api.rs:161-175` (`total: found.total` + filtered `results`) both exhibit the pattern. Repro C4 is inspection-only (prints the conclusion, runs no server); behaviour proven by code + the codifying test. Severity stays LOW (arguably intended pagination semantics).

### F-17 (MEDIUM, interop): peer `Unknown` ut_metadata requires `piece` + no-trailing
- Location: `crates/dc3-peer/src/wire.rs:403-410`, fatal at `fetch.rs:169` (parse `?`; successfully parsed `Unknown` is ignored at `:205`).
- Snippet: `other => { Unknown(other); piece()?; no_trailing()?; Ok(unknown) }`.
- Repro: `/tmp/repro_misc.py` (C5)
- Observed by inspection + BEP9: unknown `msg_type` should be ignored; requiring dict shape turns future extensions (missing `piece` or trailing/payload bytes) into `FetchError::Protocol` failing whole fetch. Tolerant choice would be `Ok(Unknown)` regardless.
- Correction (2026-10-06 re-check, PARTIALLY WRONG as originally written — extensive proof below):
  - Fatal-site fix: audit cited `fetch.rs:205` as fatal. `fetch.rs:205` is `MetadataMessage::Unknown(_) => {}` — i.e. a successfully parsed unknown is **ignored and the loop continues**, not fatal. The fatal point is the `?` at `fetch.rs:169` (`match wire::parse_metadata_message(body)?`): a *rejected* unknown (missing `piece`, trailing bytes) returns `Err(FetchError::Protocol)` there and aborts the whole fetch. Verified by reading `fetch.rs:155-206` 2026-10-06. Repro C5 text (`fetch.rs:205 fatal`) is therefore wrong on the line number; the behaviour (strict unknown shape can fail the fetch) is real but located at `:169`.
  - Blast-radius narrowing: audit said "new fields/payload". Extra dict *keys* are tolerated: `parse_metadata_message` (`wire.rs:365-410`) only reads `msg_type` via `dict.get_int(b"msg_type")` and `piece` via closure; it never rejects unknown keys, so a future extension adding a new dict key to an unknown type still parses (provided `piece` is present and there are no trailing bytes). What fails is (a) unknown type with no `piece` key, (b) unknown type with trailing bytes after the dict (including any appended payload bytes, since only `UT_DATA` reads `body[used..]`). Code proof: `wire.rs:366 let (dict, used) = decode_dict_prefix(body)?`, `:370-373 piece()` closure errors `without piece`, `:375-380 no_trailing` errors when `used != body.len()`, `:403-409` unknown arm calls both.
  - Existing tests codify the strictness deliberately: `crates/dc3-peer/src/tests.rs:272-280` asserts `d8:msg_typei9e5:piecei3ee => Ok(Unknown(9))` but `d8:msg_typei9ee` (no piece) `=> Err(Protocol)` with comment "malformed, not ignorable", plus `:282-293` trailing-bytes rejection. So the fix is a deliberate-compat-tradeoff judgment (as with F-23), not an accidental omission. Updated repro expectation: unknown type WITH `piece` and no trailing (even with extra dict keys) => `Ok(Unknown)` ignored at `fetch.rs:205`; unknown type WITHOUT `piece` or WITH trailing/payload => `Err(Protocol)` at `fetch.rs:169` failing the fetch. BEP9-literal tolerance would accept all three.
  - C5 remains inspection-only (no live peer); proof is the code + the codifying unit tests above.

### F-18 (MEDIUM, robustness): DHT lookup has no subnet/host diversity cap
- Location: `crates/dc3-dht/src/lookup.rs:166-202` (`seen_addrs: HashSet<SocketAddr>` exact-match only; checks `own_ids/family/canonical/dialable/own/seen_addrs/seen_ids`, no `/24`/`/64` or per-IP cap unlike `routing.rs`/`peer_store.rs`).
- Repro: `/tmp/repro_batch2.py` (D1) + `/tmp/audit-repro/repro_batch2.py` (same)
- Observed: same-IP different-port `(1.2.3.4,1002)` not in `{(1000),(1001)}` => admitted. One responder returning 16 distinct `(ID,same-IP:port)` can occupy large fraction of `MAX_CANDIDATES=128` over rounds. Mitigated by per-host `QuerySpacing` 1/s, but wastes budget.

### F-19 (LOW): DHT total pre-send wait can be 2× `max_send_wait`
- Location: `crates/dc3-dht/src/node.rs:564-583` (`query_gated`: `spacing.reserve(...,max_send_wait)` + `sleep` then `acquire_budget(max_send_wait)`; same shape in `query_scrape` `:687-706`), doc `config.rs:138-139` ("budget **or** spacing").
- Repro: `/tmp/repro_batch2.py` (D2)
- Observed by inspection: allows `max+max` (8s prod, 2s fast-tuning). Conservative (fails slower) but contradicts documented bound and deadline accounting.
- Correction (2026-10-06 re-check): line range fixed from `node.rs:485-527` to `:564-583` (and `:687-706` for the scrape path). Verified by reading both functions 2026-10-06: `:564-567 spacing.reserve(&addr, ..., tuning.max_send_wait)`, `:572-575 if !wait.is_zero() sleep(wait)`, `:576-583 acquire_budget(max_send_wait)`; scrape path `:687-690 reserve`, `:695-698 sleep`, `:699-706 acquire_scrape_budget(max_send_wait)`. Behaviour/interpretation unchanged (doc says "or", code allows bound+bound; conservative direction). Repro D2 is inspection-only (prints the conclusion); proof is the two code sites above.

### F-20 (LOW): DHT sampler `MAX_POPS_PER_PICK=256` can report `Empty` with eligible nodes remaining
- Location: `crates/dc3-dht/src/sampler.rs:57,210-250` (pick function span; loop at `:213-247`, verdict at `:248-249`; skipped stale/not-due dropped not re-queued; loop stops after 256 pops; 50k frontier).
- Repro: `/tmp/repro_batch2.py` (D3)
- Observed: pop 256 of 300 all-stale => `remaining=44` but verdict `Empty`. Workers refill/retry so liveness holds; single pick not complete scan. Skipped-not-due permanently evicted (must be re-`offer`ed) — by design but aggressive under churn.

### F-21 (INFO): DHT `wait_time` returns `ZERO` on overflow (fail-open in wait estimation; token still gated)
- Location: `crates/dc3-dht/src/ratelimit.rs:81-88`.
- Repro: `/tmp/repro_batch2.py` (D4)
- Observed: `checked_add` overflow (absurd `burst/rate`, e.g. `u32::MAX`) => `ZERO` => treated immediately available, spins on `MIN_BUDGET_WAIT` until `max_send_wait`. Unreachable with validated configs (≤500s); wrong direction (`MAX` would be fail-closed).
- Correction (2026-10-06 round 2, wording CLARIFIED — proof below): `ZERO` does NOT grant a token. `take()` (`ratelimit.rs:45-52`) uses `checked_mul`/`checked_add` and returns `None` on overflow, so `try_acquire` (`:60-68`) stays `false`. The caller (`node.rs:485-505` `acquire_budget`, `:507-527` scrape twin) computes `wait = budget.wait_time(now)` (`:494`/`:516`) then `wake = after(now, wait.max(MIN_BUDGET_WAIT))` (`:496`/`:518`) and loops until `give_up = after(now, max_wait)` (`:486`/`:508`). So the overflow consequence is a 1ms busy-spin (`MIN_BUDGET_WAIT`) until `max_send_wait`, not a bypass — fail-open in the wait estimate, fail-closed on the token. Repro D4 fixed 2026-10-06 round 2 to state "spins 1ms ... token NOT granted" with the `take()` + `node.rs` chain cited. Unreachable with sane configs (needs absurd `burst` so `now+capacity` overflows); INFO severity unchanged.

### F-22 (INFO): DHT `after()` overflow fail-open for sampler skips
- Location: `crates/dc3-dht/src/util.rs:15-19` (`checked_add.unwrap_or(now)`).
- Repro: `/tmp/repro_batch2.py` (D5)
- Observed: deadlines/expiry => `now` = expire immediately (fail-closed correct); `sampler::finish(next_at=after(now,skip))` => eligible immediately (fail-open, resample early). Durations ≤6h so practically impossible; inconsistency only with absurd tuning.

### F-23 (LOW, availability): DHT legacy concatenated-`values` dropped when len multiple of 18
- Location: `crates/dc3-dht/src/krpc.rs:379-396`.
- Repro: `/tmp/repro_batch2.py` (D6)
- Observed: `36/72/108 => mult6&&mult18 => v4concat_accepted=False` (falls to per-element path, `decode_peer(36B)==None` => dropped); `24 => True`. Old Mainline concatenated-V4 of those exact lengths loses all peers. Deliberate safety-vs-compat tradeoff (comment says so).

### F-24 (LOW): search `prefix_expansions("")` enumerates term dict
- Location: `crates/dc3-search/src/index.rs:547-554,634-653` (`upper=None` => range all terms => up to 200/field, up to 400 total — see correction).
- Repro: `/tmp/repro_batch2.py` (S3)
- Observed: internal callers always pass real trailing token (never empty), so API-misuse-only vocab disclosure. Suggest `None` on empty.
- Correction (2026-10-06 round 3, count precision — proof by reading `index.rs:547-554,634-653`):
  - `prefix_upper_bound("")` returns `None` (empty `pop→None` at `:621-630`), so `expand_prefix(..., "")` at `:639-642` sets `range = ge(b"")` with no `lt(upper)` — unbounded range over all terms. Per-segment loop at `:644-650` takes up to `PREFIX_MAX_EXPANSIONS=200` (`query.rs:32`) per segment (`taken` reset per segment at `:644`), `found: BTreeSet` dedups/sorts, final `take(200)` at `:652` caps each `expand_prefix` call to 200. `prefix_expansions` at `:547-554` unions two fields (`for field in [name, files] { found.extend(expand_prefix(...)?) }`) with no final cap (`:553 collect`), so the public API returns up to 200+200=400 (minus overlap), in `BTreeSet` (lexicographic) order — not "first 200" in index order. Prior "first 200" understates by up to 2× and misstates ordering. Repro S3 text fixed accordingly (behaviour/API-misuse-only conclusion unchanged; internal callers at `search.rs:111-130` + `handle.rs:94-115` pass a real trailing `single` token, `query.rs:139,199-203,238` guarantees non-empty/prefix-gated, so `""` never occurs naturally).

### F-25 (LOW, fail-closed): search `promote` check-then-act across lock
- Location: `crates/dc3-search/src/generations.rs:239-260` (`existing_index_dir` + validation before `lock()`, with re-check under it at `:242`).
- Repro: `/tmp/repro_batch2.py` (S4)
- Observed: pre-lock `existing_index_dir` at `:240` can go stale before `lock()` at `:241`, but the consequence is a clean error, not a dangling `CURRENT` (see correction). Narrow window, fail-closed; fix is re-check under lock (already present at `:242`).
- Correction (2026-10-06 re-check, original "dangling CURRENT" consequence WRONG — extensive proof below):
  - What was claimed: "concurrent `cleanup(older_than=ZERO)` can delete/rename target between check and `write_current`, leaving `CURRENT` dangling (`MissingGeneration` until re-promote)".
  - Why it cannot happen: (1) `promote` and `cleanup` take the *same* exclusive root flock. Proof: `promote` `:241 let _lock = self.lock()?`, `cleanup` `:269 let _lock = self.lock()?`, and `lock()` at `:420-440` opens `ROOT_LOCK_FILE` and `try_lock()`s it (exclusive file lock, `ROOT_LOCK_TIMEOUT` retry). The rename-to-trash at `cleanup:303-305` (`fs::rename(&dir, &trash)`) happens while that lock is held (`:269-310` scope); the actual deletion at `:314-318` happens after unlock but only on the already-renamed trash path, never on a live generation dir. So no concurrent rename/delete can interleave between `promote:241 lock` and `promote:245 write_current`. (2) Under the lock, `promote:242 drop(open_existing_index(&dir)?)` re-validates the target — this *is* the suggested "re-check under lock". If the dir vanished in the pre-lock window (`:240` check vs `:241` lock acquisition), `promote` fails cleanly at `:242` with `Err`, `CURRENT` untouched. (3) `cleanup` additionally never deletes the live generation (`:276 if generation == live ... continue`, with `live` read under the same lock at `:270`), and refuses to delete anything while `CURRENT` is broken (`:272 existing_index_dir(live)?`).
  - Remaining (downgraded) issue: the pre-lock check at `:240` can only waste a lock acquisition and return a clean error when racing; there is no TOCTOU dangling-`CURRENT` path. Repro S4 is print-only and its verdict line ("leaving CURRENT dangling") is disproven by the lock analysis above. Kept as LOW (fail-closed check-then-act shape, redundant pre-lock I/O) with corrected consequence.

### F-26 (LOW): store `tombstone_dead` early-`Ok(false)` relies on Drop-rollback
- Location: `crates/dc3-store/src/crawler.rs:766-798` (function span; early `Ok(false)` at `:778-780` no-row + `:789-791` `n==0`) with live tx holding change-feed shared lock + per-key lock.
- Repro: `/tmp/repro_batch2.py`
- Observed: correct today via `sqlx` Drop-rollback, inconsistent with explicit `tx.rollback()` in `set_setting`/HWM busy path. Fragile under future edits.
- Correction (2026-10-06 re-check): line range fixed from `772-791` to function span `766-798` (verified by reading `crawler.rs:766-798`: `:772 begin`, `:773 lock_change_shared`, `:774-777 SELECT key`, `:778-780 let Some(key) else return Ok(false)`, `:781 lock_keys`, `:782-788 TOMBSTONE_DEAD_SQL`, `:789-791 if n==0 return Ok(false)`, `:796 commit`). Behaviour/interpretation unchanged; repro remains inspection-only (no DB in `/tmp` repro).

### F-27 (LOW): store repeat `deny` re-tombstones + re-bumps `change_seq`
- Location: `crates/dc3-store/src/crawler.rs:1174-1177` (`TOMBSTONE_SQL` unconditional).
- Repro: `/tmp/repro_batch2.py`
- Observed: 2nd `deny` same key returns `tombstoned:1` again (test-codified) + moves `change_seq`, churning indexer for no-op. `blocked` daily counter correctly guarded by `newly_denied`; tombstone not.

### F-28 (LOW): store `removal_cooldown_with_base` upgrades `0` to 1 day
- Location: `crates/dc3-store/src/crawler.rs:997-1000` (`base_days.max(1)`).
- Repro: `/tmp/repro_batch2.py`
- Observed: configured `0` ("no cooldown") silently becomes 1-day (÷4 with evidence) block. Fail-closed but surprising.
- Correction (2026-10-06 round 3, reachability qualified — proof): validated `Config` cannot express `0`: `crates/dhtcrawler3/src/config.rs:805-810` `check_range("crawl.removal_cooldown_days", 1, 365)` rejects 0 at load, so "configured 0" is unreachable via config file/CLI. Reachable only via direct-API construction (`Store::removal_cooldowns` / `AdmissionTuning` with `base_days=0`, as in unit tests). Behaviour claim (`0→max(1)→1d`, then `÷4 with evidence` at `crawler.rs:1010+`) unchanged; scope is direct-API footgun, not operator-config surprise. F-14's `7→28→90` doc fix remains the operator-facing item.

### F-29 (LOW): web `router()` skips `WebConfig::validate()`
- Location: `crates/dc3-web/src/app.rs:193` (`router()->build()->Site::new` trims only) vs `serve.rs:47` (`validate()`).
- Repro: `/tmp/repro_batch2.py` (W1)
- Observed: `base_url="evil"` => `site.base_url="evil"` => `csrf::post_allowed` 403s every legit POST + malformed `security_txt` Canonical. Only safe because `serve()` validates; public `router()` API undocumented must-validate-first.

### F-30 (MEDIUM): web `POST /report` stores without torrent-existence check
- Location: `crates/dc3-web/src/handlers/report.rs:47-124` (no `get_by_key`/`is_live`; `get_by_key_calls==0` in form test).
- Repro: `/tmp/repro_batch2.py` (W5)
- Observed: any well-formed 40/64-hex key (even random/nonexistent) yields stored row. Mitigated by `Report` 3/min + `ReportsFull` 503, but distributed spam over many IPs/keys can fill queue. Confirm `Store::submit_report` rejects unknown keys; if not, add existence check.
- Correction (2026-10-06 round 3, confirmation answered with proof — `submit_report` does NOT reject unknown keys): DB migration `submit_report.sql:103-120` declares `v_torrent_id ... NULLABLE` (comment: tombstoned/denied/missing → `NULL`), and `:157-159 INSERT ... (torrent_id NULLABLE) ... RETURNING` always stores even when the key resolves to no torrent row; handler `report.rs:47-124` performs only `parse_key` + `csrf::post_allowed` + form/length checks + `submit_report` (grep: zero `get_by_key`/`is_live` calls in the handler; form test asserts `get_by_key_calls==0` at `tests/web/report.rs:50`). So random 40/64-hex keys are stored with `torrent_id=NULL` — the "confirm" question is answered: no existence rejection. MEDIUM stands (spam-fill mitigated only by per-IP 3/min + queue-full 503).

### F-31 (LOW-MEDIUM, liveness): binary fetch busy/rate-limited peers silently dropped for key
- Location: `crates/dhtcrawler3/src/fetch.rs:612-621` (`Err(denied) => counter only`, no requeue).
- Repro: `/tmp/repro_batch2.py`
- Observed: `Busy/RateLimited/Full` discards peer for this key. Under load key can return `Failed/NoPeers` despite usable momentarily-busy peers. No retry within same `obtain`.

### F-32 (LOW): binary fetch transient `complete` error bypasses `fail` accounting
- Location: `crates/dhtcrawler3/src/fetch.rs:726-729` (`StoreError` without `fail()/give_up()`).
- Repro: `/tmp/repro_batch2.py`
- Observed: `attempts/next_attempt` unchanged; retries only after lease expiry (120s) with identical `attempts`, never reaching `MAX_FETCH_ATTEMPTS` via this path. Inconsistent with `give_back/give_up`.

### F-33 (LOW): binary admission `non_zero(0)->1` footgun
- Location: `crates/dhtcrawler3/src/admission.rs:84-89`.
- Repro: `/tmp/repro_batch2.py`
- Observed: `0` tuning (intended disable) becomes capacity `1` instead of error/disabled. Only reachable via tests today (no config knob), but API footgun.

### F-34 (LOW-MEDIUM): binary admission full `removal_cache` cleared wholesale + fail-open
- Location: `crates/dhtcrawler3/src/admission.rs:629-631` (`clear()` incl `Blocked`), `:622-627` (fail-open on `removal_cooldowns` error admits unchecked).
- Repro: `/tmp/repro_batch2.py`
- Observed: next flush re-queries DB for everything; combined burst can admit cooldown keys.

### F-35 (LOW-MEDIUM, shutdown): binary scrape ignores `stop` while draining
- Location: `crates/dhtcrawler3/src/scrape.rs:192-214` (spawn loop *does* check `stop` at `:196-198`; **drain** `while let Some(done) = in_flight.next().await` at `:207-212` does not) + `scrape_one` at `:282-305` (no cancellation, only `lookup_timeout`).
- Repro: `/tmp/repro_batch2.py`
- Observed: claimed batch (default 64, max 1000, concurrency 3, lookup 60s) delays shutdown until done or outer 30s `JoinSet::shutdown()` aborts. Should abort early.
- Correction (2026-10-06 re-check, original lines/batch-size PARTIALLY WRONG — extensive proof below):
  - Line fix: audit cited `:207-213` as the whole finding. Verified by reading `scrape.rs:192-214` 2026-10-06: the spawn loop at `:195-206` DOES check stop (`:196-198 if stop.is_cancelled() { break; }`), so newly claimed items stop being queued promptly; only the drain at `:207-212` (`let mut batch = ...; while let Some(done) = in_flight.next().await { ... }`) lacks a `stop` check and waits for already-spawned futures. `scrape_one` at `:282-305` confirmed to have no `stop`/cancellation parameter — it awaits `is_denied` then `scrape_v4_first(..., self.tuning.lookup_timeout, ...)` (`:295-302`) with no select on `stop`.
  - Batch-size fix: audit said "up to 1000". `config.rs:248` default `scrape_batch: 64`, `:772` validation `check_range("crawl.scrape_batch", 1, 1000)`, `:253` default `scrape_concurrency: 3`, `:252` default `scrape_lookup_timeout_secs: 60` (asserted at `config.rs:1556-1561`). So a drain batch is 64 by default (1000 only if operator raises it to max). Worst-case drain delay ≈ one `lookup_timeout` (60s default) × queued depth/concurrency, bounded externally by the caller's 30s `JoinSet::shutdown()` abort (the same outer-abort pattern as `crawl.rs:298-305`). Consequence (shutdown delayed until drain done or outer abort) stands with corrected numbers; the "ignores stop" wording now precisely scopes to the drain + `scrape_one`, not the spawn loop.
  - Repro remains inspection-only (prints `scrape.rs:207-213`); proof is the line-level reading above plus the config defaults.

### F-36 (LOW, ops): binary `all` opens 3× pools + `run_all` no shutdown timeout
- Location: `crates/dhtcrawler3/src/roles.rs:170-172` (crawl+index+web each `max_connections`, default 16 => 48 conns), `:200-221` (cancel then `join_next` indefinitely; hung role hangs `all` forever).
- Repro: `/tmp/repro_batch2.py`
- Observed by inspection + arithmetic.
- Correction (2026-10-06 round 3, citation narrowed): "only 2nd signal escapes" removed from the location claim — `:200-221` proves indefinite `join_next` after `cancel()` (no timeout), but the second-signal escape lives in signal handling outside this span and was not re-verified here. 3×16=48 arithmetic (`config.rs:154` default 16, `roles.rs:21` each `Store::connect_with(..., max_connections)`) and no-timeout consequence stand.

### F-37 (MEDIUM, debuggability): binary crawl admission panic cause discarded
- Location: `crates/dhtcrawler3/src/crawl.rs:284-287` (`_ = &mut admission_task` drops `JoinError`; generic `"admission stopped unexpectedly"` at `:320-321`; non-early path at `:311-312` logs `%e`, early does not).
- Repro: `/tmp/repro_batch2.py`
- Observed: panic/cancel cause lost.
- Correction (2026-10-06 round 3, citation extended): `select!` at `:284-287` (`() = stop.cancelled() => false, _ = &mut admission_task => true`) discards the `JoinError` (panic vs cancel vs Ok collapsed to `bool`). Early-exit path returns generic `CrawlError::Task("admission stopped unexpectedly")` at `:320-321` with no cause (`%e` never logged); non-early path at `:309-314` does `if let Err(e) = (&mut admission_task).await { tracing::error!(error=%e, ...) }` at `:311-312`. Behaviour claim unchanged; location now cites both the discard (`:284-287`) and the generic error (`:320-321`).

## Verified clean (no finding — checked, repro would fail)
- `dc3-bencode` infohash raw spans, canonical encoder (sorted via `BTreeMap`, `i.to_string()` never `-0`), depth/item off-by-ones (`>=max`, `>max`) — correct.
- `dc3-dht` XOR/distance/prefix, bucket split/eviction, token (SHA1+8B trunc, 2-secret window, const-time), bloom BEP-33 math, byte orders (ports BE, bloom LE, BEP42 BE, Teredo/6to4 shifts) — correct per subagent verification; no `unwrap/index` on hot paths.
- `dc3-peer` handshake 68B/LTEP/extended `20/0`, UT 0/1/2, `total_size`+`piece`+`expected_len`, duplicate rejection, `try_reserve`, `verify` SHA1/trunc-SHA256 — correct.
- `dc3-search` injection (tokenized only), empty query (`NoTerms`), pagination (`u64+saturating`, cap 2450), `safe_seek` contract — correct.
- `dc3-store` SQL injection (all `$n` bound, static ORDER/LIMIT), pagination caps, change-feed locking (`lock_change_shared` first, `CACHE 1`, HWM `55P03`), denylist 20B-prefix semantics, time handling (`now()` UTC, `remaining_since` skew-safe) — correct.
- `dc3-web` XFF (untrusted ignores, trusted right-to-left + caps), XSS (no `|safe`, auto-escape + `sanitize_display` + magnet/mailto encoding + JSON `<>&` escape), pagination bounds, no open redirect (sole `/`), no traversal (embedded statics, `parse_key` caps), error leakage (generic 503, query never logged) — correct.

## Explicitly excluded (subagent claim, could not prove / needs prod DB/net)
- DHT `routing.rs:411-421` move-blocked-by-Bad vs insert-evicts (inconsistency real in code but liveness impact needs live routing table repro — not proven, excluded).
- DHT `node.rs:976-994 on_reply` dead code (harmless, no behaviour change — excluded as non-issue).
- DHT `net.rs` per-endpoint cap, `config.rs` cross-field validation, `compact.rs` compat-addr (info only — excluded).
- Peer `budget.acquire` deadline doc, `discard` EINTR, 4MiB discard budget, keep-alive spin, `MetadataSizeInvalid(0)`, `split_frame` 32-bit, DHT-bit, seeder spawn (low/theoretical/test-only — excluded pending targeted repro).
- Policy P2/P5/P6 (same root as F-06 or info — folded into F-06, not separate claims).
- Search S1/S2/S5/S6/S7 (availability/semantics/footguns, not correctness bugs — excluded from correctness audit).
- Store T5 (approximate load-shedding, errs to availability — excluded).
- Web F2/F3/F6/F7/F8 (validator gaps/framework-subtlety/cosmetic — F3 needs axum-version-specific repro; excluded pending live-router repro).
- Binary items 1-5,7,10-12,15,18-26 not listed above (config/CLI/crawl/admission/index/metrics/signals/admin — reviewed; either fail-closed/ops-only or needs DB/net to prove; excluded rather than claim without repro).

## Coverage checklist
- [x] dc3-bencode (F-01..F-04 + clean; F-04 consequence narrowed round 2 to hypothetical, hit-count fixed round 3 to 3 in-crate +1 docs — proof inline)
- [x] dc3-core (via torrent/policy/web magnet paths — no standalone issue found)
- [x] dc3-torrent (F-05 downgraded HIGH→LOW round 3, F-08 WITHDRAWN round 3, F-09 downgraded MEDIUM→LOW round 3, F-10 WITHDRAWN round 3, F-11 WITHDRAWN round 2 — proofs inline)
- [x] dc3-dht (F-18..F-23 + clean; F-20 range widened to :210-250, F-21 clarified token-still-gated; 3 info-only excluded)
- [x] dc3-peer (F-17 only proven; rest excluded pending repro)
- [x] dc3-policy (F-06..F-07 + clean; F-06 `p1thc` mechanism clarified: leet `1→i`, not split)
- [x] dc3-search (F-24 count fixed round 3 to ≤200/field ≤400 total, F-25 downgraded; availability items excluded)
- [x] dc3-store (F-26..F-28 + clean; F-28 reachability qualified round 3 to direct-API-only)
- [x] dc3-web (F-16, F-29..F-30 + clean; F-30 confirmation answered round 3: `torrent_id NULLABLE`, no rejection; framework-subtle excluded)
- [x] dhtcrawler3 binary (F-12..F-15, F-31..F-37 + clean; F-14 scope widened round 2 to prod+test; F-36/F-37 citations narrowed/extended round 3)
- [ ] workspace config (Cargo.toml/clippy/d deny/Dockerfile/deploy/scripts — not yet audited; TODO if scope expands)

> Status: 37 findings filed, each with `/tmp` repro; round 2 withdraws 1 (F-11) leaving 36 valid; round 3 withdraws 2 more (F-08, F-10) leaving 34 valid + downgrades F-05 HIGH→LOW and F-09 MEDIUM→LOW. No fixes applied (audit only).
> Re-check 2026-10-06: all 37 findings re-verified (source re-read + repros re-run). 8 entries corrected with inline `Correction (2026-10-06 re-check ...)` proof blocks: F-01 (line + repro-scope), F-09 (lossy mechanism), F-16 (test lines), F-17 (fatal site 205→169 + extra-keys tolerated), F-19 (lines 485-527→564-583/687-706), F-25 (dangling-CURRENT disproven, downgraded), F-26 (lines 772-791→766-798), F-35 (lines + batch default 64/max 1000 + spawn-does-check-stop). Live Rust repro outputs re-captured 2026-10-06 (see Re-check appendix). No source fixes applied (audit only).
> Re-check round 2 (2026-10-06, this pass): every finding re-checked a second time via parallel subagent sweeps (bencode / torrent / policy / DHT / peer+web+search / store+binary) + direct source re-reads by the auditor. 5 further corrections with inline `Correction (2026-10-06 round 2 ...)` proof blocks: F-04 (SHA1("") fail-open narrowed to hypothetical-future-caller + saturating_add unreachable proof; zero production `raw_value` callers), F-11 (WITHDRAWN: `:291` guard proof `chars<=MAX` always; C2 repro omits guard), F-14 (scope widened: prod `crawler.rs:997-1008` also ×4, not test-only), F-20 (range `210-247`→`210-250`), F-21 (clarified `ZERO` spins 1ms, token still gated via `take()`). Repro scripts fixed in both `/tmp/*.py` and `/tmp/audit-repro/*.py` (identical pairs kept in sync, `diff` clean): C2 marked WITHDRAWN, C4 `:56-61`→`:58,61`, C5 fatal `:205`→`:169` + extra-keys tolerated, D2 `:485-527`→`:564-583`+`:687-706`, D3 comment `213-248`→`210-250`, D4 token-still-gated wording, S4 rewritten to fail-closed + `:242` re-check + shared-flock proof, T-store `:772-791`→`:766-798`, B-scrape `:207-213`→ drain `:207-212` + spawn-does-check. Both `.py` pairs re-run exit 0; outputs re-captured below. No source fixes applied (audit only).
> Re-check round 3 (2026-10-06, this pass — requested "double check every issue, repro correct, interpretation proper, extensive proof for fixes"): 6 parallel subagent sweeps re-ran all Rust repros + both `.py` pairs (exit 0, `diff` clean) and re-read every cited Rust site; auditor directly re-read contested sites (`parse.rs:53-114,186-221,265-274,340-412,428-440,498-559`, `lib.rs:28-43`, `tests.rs:118-130,225-269,880-957`, `text.rs:11-26`, `index.rs:547-554,634-653`, `config.rs:805-810`, `crawl.rs:283-323`, `roles.rs:21,170-172,200-221`) + BEP47 (`[".pad","N"]` recommended, `attr p` authoritative) + `grep raw_value` (3 in-crate +1 docs). 10 entries corrected/withdrawn with inline `Correction/Why withdrawn (2026-10-06 round 3 ...)` proof blocks: F-04 (hit-count `exactly 3`→3 in-crate+1 docs), F-05 (HIGH→LOW: `parse.rs:53-59` documents past-budget `TooMuchText`, tested bound `budget+raw.len()` at `tests.rs:888` `5891510<7266095`, `lib.rs:42-43` is budget-clamp not visited-bound), F-08 (WITHDRAWN: BEP47 top-level + `tests.rs:235-236,241` + safe-direction over-count), F-09 (MEDIUM→LOW: code-matches-own-docs `parse.rs:340-344` + display-only + lossy-leg proof), F-10 (WITHDRAWN: `tests.rs:118-130` zero-length valid + `InvalidField(length)` correct routing), F-24 (`first 200`→≤200/field ≤400 total via `:547-554` union + `:652` take), F-28 (config `0` unreachable via `check_range 1..365`, direct-API-only), F-30 (confirmation answered: `submit_report.sql:103-120,157-159` `NULLABLE`, always stores), F-36 (removed unproven `2nd-signal` from `:200-221` claim), F-37 (added `:320-321` generic-error site). No source fixes applied (audit only).

## Re-check appendix (2026-10-06): method, repro fidelity, live outputs
- Method: every cited Rust location re-read; every `/tmp/audit-repro/src/bin/repro_*.rs` re-run via `cargo run --manifest-path /tmp/audit-repro/Cargo.toml --bin <name>`; both `/tmp/repro_misc.py` and `/tmp/repro_batch2.py` re-run via `python3`; `diff /tmp/repro_misc.py /tmp/audit-repro/repro_misc.py` and batch2 diff both IDENTICAL.
- Repro fidelity (important for interpreting "every claim has a repro"): F-01/F-02/F-03/F-05/F-06/F-07/F-08/F-09/F-10 are **live Rust repros** (execute the real crates; outputs re-captured below). F-08/F-10 repros prove the behaviour but the behaviour is correct (WITHDRAWN round 3 — repro outputs match the tested contract: `a/.pad/f` listed per `tests.rs:241`, `EMPTY-PIECES InvalidField(length)` per missing-length routing). F-05 repro numbers are exact but the "exceeds documented bound" interpretation is corrected round 3 (documented bound is `budget+raw.len()` with `Err`, both satisfied: `5891510 < 7266095`, `result=Err(TooMuchText)`). F-04 is a **disclosed inspection finding** (repro demonstrates the pattern/arithmetic, not a live trigger — audit text already says so; round 2 proves the `SHA1("")` consequence hypothetical: zero production `raw_value` callers, real path hashes whole `info` at `parse.rs:124-125`). F-11 is **withdrawn round 2** (C2 repro omits the `:291` guard; marked WITHDRAWN in both `.py` copies). F-12..F-37 Python scripts are **faithful inspection mirrors** (re-implement the exact predicate, e.g. `lower.contains("pass")` mirroring `to_ascii_lowercase().contains("pass")` for ASCII keys, `len%6/%18` arithmetic mirroring `krpc.rs:379-396`, exact-dedup set mirroring `seen_addrs: HashSet<SocketAddr>`): conclusion proven by the cited Rust code, not by the script output. Each mirror was compared line-by-line against the Rust source during re-check and found faithful except where corrected inline (F-17/F-25/F-35) and round 2 (C4/C5/D2/D3/D4/S4/T-store/B-scrape line/wording fixes; both `.py` pairs kept identical, `diff` clean).
- Live outputs 2026-10-06 (exact):
  - `repro_bencode`: `max_items=3 => Err("Error { kind: TooManyItems, pos: 10 }")`, `max_items=5 => Ok(Dict...)` (F-02); `huge-len-digits => Some("IntegerOverflow @19")`, `u64max-len => Some("StringTooLong @21")` (F-03); `get(99..100).unwrap_or_default() => []`, `usize::MAX.saturating_add(1) => 18446744073709551615` (F-04 pattern demo).
  - `repro_bencode_drop`: `decoding depth=50000 iteratively... / decode OK. Now dropping in 512KiB-stack thread... / thread '<unknown>' (...) has overflowed its stack / fatal runtime error: stack overflow, aborting / Aborted (core dumped)` exit 134 (F-01; second `to_owned_value` half never runs in same process — see F-01 correction).
  - `repro_policy`: `pthc=>true/true`, `xpthc=>false/true`, `p.t.h.c/p t h c/p-t-h-c/p_t_h_c/p1thc=>false/false`, `normalise p.t.h.c => ["p","t","h","c"]`, `normalise p1thc => ["p1thc"]`, `P4: 1MB comment line accepted, len=1` (F-06/F-07).
  - `repro_torrent2`: `PAD-NESTED raw_len=88 file_count=1 files=[FileEntry { path: "a/.pad/f", size: 1 }]`, `PAD-TOP file_count=0 files=[]` (F-08 WITHDRAWN round 3 — outputs match tested contract `tests.rs:241`); `UTF8 pref=[129,48] leg=fallback => files=[FileEntry { path: "�0", size: 1 }]` (F-09 downgraded LOW round 3, lossy leg — see correction); `EMPTY-PIECES err InvalidField("length")` (F-10 WITHDRAWN round 3 — correct routing, repro dict lacks `length`; valid zero-length case is `tests.rs:118-130`).
  - `repro_torrent_budget`: `result=Err(TooMuchText) raw.len=1453219 budget=5812876 chars_visited=5891510 calls=21434 exceeds_budget=true excess=78634` (F-05 downgraded HIGH→LOW round 3: numbers match, but `5891510 < 5812876+1453219=7266095` tested bound at `tests.rs:888` and `result=Err` per `parse.rs:53-59,112-114`; `exceeds_budget` ≠ exceeds-documented-bound). Sibling `repro_torrent.rs` is NOT evidence for F-05 (5000×200-char case stays under budget, `is_err=false`) and its hand-rolled T-UTF8 vector is malformed (`InvalidLength @77` panic before T-V2) — audit correctly cites only `repro_torrent_budget.rs`.
  - `repro_misc.py` C1/C7 outputs match audit to the token (`bypass/compass/passport/passwordless=>True`, `28 != 30`); C2 now prints WITHDRAWN (guard proof; old `4097 > MAX(4096)` arithmetic kept only as guard-ignored illustration); C3-C6 print inspection conclusions whose cited Rust lines were re-read and confirmed (with F-16/F-17 corrections noted; C4 `:58,61`, C5 fatal `:169` + extra-keys tolerated, both fixed round 2).
  - `repro_batch2.py` all sections exit 0; D1/D3/D6 arithmetic (`(1.2.3.4,1002)` admitted; `popped=256 remaining=44`; `36/72/108=>False, 24=>True`) mirrors confirmed faithful to `lookup.rs:166-202`, `sampler.rs:210-250`, `krpc.rs:379-396`. Round-2 wording/line fixes: D2 `:564-583`+`:687-706`, D3 comment `:210-250`, D4 token-still-gated, S4 fail-closed rewrite, T-store `:766-798`, B-scrape drain `:207-212` + spawn-does-check.
- Verdict summary round 3 (supersedes prior summary; every entry re-verified this pass): F-02/F-03/F-06/F-07 VERIFIED exactly (live outputs match); F-01/F-16/F-19/F-26 VERIFIED with minor line/mechanism corrections (proof inline); F-04 VERIFIED as disclosed inspection finding with consequence NARROWED round 2 + hit-count fixed round 3 (3 in-crate +1 docs; hypothetical-only proof inline); F-05 DOWNGRADED HIGH→LOW round 3 (numbers match, doc reading wrong: documented bound is `budget+raw.len()` with `Err`, both satisfied); F-08 WITHDRAWN round 3 (BEP47 top-level + tested contract + safe-direction proof); F-09 DOWNGRADED MEDIUM→LOW round 3 (live output matches, lossy leg proven, code-matches-own-docs, display-only); F-10 WITHDRAWN round 3 (empty-pieces valid per `tests.rs:118-130`, `InvalidField(length)` correct routing); F-11 WITHDRAWN round 2 (no bug — `:291` guard proof inline); F-12/F-13/F-15/F-18/F-20/F-21/F-22/F-23/F-27/F-29/F-31/F-32/F-33/F-34 VERIFIED (inspection mirrors faithful, Rust lines confirmed); F-24 VERIFIED with count fixed round 3 (≤200/field ≤400 total); F-28 VERIFIED with reachability qualified round 3 (direct-API-only); F-30 VERIFIED with confirmation answered round 3 (`NULLABLE`, always stores); F-36/F-37 VERIFIED with citation fixes round 3; F-14 VERIFIED with scope WIDENED round 2 (prod also ×4 — proof inline); F-17 PARTIALLY WRONG as written (fixed: fatal at :169 not :205, extra keys tolerated — proof inline); F-25 consequence WRONG as written (dangling CURRENT impossible under exclusive flock + `:242` re-check — downgraded with proof inline); F-35 PARTIALLY WRONG as written (fixed lines + spawn-does-check + default-64/max-1000 — proof inline). Totals: 37 filed, 3 withdrawn (F-08/F-10/F-11) → 34 valid.
- Round-2 method: 6 parallel subagent sweeps (bencode F-01..F-04 / torrent F-05,F-08..F-11 / policy F-06..F-07 / DHT F-18..F-23 / peer+web+search F-16,F-17,F-24,F-25,F-29,F-30 / store+binary F-12..F-15,F-26..F-28,F-31..F-37) each re-read every cited Rust location, re-ran every repro (`cargo run --manifest-path /tmp/audit-repro/Cargo.toml --bin <name>`, `python3 /tmp/*.py`), and returned VERIFIED/PARTIALLY WRONG/WRONG per finding with exact lines/snippets/outputs; auditor then directly re-read the contested sites (`parse.rs:287-312` + `lib.rs:31`, `decode.rs:61-72,187-190` + `lib.rs:157-161,208-210` + `tests.rs:88-94` + torrent `parse.rs:124-125`, `config.rs:212-214` + `crawler.rs:990-1008` + `memstore.rs:100-110`, `ratelimit.rs:40-89` + `node.rs:485-527,560-589`, `sampler.rs:208-250`, `scrape.rs:192-214`) and grep-verified `raw_value` callers before writing the round-2 corrections above.

## Coverage checklist (stale template — superseded by the checked list above; kept for history)
- [ ] dc3-bencode
- [ ] dc3-core
- [ ] dc3-torrent (parse + text)
- [ ] dc3-dht (routing, lookup, bloom, token, responder, sampler, peer_store, net, stats, state, ratelimit, util)
- [ ] dc3-peer (wire, fetch, assembly, seeder)
- [ ] dc3-policy (lib + normalise)
- [ ] dc3-search (query, index, handle, tokenizer, safe_seek, generations)
- [ ] dc3-store (types, indexer, crawler, web, admin)
- [ ] dc3-web (app, backend, handlers, csrf, ratelimit, client_ip, middleware, json, format, render, serve, static_files, telemetry, listener, templates)
- [ ] dhtcrawler3 binary (main, cli, config, crawl, fetch, peers, admission, index, stores, memstore, scrape, policy, roles, admin, healthcheck, web, metrics_server, logging, signals)
- [ ] workspace config (Cargo.toml, clippy.toml, deny.toml, Dockerfile, deploy/, scripts/)

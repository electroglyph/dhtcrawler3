use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use proptest::prelude::*;
use tantivy::tokenizer::{TextAnalyzer, TokenStream, Tokenizer};
use unicode_normalization::char::is_combining_mark;

use crate::*;

const HEAP: usize = 20 * 1024 * 1024;

fn toks(s: &str) -> Vec<String> {
    Dc3Tokenizer::new().token_texts(s)
}

fn cjk1(s: &str) -> Vec<String> {
    Cjk1Tokenizer::new().token_texts(s)
}

// ---------------------------------------------------------------- tokenizer

#[test]
fn latin_splits_on_punctuation() {
    assert_eq!(
        toks("Ubuntu-22.04_desktop[amd64].iso"),
        ["ubuntu", "22", "04", "desktop", "amd64", "iso"]
    );
    assert_eq!(toks("  a  b\tc\n"), ["a", "b", "c"]);
    assert!(toks(".-_[]()!@#").is_empty());
}

#[test]
fn accents_are_folded() {
    assert_eq!(toks("Café"), ["cafe"]);
    assert_eq!(toks("Cafe\u{301}"), ["cafe"]);
    assert_eq!(toks("Straße Ångström"), ["strasse", "angstrom"]);
}

#[test]
fn fullwidth_is_nfkc_normalised() {
    assert_eq!(toks("ＵＢＵＮＴＵ　２２"), ["ubuntu", "22"]);
    // Halfwidth katakana becomes fullwidth.
    assert_eq!(toks("ｶﾀｶﾅ"), ["カタ", "タカ", "カナ"]);
}

#[test]
fn cjk_bigrams_with_positions() {
    let tokens = Dc3Tokenizer::new().tokens("東京大学");
    let got: Vec<(&str, usize)> = tokens
        .iter()
        .map(|t| (t.text.as_str(), t.position))
        .collect();
    assert_eq!(got, [("東京", 0), ("京大", 1), ("大学", 2)]);
    // Offsets point into the original text.
    assert_eq!(tokens[1].offset_from, 3);
    assert_eq!(tokens[1].offset_to, 9);
}

#[test]
fn single_cjk_character_emits_itself() {
    assert_eq!(toks("猫"), ["猫"]);
    assert_eq!(toks("猫 犬"), ["猫", "犬"]);
}

#[test]
fn mixed_run_splits_at_script_boundaries() {
    let tokens = Dc3Tokenizer::new().tokens("Ubuntu東京2024");
    let got: Vec<(&str, usize)> = tokens
        .iter()
        .map(|t| (t.text.as_str(), t.position))
        .collect();
    assert_eq!(got, [("ubuntu", 0), ("東京", 1), ("2024", 2)]);
}

#[test]
fn hangul_and_kana() {
    assert_eq!(toks("한국어"), ["한국", "국어"]);
    assert_eq!(toks("ひらがな"), ["ひら", "らが", "がな"]);
    // The prolonged sound mark belongs to the katakana run.
    assert_eq!(toks("ラーメン"), ["ラー", "ーメ", "メン"]);
}

#[test]
fn overlong_tokens_are_dropped() {
    let long = "a".repeat(MAX_TOKEN_BYTES + 1);
    let ok = "b".repeat(MAX_TOKEN_BYTES);
    let tokens = Dc3Tokenizer::new().tokens(&format!("x {long} {ok} y"));
    let got: Vec<(&str, usize)> = tokens
        .iter()
        .map(|t| (t.text.as_str(), t.position))
        .collect();
    // The dropped token leaves a position gap so phrase queries cannot
    // match across it.
    assert_eq!(got, [("x", 0), (ok.as_str(), 2), ("y", 3)]);
}

#[test]
fn dropped_token_leaves_position_gap() {
    // `x <overlong> ok` must not yield adjacent positions.
    let long = "a".repeat(MAX_TOKEN_BYTES + 1);
    let tokens = Dc3Tokenizer::new().tokens(&format!("x {long} ok"));
    let got: Vec<(&str, usize)> = tokens
        .iter()
        .map(|t| (t.text.as_str(), t.position))
        .collect();
    assert_eq!(got, [("x", 0), ("ok", 2)]);
}

#[test]
fn cjk1_emits_only_cjk_characters() {
    let tokens = Cjk1Tokenizer::new().tokens("Ubuntu東京2024 한국-ｶﾅ!");
    let got: Vec<(&str, usize)> = tokens
        .iter()
        .map(|t| (t.text.as_str(), t.position))
        .collect();
    assert_eq!(
        got,
        [
            ("東", 0),
            ("京", 1),
            ("한", 2),
            ("국", 3),
            ("カ", 4),
            ("ナ", 5)
        ]
    );
    // Offsets point into the original text.
    assert_eq!((tokens[1].offset_from, tokens[1].offset_to), (9, 12));
    assert!(cjk1("ubuntu 22.04 café").is_empty());
    assert!(cjk1("").is_empty());
    // Same normalisation as `dc3`: a lone character gives the same token.
    assert_eq!(cjk1("ラーメン"), ["ラ", "ー", "メ", "ン"]);
    assert_eq!(cjk1("学"), toks("学"));
    // A decomposed kana composes; the character stays one token.
    assert_eq!(cjk1("か\u{3099}"), ["が"]);
    assert_eq!(cjk1("学\u{301}"), ["学\u{301}"]);
    // A kana voicing mark after a Latin letter belongs to the Latin word.
    assert!(cjk1("a\u{3099}").is_empty());
    assert_eq!(toks("a\u{3099}"), ["a\u{3099}"]);
}

#[test]
fn cjk_unigram_detection() {
    assert!(is_cjk_unigram("学"));
    assert!(is_cjk_unigram("ー"));
    assert!(is_cjk_unigram("学\u{301}"));
    assert!(!is_cjk_unigram("大学"));
    assert!(!is_cjk_unigram("a"));
    assert!(!is_cjk_unigram(""));
    assert!(!is_cjk_unigram("\u{301}"));
    let word = |q: &str| parse_query(q).unwrap().words[0].is_cjk_unigram();
    assert!(word("学"));
    assert!(word("-学 x"));
    assert!(word("\"学\""));
    assert!(word("学!"));
    assert!(word("ｶ"));
    assert!(!word("大学"));
    assert!(!word("学x"));
    assert!(!word("x"));
}

#[test]
fn works_through_text_analyzer() {
    let mut analyzer = TextAnalyzer::from(Dc3Tokenizer::new());
    let mut stream = analyzer.token_stream("Foo.Bar");
    let mut out = Vec::new();
    while stream.advance() {
        out.push(stream.token().text.clone());
    }
    assert_eq!(out, ["foo", "bar"]);

    let mut t = Dc3Tokenizer::new();
    let mut s = t.token_stream("");
    assert!(!s.advance());

    let mut analyzer = TextAnalyzer::from(Cjk1Tokenizer::new());
    let mut stream = analyzer.token_stream("x東y京");
    let mut out = Vec::new();
    while stream.advance() {
        out.push(stream.token().text.clone());
    }
    assert_eq!(out, ["東", "京"]);
}

// ------------------------------------------------------------------ parsing

#[test]
fn parse_basic_syntax() {
    let q = parse_query("foo -bar \"a b\" baz").unwrap();
    assert_eq!(q.words.len(), 4);
    assert_eq!(q.words[0].tokens, ["foo"]);
    assert!(!q.words[0].exclude && !q.words[0].prefix);
    assert!(q.words[1].exclude);
    assert!(q.words[2].quoted);
    assert_eq!(q.words[2].tokens, ["a", "b"]);
    assert!(q.words[3].prefix);

    assert!(!parse_query("baz ").unwrap().words[0].prefix);
    assert!(!parse_query("b").unwrap().words[0].prefix);
    assert!(!parse_query("東京").unwrap().words[0].prefix);
    assert!(!parse_query("\"baz\"").unwrap().words[0].prefix);
}

#[test]
fn trailing_ignored_word_does_not_open_prefix() {
    assert!(parse_query("foo").unwrap().words[0].prefix);
    assert!(!parse_query("foo ").unwrap().words[0].prefix);
    assert!(!parse_query("foo !!!").unwrap().words[0].prefix);
    assert!(!parse_query("foo *").unwrap().words[0].prefix);
    assert!(!parse_query("foo bar !!!").unwrap().words[1].prefix);
    // Dangling split-level empties must behave like trailing ignored words.
    assert!(!parse_query("foo -").unwrap().words[0].prefix);
    assert!(!parse_query("foo \"\"").unwrap().words[0].prefix);
    assert!(!parse_query("foo \"").unwrap().words[0].prefix);
}

#[test]
fn prefix_gate_uses_folded_token_length() {
    let folded = parse_query("ß").unwrap();
    assert_eq!(folded.words[0].tokens, ["ss"]);
    assert!(folded.words[0].prefix);
    assert_eq!(
        folded.words[0].prefix,
        parse_query("ss").unwrap().words[0].prefix
    );
    let ligature = parse_query("ﬁ").unwrap();
    assert_eq!(ligature.words[0].tokens, ["fi"]);
    assert!(ligature.words[0].prefix);
    assert_eq!(
        ligature.words[0].prefix,
        parse_query("fi").unwrap().words[0].prefix
    );
    // Multi-token words keep their phrase prefix: total folded length
    // gates, so `22.0` (tokens `22`,`0`) still qualifies.
    assert!(parse_query("22.0").unwrap().words[0].prefix);
    // Single folded chars still do not qualify.
    assert!(!parse_query("é").unwrap().words[0].prefix);
}

#[test]
fn truncated_word_does_not_become_prefix() {
    // A word capped at the per-word token limit keeps its tokens but must
    // not gain prefix expansion on the truncated tail.
    let word: Vec<String> = ('a'..='t').map(|c| c.to_string()).collect();
    let text = word.join("-");
    let q = parse_query(&text).unwrap();
    assert_eq!(q.words.len(), 1);
    assert_eq!(q.words[0].tokens.len(), MAX_TOKENS_PER_WORD);
    assert!(!q.words[0].prefix);
}

#[test]
fn parse_limits() {
    assert_eq!(parse_query(""), Err(QueryError::NoTerms));
    assert_eq!(parse_query("   "), Err(QueryError::NoTerms));
    assert_eq!(parse_query("-foo -bar"), Err(QueryError::NoTerms));
    assert_eq!(parse_query("* . -- \"\""), Err(QueryError::NoTerms));
    assert!(matches!(
        parse_query(&"a".repeat(MAX_QUERY_CHARS + 1)),
        Err(QueryError::TooLong { .. })
    ));
    let at_limit = format!("{} {}", "a".repeat(60), "b".repeat(MAX_QUERY_CHARS - 61));
    assert_eq!(at_limit.chars().count(), MAX_QUERY_CHARS);
    // The over-long second word yields no token and is ignored.
    assert_eq!(parse_query(&at_limit).unwrap().words.len(), 1);
    let twelve = ["w"; MAX_TERMS].join(" ");
    assert!(parse_query(&twelve).is_ok());
    let thirteen = ["w"; MAX_TERMS + 1].join(" ");
    assert!(matches!(
        parse_query(&thirteen),
        Err(QueryError::TooManyTerms { .. })
    ));
    // One long CJK word tokenizes to hundreds of bigrams: it is capped.
    let long_cjk = "東".repeat(200);
    let q = parse_query(&long_cjk).unwrap();
    assert_eq!(q.words.len(), 1);
    assert_eq!(q.words[0].tokens.len(), MAX_TOKENS_PER_WORD);
}

#[test]
fn query_length_gate_counts_characters_not_bytes() {
    // Over the limit in characters: the error reports characters, even when
    // the byte length is far larger.
    assert_eq!(
        parse_query(&"é".repeat(MAX_QUERY_CHARS + 1)),
        Err(QueryError::TooLong {
            chars: MAX_QUERY_CHARS + 1,
            max: MAX_QUERY_CHARS,
        })
    );
    assert_eq!(
        parse_query(&"a".repeat(MAX_QUERY_CHARS + 1)),
        Err(QueryError::TooLong {
            chars: MAX_QUERY_CHARS + 1,
            max: MAX_QUERY_CHARS,
        })
    );
    // At the limit in characters but over it in bytes: still accepted.
    // (The over-long second word yields no token and is ignored, as in
    // `parse_limits`.)
    let at_limit_multi = format!("{} {}", "a".repeat(60), "é".repeat(MAX_QUERY_CHARS - 61));
    assert_eq!(at_limit_multi.chars().count(), MAX_QUERY_CHARS);
    assert_eq!(parse_query(&at_limit_multi).unwrap().words.len(), 1);
    // A 4-byte character straddling the boundary is counted, not measured.
    let mut edge = format!("{} {}🦀", "a".repeat(60), "b".repeat(MAX_QUERY_CHARS - 62));
    assert_eq!(edge.chars().count(), MAX_QUERY_CHARS);
    assert_eq!(parse_query(&edge).unwrap().words.len(), 1);
    edge.push('x');
    assert_eq!(
        parse_query(&edge),
        Err(QueryError::TooLong {
            chars: MAX_QUERY_CHARS + 1,
            max: MAX_QUERY_CHARS,
        })
    );
    // The trailing-character check reads from the end: a multibyte
    // non-breaking space closes the query, a multibyte letter leaves it open.
    assert!(!parse_query("foo\u{a0}").unwrap().words[0].prefix);
    assert!(parse_query("fooé").unwrap().words[0].prefix);
}

#[test]
fn hostile_query_strings_are_plain_words() {
    let q = parse_query("name:foo OR *").unwrap();
    let all: Vec<&str> = q
        .words
        .iter()
        .flat_map(|w| w.tokens.iter().map(String::as_str))
        .collect();
    assert_eq!(all, ["name", "foo", "or"]);
    let q = parse_query("\"unterminated phrase").unwrap();
    assert_eq!(q.words[0].tokens, ["unterminated", "phrase"]);
    assert!(q.words[0].quoted);
    assert!(parse_query("foo\"bar").is_ok());
    assert!(parse_query("-").is_err());
}

// ------------------------------------------------------------------- search

fn doc(id: i64, name: &str, files: &str) -> IndexDoc {
    IndexDoc {
        id,
        name: name.into(),
        files: files.into(),
        size: 0,
        created: 0,
        seen: 0,
        file_count: 1,
    }
}

fn index_with(docs: &[IndexDoc]) -> SearchIndex {
    let index = SearchIndex::create_in_ram().unwrap();
    let mut w = index.writer(HEAP).unwrap();
    for d in docs {
        w.upsert(d).unwrap();
    }
    w.commit(1).unwrap();
    index
}

fn ids(index: &SearchIndex, text: &str) -> Vec<i64> {
    let mut v: Vec<i64> = run(index, SearchQuery::new(text))
        .hits
        .iter()
        .map(|h| h.id)
        .collect();
    v.sort_unstable();
    v
}

fn ranked(index: &SearchIndex, text: &str) -> Vec<i64> {
    run(index, SearchQuery::new(text))
        .hits
        .iter()
        .map(|h| h.id)
        .collect()
}

fn run(index: &SearchIndex, q: SearchQuery) -> SearchResults {
    index.search(&q).unwrap().0
}

#[test]
fn upsert_delete_reupsert_without_duplicates() {
    let index = SearchIndex::create_in_ram().unwrap();
    let mut w = index.writer(HEAP).unwrap();
    w.upsert(&doc(7, "alpha", "")).unwrap();
    w.upsert(&doc(7, "alpha beta", "")).unwrap();
    w.commit(1).unwrap();
    assert_eq!(index.doc_count(), 1);
    assert_eq!(ids(&index, "alpha"), [7]);
    assert_eq!(ids(&index, "beta"), [7]);

    w.upsert(&doc(7, "alpha gamma", "")).unwrap();
    w.commit(2).unwrap();
    assert_eq!(index.doc_count(), 1);
    assert!(ids(&index, "beta").is_empty());
    assert_eq!(ids(&index, "gamma"), [7]);

    w.delete(7).unwrap();
    w.commit(3).unwrap();
    assert_eq!(index.doc_count(), 0);
    assert!(ids(&index, "alpha").is_empty());

    w.upsert(&doc(7, "alpha", "")).unwrap();
    w.commit(4).unwrap();
    assert_eq!(index.doc_count(), 1);
    assert_eq!(ids(&index, "alpha"), [7]);
    assert_eq!(w.checkpoint(), 4);
    assert_eq!(index.checkpoint().unwrap(), 4);
}

#[test]
fn prefix_matching_and_trailing_space() {
    let index = index_with(&[doc(1, "Ubuntu 22.04 desktop", ""), doc(2, "Debian", "")]);
    assert_eq!(ids(&index, "ubunt"), [1]);
    assert!(ids(&index, "ubunt ").is_empty());
    assert_eq!(ids(&index, "ubuntu "), [1]);
    assert_eq!(ids(&index, "debian ubu"), Vec::<i64>::new());
    assert_eq!(ids(&index, "desktop ubu"), [1]);
    // Only the last word is a prefix.
    assert!(ids(&index, "ubunt desktop").is_empty());
    // Single character: no prefix expansion.
    assert!(ids(&index, "u").is_empty());
    // Multi-token last word uses a phrase prefix.
    assert_eq!(ids(&index, "22.0"), [1]);
}

#[test]
fn prefix_expansion_is_capped() {
    let docs: Vec<IndexDoc> = (0..(PREFIX_MAX_EXPANSIONS as i64 + 50))
        .map(|i| doc(i, &format!("pre{i:04}"), ""))
        .collect();
    let index = index_with(&docs);
    let r = run(&index, SearchQuery::new("pre"));
    assert_eq!(r.total, PREFIX_MAX_EXPANSIONS as u64);
}

#[test]
fn prefix_expansions_lists_what_a_trailing_word_matches() {
    let index = index_with(&[doc(1, "holiday photos", "pthc/a.jpg")]);
    let mut got = index.prefix_expansions("pt").unwrap();
    got.sort_unstable();
    assert_eq!(got, ["pthc"]);
    // File-only terms are listed too.
    let mut got = index.prefix_expansions("pth").unwrap();
    got.sort_unstable();
    assert_eq!(got, ["pthc"]);
    // No indexed term starts here.
    assert!(index.prefix_expansions("zzz").unwrap().is_empty());
    // An empty prefix must not enumerate the term dictionary.
    assert!(index.prefix_expansions("").unwrap().is_empty());
}

#[test]
fn prefix_expansion_returns_globally_smallest_terms() {
    // Terms split across two commits so segments can disagree about
    // order: the second batch holds the smaller terms. Whichever way the
    // segments merge, the answer is the 200 smallest of all 300.
    let index = SearchIndex::create_in_ram().unwrap();
    let mut w = index.writer(HEAP).unwrap();
    for i in 150..300 {
        w.upsert(&doc(i as i64, &format!("pre{i:03}"), "")).unwrap();
    }
    w.commit(1).unwrap();
    for i in 0..150 {
        w.upsert(&doc(i as i64, &format!("pre{i:03}"), "")).unwrap();
    }
    w.commit(1).unwrap();
    let expected: Vec<String> = (0..200).map(|i| format!("pre{i:03}")).collect();
    assert_eq!(index.prefix_expansions("pre").unwrap(), expected);
    // The search path sees the same budget: only the 200 smallest terms
    // match, so only their 200 documents hit.
    assert_eq!(run(&index, SearchQuery::new("pre")).total, 200);
}

#[test]
fn prefix_expansions_union_covers_both_fields_whole() {
    // 250 name terms and 250 file terms under one prefix: each field
    // contributes a full per-field budget, so the policy gate sees all
    // 400 terms either field query could match.
    let docs: Vec<IndexDoc> = (0..250)
        .map(|i| doc(i as i64, &format!("zz{i:03}"), &format!("zzf{i:03}/x")))
        .collect();
    let index = index_with(&docs);
    let got = index.prefix_expansions("zz").unwrap();
    assert_eq!(got.len(), 2 * PREFIX_MAX_EXPANSIONS);
    assert!(got.iter().all(|t| t.starts_with("zz")));
    assert!(got.windows(2).all(|w| w[0] < w[1]));
    for i in 0..200 {
        assert!(got.contains(&format!("zz{i:03}")));
        assert!(got.contains(&format!("zzf{i:03}")));
    }
}

#[test]
fn exclusion() {
    let index = index_with(&[
        doc(1, "linux iso", ""),
        doc(2, "linux source", ""),
        doc(3, "linux", "docs/iso/readme.txt"),
    ]);
    assert_eq!(ids(&index, "linux "), [1, 2, 3]);
    assert_eq!(ids(&index, "linux -iso"), [2]);
    assert_eq!(ids(&index, "linux -\"linux iso\""), [2, 3]);
}

#[test]
fn phrases() {
    let index = index_with(&[doc(1, "big red dog", ""), doc(2, "red big dog", "")]);
    assert_eq!(ids(&index, "\"big red\""), [1]);
    assert_eq!(ids(&index, "big red "), [1, 2]);
    // A punctuated word is a phrase too.
    assert_eq!(ids(&index, "big-red "), [1]);
}

#[test]
fn cjk_substring() {
    let index = index_with(&[doc(1, "東京大学", ""), doc(2, "京都大学", "")]);
    assert_eq!(ids(&index, "京大"), [1]);
    assert_eq!(ids(&index, "大学"), [1, 2]);
    assert_eq!(ids(&index, "東京大学"), [1]);
    assert!(ids(&index, "東大").is_empty());
    // A single character searches the unigram fields.
    assert_eq!(ids(&index, "学"), [1, 2]);
    assert_eq!(ids(&index, "東"), [1]);
    assert_eq!(ids(&index, "都"), [2]);
    assert!(ids(&index, "猫").is_empty());
}

#[test]
fn single_cjk_character_finds_longer_names() {
    let index = index_with(&[
        doc(1, "东京大学", ""),
        doc(2, "Lecture notes", "学生/notes.pdf"),
        doc(3, "Ubuntu", "readme.txt"),
        doc(4, "한국어 ラーメン", "カタカナ.txt"),
    ]);
    assert_eq!(ids(&index, "学"), [1, 2]);
    // A name match outranks a file-path match.
    assert_eq!(ranked(&index, "学"), [1, 2]);
    // Hangul, kana, the prolonged sound mark and halfwidth forms.
    assert_eq!(ids(&index, "국"), [4]);
    assert_eq!(ids(&index, "ー"), [4]);
    assert_eq!(ids(&index, "ｶ"), [4]);
    // Quoted and combined with other words.
    assert_eq!(ids(&index, "\"学\""), [1, 2]);
    assert_eq!(ids(&index, "学 notes"), [2]);
    assert_eq!(ids(&index, "东京 学"), [1]);
    assert!(ids(&index, "东京 生").is_empty());
    // Longer CJK words still use the bigram phrase fields.
    assert_eq!(ids(&index, "大学"), [1]);
    assert_eq!(ids(&index, "学生"), [2]);
    assert!(ids(&index, "东学").is_empty());
}

#[test]
fn single_cjk_character_exclusion() {
    let index = index_with(&[
        doc(1, "linux 东京", ""),
        doc(2, "linux", "大学/readme.txt"),
        doc(3, "linux", "notes.txt"),
    ]);
    assert_eq!(ids(&index, "linux "), [1, 2, 3]);
    assert_eq!(ids(&index, "linux -学"), [1, 3]);
    assert_eq!(ids(&index, "linux -京"), [2, 3]);
    assert_eq!(ids(&index, "linux -东京"), [2, 3]);
    assert_eq!(ids(&index, "linux -猫"), [1, 2, 3]);
    assert!(matches!(
        index.search(&SearchQuery::new("-学")),
        Err(SearchError::Query(QueryError::NoTerms))
    ));
}

#[test]
fn file_name_matches() {
    let index = index_with(&[
        doc(
            1,
            "Collection",
            "disc1/track01_moonlight.flac\ndisc1/cover.jpg",
        ),
        doc(2, "Moonlight sonata", "sonata.flac"),
    ]);
    assert_eq!(ids(&index, "cover "), [1]);
    assert_eq!(ids(&index, "moonlight "), [1, 2]);
    // Name matches outrank file-only matches.
    assert_eq!(ranked(&index, "moonlight "), [2, 1]);
}

#[test]
fn popularity_orders_equal_docs() {
    let mut a = doc(1, "same words", "");
    let mut b = doc(2, "same words", "");
    let mut c = doc(3, "same words", "");
    a.seen = 5;
    b.seen = 500;
    c.seen = 0;
    let index = index_with(&[a, b, c]);
    let r = run(&index, SearchQuery::new("same words "));
    let order: Vec<i64> = r.hits.iter().map(|h| h.id).collect();
    assert_eq!(order, [2, 1, 3]);
    assert!(r.hits[0].score > r.hits[1].score);
    assert!(r.hits[1].score > r.hits[2].score);
}

#[test]
fn sort_orders() {
    let mk = |id, size, created, seen| IndexDoc {
        id,
        name: "thing".into(),
        files: String::new(),
        size,
        created,
        seen,
        file_count: 1,
    };
    let index = index_with(&[
        mk(1, 300, -5, 10),
        mk(2, 100, 50, 30),
        mk(3, 200, 10, 20),
        mk(4, 200, 10, 20),
    ]);
    let order = |sort| {
        let mut q = SearchQuery::new("thing");
        q.sort = sort;
        run(&index, q).hits.iter().map(|h| h.id).collect::<Vec<_>>()
    };
    // Ties are broken by id, highest first.
    assert_eq!(order(Sort::Size), [1, 4, 3, 2]);
    assert_eq!(order(Sort::Newest), [2, 4, 3, 1]);
    assert_eq!(order(Sort::Seen), [2, 4, 3, 1]);
    assert_eq!(order(Sort::Relevance), [2, 4, 3, 1]);
    // No seeder field is indexed: Seeders orders like Seen there (the web
    // role re-sorts the hydrated page by the estimate).
    assert_eq!(order(Sort::Seeders), order(Sort::Seen));
    assert_eq!(Sort::parse("size"), Some(Sort::Size));
    assert_eq!(Sort::parse("seeders"), Some(Sort::Seeders));
    assert_eq!(Sort::Seeders.as_str(), "seeders");
    assert_eq!(Sort::parse("bogus"), None);
    assert_eq!(Sort::Newest.as_str(), "newest");
}

#[test]
fn seeder_multiplier_mirrors_popularity() {
    assert_eq!(seeder_multiplier(0), 1.0);
    let some = seeder_multiplier(99);
    assert!(some > 1.0);
    // Sublinear: a hundred times the seeders is far from a hundred times
    // the boost, and the weight matches popularity's.
    assert!(seeder_multiplier(9999) < 1.0 + 100.0 * (some - 1.0));
    assert_eq!(SEEDER_WEIGHT, POPULARITY_WEIGHT);
}

#[test]
fn pagination_and_total() {
    let docs: Vec<IndexDoc> = (1..=45)
        .map(|i| {
            let mut d = doc(i, "page", "");
            d.size = i as u64;
            d
        })
        .collect();
    let index = index_with(&docs);
    let page = |p, per| {
        let mut q = SearchQuery::new("page");
        q.sort = Sort::Size;
        q.page = p;
        q.per_page = per;
        run(&index, q)
    };
    let p1 = page(1, 20);
    assert_eq!(p1.total, 45);
    assert_eq!(p1.hits.len(), 20);
    assert_eq!(p1.hits[0].id, 45);
    let p3 = page(3, 20);
    assert_eq!(p3.total, 45);
    let p3ids: Vec<i64> = p3.hits.iter().map(|h| h.id).collect();
    assert_eq!(p3ids, [5, 4, 3, 2, 1]);
    assert!(page(4, 20).hits.is_empty());
    assert_eq!(page(4, 20).total, 45);
}

#[test]
fn limit_errors() {
    let index = index_with(&[doc(1, "x", "")]);
    let err = |q: SearchQuery| index.search(&q).unwrap_err();
    let mut q = SearchQuery::new("x");
    q.page = 0;
    assert!(matches!(
        err(q.clone()),
        SearchError::Query(QueryError::PageOutOfRange { .. })
    ));
    q.page = MAX_PAGE + 1;
    assert!(matches!(
        err(q.clone()),
        SearchError::Query(QueryError::PageOutOfRange { .. })
    ));
    q.page = MAX_PAGE;
    assert!(index.search(&q).is_ok());
    q.per_page = 0;
    assert!(matches!(
        err(q.clone()),
        SearchError::Query(QueryError::PerPageOutOfRange { .. })
    ));
    q.per_page = MAX_PER_PAGE + 1;
    assert!(matches!(
        err(q.clone()),
        SearchError::Query(QueryError::PerPageOutOfRange { .. })
    ));
    assert!(matches!(
        err(SearchQuery::new("a".repeat(MAX_QUERY_CHARS + 1))),
        SearchError::Query(QueryError::TooLong { .. })
    ));
    assert!(matches!(
        err(SearchQuery::new(["a"; MAX_TERMS + 1].join(" "))),
        SearchError::Query(QueryError::TooManyTerms { .. })
    ));
    assert!(matches!(
        err(SearchQuery::new("-x")),
        SearchError::Query(QueryError::NoTerms)
    ));
    assert!(matches!(
        err(SearchQuery::new("")),
        SearchError::Query(QueryError::NoTerms)
    ));
}

#[test]
fn hostile_queries_search_cleanly() {
    let index = index_with(&[doc(1, "name foo", ""), doc(2, "other", "")]);
    for q in [
        "name:foo OR *",
        "\"unterminated",
        "*",
        "foo AND (bar",
        "\\\"",
        "title:[a TO z]",
        "~~~ ^^^",
        "\u{202e}foo",
    ] {
        match index.search(&SearchQuery::new(q)) {
            Ok(_) | Err(SearchError::Query(_)) => {}
            Err(e) => panic!("{q:?}: {e}"),
        }
    }
    // Operators are words, so this matches nothing rather than everything.
    assert!(ids(&index, "name:foo OR *").is_empty());
    assert_eq!(ids(&index, "name:foo"), [1]);
    assert!(matches!(
        index.search(&SearchQuery::new("*")),
        Err(SearchError::Query(QueryError::NoTerms))
    ));
}

#[test]
fn files_text_is_truncated() {
    let mut files = "é".repeat(FILES_TEXT_MAX_BYTES / 2);
    files.push_str(" needle");
    let index = index_with(&[doc(1, "x", &files)]);
    assert!(ids(&index, "needle ").is_empty());
    assert_eq!(truncate_on_char_boundary("aé", 2), "a");
    assert_eq!(truncate_on_char_boundary("aé", 3), "aé");
}

#[test]
fn checkpoint_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index");
    {
        let index = SearchIndex::open_or_create(&path).unwrap();
        assert_eq!(index.checkpoint().unwrap(), 0);
        let mut w = index.writer(HEAP).unwrap();
        assert_eq!(w.checkpoint(), 0);
        w.upsert(&doc(1, "persisted", "")).unwrap();
        w.commit(42).unwrap();
        w.upsert(&doc(2, "uncommitted", "")).unwrap();
        // Dropped without commit.
    }
    let index = SearchIndex::open_or_create(&path).unwrap();
    assert_eq!(index.checkpoint().unwrap(), 42);
    let w = index.writer(HEAP).unwrap();
    assert_eq!(w.checkpoint(), 42);
    assert_eq!(index.doc_count(), 1);
    assert_eq!(ids(&index, "persisted"), [1]);
    assert!(ids(&index, "uncommitted").is_empty());
    index.reload().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_search() {
    let index = Arc::new(index_with(&[doc(1, "async", "")]));
    let r = index
        .search_async(SearchQuery::new("async"), Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(r.0.hits.len(), 1);
    let e = index
        .search_async(SearchQuery::new(""), Duration::from_secs(5))
        .await
        .unwrap_err();
    assert!(matches!(e, SearchError::Query(QueryError::NoTerms)));
    let e = index
        .search_async(SearchQuery::new("async"), Duration::ZERO)
        .await;
    // A zero timeout may still win the race; it must never hang or panic.
    assert!(matches!(e, Ok(_) | Err(SearchError::Timeout)));

    let handles: Vec<_> = (0..40)
        .map(|_| {
            let index = Arc::clone(&index);
            tokio::spawn(async move {
                index
                    .search_async(SearchQuery::new("async"), SEARCH_TIMEOUT)
                    .await
            })
        })
        .collect();
    for h in handles {
        assert_eq!(h.await.unwrap().unwrap().0.total, 1);
    }
}

// --------------------------------------------------------------- properties

proptest! {
    #[test]
    fn tokenizer_never_panics(s in any::<String>()) {
        let tokens = Dc3Tokenizer::new().tokens(&s);
        for (i, t) in tokens.iter().enumerate() {
            prop_assert!(!t.text.is_empty() && t.text.len() <= MAX_TOKEN_BYTES);
            // Positions may have gaps from dropped overlong tokens.
            prop_assert!(t.position >= i);
            if i > 0 {
                prop_assert!(t.position > tokens[i - 1].position);
            }
            prop_assert!(t.offset_from <= t.offset_to && t.offset_to <= s.len());
            prop_assert!(s.is_char_boundary(t.offset_from) && s.is_char_boundary(t.offset_to));
        }
    }

    #[test]
    fn cjk1_tokens_are_single_characters(s in any::<String>()) {
        let tokens = Cjk1Tokenizer::new().tokens(&s);
        for (i, t) in tokens.iter().enumerate() {
            prop_assert!(is_cjk_unigram(&t.text), "{:?}", t.text);
            prop_assert!(t.text.len() <= MAX_TOKEN_BYTES);
            prop_assert!(t.position >= i);
            if i > 0 {
                prop_assert!(t.position > tokens[i - 1].position);
            }
            prop_assert!(t.offset_from <= t.offset_to && t.offset_to <= s.len());
            prop_assert!(s.is_char_boundary(t.offset_from) && s.is_char_boundary(t.offset_to));
        }
    }

    #[test]
    fn cjk1_agrees_with_dc3_on_mixed_scripts(s in "[a-z東京한국カナー\u{3099}\u{301}._ \\-0-9ｶ]{0,48}") {
        // The CJK base characters dc3 emits (alone or in bigrams) are the
        // ones dc3_cjk1 emits. Marks can make a bigram too long for dc3 while
        // its halves still fit, so with marks this is only a subset. Marks
        // themselves are skipped: some (U+3099) have CJK script extensions
        // but may follow a Latin letter.
        let chars = |tokens: Vec<String>| -> BTreeSet<char> {
            tokens
                .iter()
                .flat_map(|t| t.chars())
                .filter(|c| is_cjk(*c) && !is_combining_mark(*c))
                .collect()
        };
        let from_dc3 = chars(Dc3Tokenizer::new().token_texts(&s));
        let from_cjk1 = chars(Cjk1Tokenizer::new().token_texts(&s));
        prop_assert!(from_dc3.is_subset(&from_cjk1));
        if !s.chars().any(is_combining_mark) {
            prop_assert_eq!(from_dc3, from_cjk1);
        }
    }

    #[test]
    fn tokenizer_never_panics_on_mixed_scripts(s in "[a-zé東京한국カナー゙\u{301}._ \\-\\[\\]0-9ＡＢ]{0,64}") {
        let _ = Dc3Tokenizer::new().tokens(&s);
    }

    #[test]
    fn parse_query_never_panics(s in any::<String>()) {
        if let Ok(q) = parse_query(&s) {
            prop_assert!(q.words.len() <= MAX_TERMS);
            prop_assert!(q.positive().next().is_some());
            prop_assert!(q.words.iter().all(|w| !w.tokens.is_empty()));
            prop_assert!(q.words.iter().all(|w| w.tokens.len() <= MAX_TOKENS_PER_WORD));
        }
    }

    #[test]
    fn parse_query_never_panics_on_syntax(s in "[a-z\"\\- *:()東]{0,80}") {
        let _ = parse_query(&s);
    }
}

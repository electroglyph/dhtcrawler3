use dc3_policy::{
    MAX_TERM_LINE_CHARS, MAX_TERM_TOKENS, PolicyError, SEED_TERMS, TermMatcher, normalise, seed,
};
use proptest::prelude::*;
use proptest::test_runner::RngSeed;

/// Fixed seed so property tests are deterministic; `PROPTEST_CASES` still
/// raises the case count for stress runs.
const PROPTEST_SEED: u64 = 0x00dc_3901;

fn proptest_config() -> ProptestConfig {
    ProptestConfig {
        rng_seed: RngSeed::Fixed(PROPTEST_SEED),
        failure_persistence: None,
        ..ProptestConfig::default()
    }
}

#[test]
fn seed_loads() {
    let m = TermMatcher::load(SEED_TERMS).unwrap();
    assert!(m.len() >= 10, "seed has {} terms", m.len());
    assert!(!m.is_empty());
    assert_eq!(seed().len(), m.len());
}

#[test]
fn empty_matcher_matches_nothing() {
    let m = TermMatcher::empty();
    assert!(m.is_empty());
    assert_eq!(m.len(), 0);
    assert!(!m.matches("pthc"));
}

#[test]
fn load_format() {
    let m = TermMatcher::load("# comment\n\n  foo  # trailing\nbar baz\nFOO\r\n").unwrap();
    assert_eq!(m.len(), 2);
    assert!(m.matches("x foo y"));
    assert!(m.matches("bar-baz"));
    assert!(!m.matches("bar"));
    assert!(!m.matches("baz bar"));
    assert!(!m.matches("comment trailing"));
}

#[test]
fn load_errors() {
    assert_eq!(
        TermMatcher::load("ok\n!!!\n").unwrap_err(),
        PolicyError::EmptyTerm { line: 2 }
    );
    let long = "a".repeat(MAX_TERM_LINE_CHARS + 1);
    assert!(matches!(
        TermMatcher::load(&long),
        Err(PolicyError::LineTooLong { line: 1, .. })
    ));
    let many = vec!["w"; MAX_TERM_TOKENS + 1].join(" ");
    assert!(matches!(
        TermMatcher::load(&many),
        Err(PolicyError::TooManyTokens { line: 1, .. })
    ));
    assert!(TermMatcher::load(&vec!["w"; MAX_TERM_TOKENS].join(" ")).is_ok());
}

#[test]
fn whole_token_only() {
    let m = TermMatcher::load("cp\nboy\n").unwrap();
    assert!(!m.matches("mcp server"));
    assert!(!m.matches("Cowboy Bebop"));
    assert!(m.matches("cp"));
    assert!(m.matches("the boy"));
}

#[test]
fn affixed_short_seeds_match() {
    let m = TermMatcher::load("pthc\nchild porn\naverylongseed\n").unwrap();
    // Whole tokens and phrases still match.
    assert!(m.matches_affixed("pthc"));
    assert!(m.matches_affixed("child porn"));
    // Short seeds hide in affixes and behind short prefixes.
    assert!(m.matches_affixed("xpthc"));
    assert!(m.matches_affixed("pthc/a.jpg"));
    // Long seeds stay whole-token only.
    assert!(!m.matches_affixed("xaverylongseed"));
    assert!(!m.matches_affixed("my child pornography"));
    assert!(!m.matches_affixed("holiday photos"));
    // An empty matcher matches nothing either way.
    assert!(!TermMatcher::empty().matches_affixed("xpthc"));
}

#[test]
fn years_stay_years() {
    assert_eq!(normalise("2024"), ["2024"]);
    let m = TermMatcher::load("zoza\n").unwrap();
    assert!(!m.matches("2024"));
}

#[test]
fn regression_f06_separator_fragmented_short_seeds_match() {
    // F-06: intra-term separators must not evade the affix rule.
    let m = TermMatcher::load("pthc\n").unwrap();
    for text in ["p.t.h.c", "p t h c", "p-t-h-c", "p_t_h_c", "p/t/h/c"] {
        assert!(m.matches_affixed(text), "missed: {text:?}");
    }
    assert!(!m.matches_affixed("clean holiday photos"));
}

#[test]
fn regression_f06_digit_interleaved_short_seeds_match() {
    // F-06: digits interleaved in the seed must not evade the affix rule.
    let m = TermMatcher::load("pthc\n").unwrap();
    assert!(m.matches_affixed("p1thc"));
    assert!(!m.matches_affixed("clean movie 2024"));
}

#[test]
fn regression_f06_combined_separator_and_digit_evasion_matches() {
    // F-06 follow-up: separators fragmenting AND digits interleaving at
    // once (`p.1.t.h.c` compacts to `p1thc`) must not evade either.
    let m = TermMatcher::load("pthc\n").unwrap();
    for text in ["p.1t-h_c", "p 1 t h c", "p.1.t.h.c"] {
        assert!(m.matches_affixed(text), "missed: {text:?}");
    }
    assert!(!m.matches_affixed("clean holiday photos"));
    assert!(!m.matches_affixed("clean movie 2024"));
}

#[test]
fn affixed_checks_share_one_variant_pass() {
    // Every evasion class fires from a single walk over the variant, with
    // all seeds matched in one automaton pass per haystack.
    let m = TermMatcher::load("ab\ncd\nef\n").unwrap();
    // Each check in isolation: affix, fragment, digit-strip, combined.
    assert!(m.matches_affixed("xxabxx"));
    assert!(m.matches_affixed("c.d"));
    assert!(m.matches_affixed("a1b"));
    assert!(m.matches_affixed("e.1.f"));
    // Several seeds across several checks in one text.
    assert!(m.matches_affixed("xxabxx c.d e1f"));
    // A later seed still fires when earlier ones are absent.
    assert!(m.matches_affixed("zz e.1.f zz"));
    // Near misses across every derived form stay negative.
    assert!(!m.matches_affixed("axb cxd exf"));
    assert!(!m.matches_affixed("a1x c2y e3z"));
}

#[test]
fn affixed_handles_empty_and_long_only_matchers() {
    let m = TermMatcher::load("ab\n").unwrap();
    // Separator-only and empty texts have no tokens to scan.
    assert!(!m.matches_affixed("..."));
    assert!(!m.matches_affixed(""));
    // Without short seeds the affix fast path stays silent, while whole
    // tokens still match.
    let m = TermMatcher::load("abcdefgh\n").unwrap();
    assert!(!m.matches_affixed("xxabcdefghxx"));
    assert!(!m.matches_affixed("a.b.c.d.e.f.g.h"));
    assert!(m.matches_affixed("abcdefgh"));
}

#[test]
fn affixed_finds_every_seed_in_one_pass() {
    // Dozens of seeds share one automaton pass per haystack; each must be
    // found affixed on its own, and a text holding none must stay clean.
    let seeds: Vec<String> = ('a'..='h')
        .flat_map(|a| ('a'..='h').map(move |b| format!("{a}{b}")))
        .collect();
    assert_eq!(seeds.len(), 64);
    let m = TermMatcher::load(&seeds.join("\n")).unwrap();
    for seed in ["ab", "cd", "ef", "gh", "ha", "bd", "ec", "ga", "dh", "fb"] {
        assert!(
            m.matches_affixed(&format!("xx{seed}xx")),
            "missed seed: {seed:?}"
        );
    }
    assert!(!m.matches_affixed("qq qq"));
    assert!(!m.matches_affixed("qq1q q.q"));
}

#[test]
fn regression_f07_raw_line_length_is_bounded() {
    // F-07: the length gate measures the raw line, so a megabyte of
    // comment text cannot slip past it.
    let raw = format!("ab#{}", "x".repeat(1_000_000));
    assert!(matches!(
        TermMatcher::load(&raw),
        Err(PolicyError::LineTooLong { .. })
    ));
}

#[test]
fn line_length_gate_handles_multibyte_boundaries() {
    // ASCII fast accept: exactly MAX chars fits.
    assert!(TermMatcher::load(&"a".repeat(MAX_TERM_LINE_CHARS)).is_ok());
    // ASCII just over: rejected.
    assert!(matches!(
        TermMatcher::load(&"a".repeat(MAX_TERM_LINE_CHARS + 1)),
        Err(PolicyError::LineTooLong { line: 1, .. })
    ));
    // Way over the 4-bytes-per-char ceiling: rejected without a char walk,
    // and the reported line number still tracks the file.
    let huge = format!("ok\n{}", "a".repeat(4 * MAX_TERM_LINE_CHARS + 1));
    assert_eq!(
        TermMatcher::load(&huge).unwrap_err(),
        PolicyError::LineTooLong { line: 2, max: MAX_TERM_LINE_CHARS }
    );
    // Two-byte chars: 1025 of them exceed the gate.
    assert!(matches!(
        TermMatcher::load(&"é".repeat(MAX_TERM_LINE_CHARS + 1)),
        Err(PolicyError::LineTooLong { .. })
    ));
    // Four-byte chars: exactly MAX passes the length gate (a later token
    // gate may still reject the line, but never with LineTooLong).
    assert!(!matches!(
        TermMatcher::load(&"😀".repeat(MAX_TERM_LINE_CHARS)),
        Err(PolicyError::LineTooLong { .. })
    ));
    // Four-byte chars over: rejected.
    assert!(matches!(
        TermMatcher::load(&"😀".repeat(MAX_TERM_LINE_CHARS + 1)),
        Err(PolicyError::LineTooLong { .. })
    ));
    // Empty line is fine.
    assert!(TermMatcher::load("\n").is_ok());
}

#[test]
fn ordinary_names_not_blocked() {
    let m = seed();
    for name in [
        "Ubuntu 26.04 LTS",
        "Lolita (1962) 1080p",
        "The Child in Time",
        "Preteen Fashion Guide",
        "Cowboy Bebop S01 1080p",
        "Child Development 2024.pdf",
        "Kinder Surprise unboxing",
        "Raymond Gold - Live",
        "Sex Education S02",
        "mcp-server-2024.tar.gz",
        "Porn Studies journal vol 3",
    ] {
        assert!(!m.matches(name), "false positive: {name}");
    }
}

#[test]
fn evasions_blocked() {
    let m = seed();
    for name in [
        "pthc",
        "ＰＴＨＣ video",
        "\u{0440}th\u{0441}",
        "p\u{200D}t\u{200D}h\u{200D}c",
        "pt\u{00AD}hc",
        "hussy\u{200B}fan",
        "r@ygold",
        "hussyf4n",
        "HuSsYfAn",
        "[PTHC]",
        "pthc_2011",
        "PTHC2011",
        "r@ygold2011",
        "child.porn",
        "Kiddy-Porn",
        "CHILD   PORNOGRAPHY",
        "some/dir/babyshivid/file.avi",
        "p\u{0332}t\u{0332}h\u{0332}c\u{0332}",
        "pthç",
        "𝐩𝐭𝐡𝐜",
    ] {
        assert!(m.matches(name), "missed: {name:?}");
    }
}

#[test]
fn phrase_needs_contiguity() {
    let m = seed();
    assert!(!m.matches("child safety and porn filters"));
    assert!(!m.matches("porn child"));
}

proptest! {
    #![proptest_config(proptest_config())]

    #[test]
    fn normalise_never_panics(s in any::<String>()) {
        let _ = normalise(&s);
    }

    #[test]
    fn normalise_idempotent(s in any::<String>()) {
        let once = normalise(&s);
        let twice = normalise(&once.join(" "));
        prop_assert_eq!(once, twice);
    }

    #[test]
    fn normalise_idempotent_mixed(s in "[a-zA-Z0-9 @$._\\-\u{0400}-\u{04FF}\u{0300}-\u{036F}\u{200B}-\u{200F}\u{FF01}-\u{FF5E}\u{3040}-\u{30FF}\u{AC00}-\u{AC20}\u{1100}-\u{1170}]{0,40}") {
        let once = normalise(&s);
        let twice = normalise(&once.join(" "));
        prop_assert_eq!(once, twice);
    }

    #[test]
    fn matches_never_panics(s in any::<String>()) {
        let m = seed();
        let _ = m.matches(&s);
    }

    #[test]
    fn load_never_panics(s in any::<String>()) {
        let _ = TermMatcher::load(&s);
    }

    #[test]
    fn normalised_term_matches_itself(s in "[a-z]{1,8}( [a-z]{1,8}){0,3}") {
        let m = TermMatcher::load(&s).unwrap();
        prop_assert!(m.matches(&s));
        let upper = s.to_uppercase();
        let wrapped = format!("x.{upper}.y");
        prop_assert!(m.matches(&wrapped));
    }
}

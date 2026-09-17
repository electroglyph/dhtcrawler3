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
fn years_stay_years() {
    assert_eq!(normalise("2024"), ["2024"]);
    let m = TermMatcher::load("zoza\n").unwrap();
    assert!(!m.matches("2024"));
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

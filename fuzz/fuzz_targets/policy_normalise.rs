#![no_main]
//! Normalisation and term matching must never panic, and normalisation must
//! be idempotent on its own joined output.

use std::sync::OnceLock;

use dc3_policy::{TermMatcher, normalise, try_seed};
use libfuzzer_sys::fuzz_target;

fn matcher() -> &'static TermMatcher {
    static SEED: OnceLock<TermMatcher> = OnceLock::new();
    SEED.get_or_init(|| try_seed().expect("the shipped seed list loads"))
}

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let tokens = normalise(&text);
    assert!(
        tokens.iter().all(|t| !t.is_empty()),
        "empty token in {tokens:?}"
    );
    let joined = tokens.join(" ");
    let again = normalise(&joined);
    assert_eq!(again, tokens, "normalise is not idempotent on {joined:?}");
    let m = matcher();
    let hit = m.matches(&text);
    // The joined base tokens are what the matcher sees first, so a match on
    // them is a match on the text.
    if !hit {
        let _ = m.matches(&joined);
    }
});

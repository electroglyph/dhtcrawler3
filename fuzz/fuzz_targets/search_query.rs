#![no_main]
//! Query parsing and the dc3 tokenizers must never panic, must respect their
//! limits, and must give token offsets that slice the input.

use dc3_search::{
    Cjk1Tokenizer, Dc3Tokenizer, MAX_QUERY_CHARS, MAX_TERMS, MAX_TOKEN_BYTES, is_cjk_unigram,
    parse_query,
};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    match parse_query(&text) {
        Ok(q) => {
            assert!(text.chars().count() <= MAX_QUERY_CHARS);
            assert!(!q.words.is_empty() && q.words.len() <= MAX_TERMS);
            assert!(q.positive().next().is_some());
            for (i, w) in q.words.iter().enumerate() {
                assert!(!w.tokens.is_empty());
                assert!(
                    w.tokens
                        .iter()
                        .all(|t| !t.is_empty() && t.len() <= MAX_TOKEN_BYTES)
                );
                assert!(!w.prefix || (i + 1 == q.words.len() && !w.exclude && !w.quoted));
                let _ = w.is_cjk_unigram();
            }
        }
        Err(e) => {
            let _ = e.to_string();
        }
    }

    let mut dc3 = Dc3Tokenizer::new();
    for t in dc3.tokens(&text) {
        assert!(!t.text.is_empty() && t.text.len() <= MAX_TOKEN_BYTES);
        assert!(t.offset_from <= t.offset_to);
        assert!(
            text.get(t.offset_from..t.offset_to).is_some(),
            "bad offsets {t:?}"
        );
    }
    let mut cjk = Cjk1Tokenizer::new();
    for t in cjk.tokens(&text) {
        assert!(is_cjk_unigram(&t.text), "not a CJK unigram: {t:?}");
        assert!(
            text.get(t.offset_from..t.offset_to).is_some(),
            "bad offsets {t:?}"
        );
    }
});

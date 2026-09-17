//! Tantivy search index for dhtcrawler3 (design §11; R16, R17).
//!
//! * [`Dc3Tokenizer`] / [`Cjk1Tokenizer`]: dictionary-free tokenizers with
//!   CJK bigrams and CJK unigrams.
//! * [`parse_query`]: the user query syntax and its limits.
//! * [`SearchIndex`] / [`IndexWriterHandle`]: the index, a writer whose
//!   commits carry the indexer checkpoint, and bounded searches.
//! * [`IndexRoot`]: numbered index generations and the `CURRENT` file.
//! * [`SearchHandle`]: a cloneable reader that follows `CURRENT`.
//!
//! The index is a derived projection of PostgreSQL: it can always be deleted
//! and rebuilt from checkpoint 0.
#![forbid(unsafe_code)]
#![warn(clippy::arithmetic_side_effects)]

mod generations;
mod handle;
mod index;
mod query;
mod safe_seek;
mod tokenizer;

#[cfg(test)]
mod generation_tests;
#[cfg(test)]
mod tests;

pub use generations::{
    CURRENT_FILE, FIRST_GENERATION, GENERATION_DIGITS, GENERATION_PREFIX, IndexRoot,
    ROOT_LOCK_TIMEOUT, WRITER_PROBE_HEAP_BYTES, generation_dir_name, parse_generation_dir_name,
};
pub use handle::{MIN_WATCH_INTERVAL, SearchHandle};
pub use index::{
    DEFAULT_WRITER_HEAP_BYTES, FILES_TEXT_MAX_BYTES, Hit, IndexDoc, IndexWriterHandle,
    MAX_CONCURRENT_SEARCHES, NAME_BOOST, POPULARITY_WEIGHT, PREFIX_BOOST, Result, SEARCH_TIMEOUT,
    SearchError, SearchIndex, SearchResults, field_names, popularity_multiplier, schema,
    truncate_on_char_boundary,
};
pub use query::{
    DEFAULT_PER_PAGE, MAX_PAGE, MAX_PER_PAGE, MAX_QUERY_CHARS, MAX_TERMS, PREFIX_MAX_EXPANSIONS,
    PREFIX_MIN_CHARS, ParsedQuery, QueryError, QueryWord, SearchQuery, Sort, parse_query,
};
pub use tokenizer::{
    CJK1_TOKENIZER_NAME, Cjk1Tokenizer, Dc3TokenStream, Dc3Tokenizer, MAX_TOKEN_BYTES,
    TOKENIZER_NAME, contains_cjk, is_cjk, is_cjk_unigram,
};

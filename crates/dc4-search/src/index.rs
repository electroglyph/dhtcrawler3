//! The Tantivy index: schema, writer with checkpointed commits, and search.
//!
//! # Sharing a directory between processes
//!
//! The index role writes and the web role reads the same generation
//! directory through separate [`Index`] instances. Tantivy 0.26.2's
//! `MmapDirectory` makes this safe with two `flock` locks (via `fs4`):
//!
//! * `.tantivy-writer.lock`, taken without waiting by every `IndexWriter`, so
//!   there is at most one writer per directory across all processes. The lock
//!   is released when the writer is dropped or its process exits; the empty
//!   lock file itself stays.
//! * `.tantivy-meta.lock`, taken (waiting) by a reader while it loads
//!   `meta.json` and opens the segment files, and by the writer's garbage
//!   collector while it decides which files to delete. A reload therefore
//!   never opens a file that is being deleted. Files deleted after a searcher
//!   opened them stay readable through its memory maps (POSIX unlink rules).
//!
//! Readers need write access to the directory because of the second lock.
//! They use [`ReloadPolicy::OnCommitWithDelay`], which polls `meta.json`
//! every 500 ms. Tantivy does not retry a failed reload, so
//! [`SearchIndex::reload_if_changed`] (called on every
//! [`SearchHandle`](crate::SearchHandle) refresh) compares the open segments
//! with `meta.json` and reloads when they differ.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tantivy::collector::sort_key::NaturalComparator;
use tantivy::collector::{Count, SegmentSortKeyComputer, SortKeyComputer, TopDocs};
use tantivy::columnar::Column;
use tantivy::directory::MmapDirectory;
use tantivy::index::SegmentId;
use tantivy::query::{
    BooleanQuery, BoostQuery, Occur, PhrasePrefixQuery, PhraseQuery, Query, TermQuery, TermSetQuery,
};
use tantivy::schema::{
    FAST, Field, INDEXED, IndexRecordOption, STORED, Schema, TextFieldIndexing, TextOptions,
};
use tantivy::{
    DocId, Index, IndexReader, IndexSettings, IndexWriter, Opstamp, ReloadPolicy, Score, Searcher,
    SegmentReader, TantivyDocument, Term,
};
use tokio::sync::Semaphore;

use crate::query::{
    PREFIX_MAX_EXPANSIONS, ParsedQuery, QueryError, QueryWord, SearchQuery, Sort, parse_query,
};
use crate::safe_seek::SafeSeekQuery;
use crate::tokenizer::{CJK1_TOKENIZER_NAME, Cjk1Tokenizer, Dc4Tokenizer, TOKENIZER_NAME};

/// Maximum bytes of concatenated file paths indexed per document.
pub const FILES_TEXT_MAX_BYTES: usize = 64 * 1024;
/// Maximum number of searches running at once in [`SearchIndex::search_async`].
pub const MAX_CONCURRENT_SEARCHES: usize = 16;
/// Default time limit for one search.
pub const SEARCH_TIMEOUT: Duration = Duration::from_secs(5);
/// Default writer heap size.
pub const DEFAULT_WRITER_HEAP_BYTES: usize = 128 * 1024 * 1024;
/// Score multiplier for matches in the torrent name.
pub const NAME_BOOST: Score = 3.0;
/// Score multiplier for documents matched only through a prefix expansion.
pub const PREFIX_BOOST: Score = 0.5;
/// Weight of `log10(1 + seen)` in the relevance multiplier.
pub const POPULARITY_WEIGHT: f64 = 0.15;
/// Weight of `log10(1 + seeders_est)` in the web-side seeder boost, mirroring
/// [`POPULARITY_WEIGHT`]. Applied after hydration, fresh estimates only.
pub const SEEDER_WEIGHT: f64 = 0.15;

/// Schema field names.
pub mod field_names {
    pub const ID: &str = "id";
    /// Name words and CJK bigrams, with positions.
    pub const NAME: &str = "name";
    /// File-path words and CJK bigrams, with positions.
    pub const FILES: &str = "files";
    /// CJK characters of the name, one token each.
    pub const NAME_CJK1: &str = "name_cjk1";
    /// CJK characters of the file paths, one token each.
    pub const FILES_CJK1: &str = "files_cjk1";
    pub const SIZE: &str = "size";
    pub const CREATED: &str = "created";
    pub const SEEN: &str = "seen";
    pub const FILE_COUNT: &str = "file_count";
}

/// Errors from the search index.
#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error(transparent)]
    Query(#[from] QueryError),
    #[error("search index error: {0}")]
    Index(#[from] tantivy::TantivyError),
    #[error("cannot open index directory: {0}")]
    Directory(#[from] tantivy::directory::error::OpenDirectoryError),
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid commit payload: {0}")]
    Payload(String),
    #[error("index schema has no field {0:?}")]
    MissingField(&'static str),
    /// The index on disk was built by a version with another schema.
    #[error("the index on disk has a different schema; rebuild the index")]
    SchemaMismatch,
    /// The index root has no `CURRENT` file (and cannot safely get one).
    #[error("index root has no CURRENT file")]
    MissingCurrent,
    /// The `CURRENT` file does not hold exactly one generation number.
    #[error("index root CURRENT file is invalid: {0}")]
    InvalidCurrent(&'static str),
    /// Generation numbers start at 1.
    #[error("{0} is not a valid index generation")]
    InvalidGeneration(u64),
    /// The generation directory or the index inside it does not exist.
    #[error("index generation {0} does not exist")]
    MissingGeneration(u64),
    /// Another process held the index root lock for too long.
    #[error("timed out waiting for the index root lock")]
    RootLocked,
    #[error("search timed out")]
    Timeout,
    #[error("search task failed: {0}")]
    Task(String),
}

/// Result alias for this crate.
pub type Result<T, E = SearchError> = std::result::Result<T, E>;

/// One torrent as the index sees it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IndexDoc {
    /// Database row id; unique in the index.
    pub id: i64,
    pub name: String,
    /// File paths joined with newlines; truncated to [`FILES_TEXT_MAX_BYTES`].
    pub files: String,
    pub size: u64,
    /// Unix seconds.
    pub created: i64,
    pub seen: u64,
    pub file_count: u64,
}

/// One search result.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hit {
    pub id: i64,
    /// Relevance score for [`Sort::Relevance`]; 0 for other orders.
    pub score: f32,
}

/// One page of results plus the total number of matches.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SearchResults {
    pub hits: Vec<Hit>,
    pub total: u64,
}

/// Version of the index a search ran against: the open segment set plus
/// delete opstamps, as compared by [`SearchIndex::reload_if_changed`].
///
/// This is the cloned segments map itself — not tantivy's whole
/// `SearcherGeneration`, whose per-`IndexReader` monotonic `generation_id`
/// would make two readers over identical segments compare unequal.
pub type IndexStamp = BTreeMap<SegmentId, Option<Opstamp>>;

#[derive(Serialize, Deserialize)]
struct CommitPayload {
    checkpoint: i64,
}

#[derive(Clone, Copy, Debug)]
struct Fields {
    id: Field,
    name: Field,
    files: Field,
    name_cjk1: Field,
    files_cjk1: Field,
    size: Field,
    created: Field,
    seen: Field,
    file_count: Field,
}

impl Fields {
    fn from_schema(schema: &Schema) -> Result<Self> {
        let get = |n: &'static str| {
            schema
                .get_field(n)
                .map_err(|_| SearchError::MissingField(n))
        };
        Ok(Self {
            id: get(field_names::ID)?,
            name: get(field_names::NAME)?,
            files: get(field_names::FILES)?,
            name_cjk1: get(field_names::NAME_CJK1)?,
            files_cjk1: get(field_names::FILES_CJK1)?,
            size: get(field_names::SIZE)?,
            created: get(field_names::CREATED)?,
            seen: get(field_names::SEEN)?,
            file_count: get(field_names::FILE_COUNT)?,
        })
    }
}

/// The index schema.
pub fn schema() -> Schema {
    let text = |tokenizer: &str, record: IndexRecordOption| {
        TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(tokenizer)
                .set_index_option(record),
        )
    };
    let words = text(TOKENIZER_NAME, IndexRecordOption::WithFreqsAndPositions);
    let unigrams = text(CJK1_TOKENIZER_NAME, IndexRecordOption::WithFreqs);
    let mut b = Schema::builder();
    b.add_i64_field(field_names::ID, INDEXED | FAST | STORED);
    b.add_text_field(field_names::NAME, words.clone());
    b.add_text_field(field_names::FILES, words);
    b.add_text_field(field_names::NAME_CJK1, unigrams.clone());
    b.add_text_field(field_names::FILES_CJK1, unigrams);
    b.add_u64_field(field_names::SIZE, FAST);
    b.add_i64_field(field_names::CREATED, FAST);
    b.add_u64_field(field_names::SEEN, FAST);
    b.add_u64_field(field_names::FILE_COUNT, FAST);
    b.build()
}

/// Truncates `s` to at most `max_bytes` bytes on a character boundary.
pub fn truncate_on_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    s.get(..end).unwrap_or("")
}

fn new_permits() -> Arc<Semaphore> {
    Arc::new(Semaphore::new(MAX_CONCURRENT_SEARCHES))
}

/// Opens the index in an existing directory without creating anything.
/// A directory without an index gives a Tantivy "file does not exist" error.
pub(crate) fn open_existing_index(dir: &Path) -> Result<Index> {
    let index = Index::open(MmapDirectory::open(dir)?)?;
    if index.schema() != schema() {
        return Err(SearchError::SchemaMismatch);
    }
    Ok(index)
}

/// True when `dir` is a directory that holds an index (`meta.json`).
pub(crate) fn index_exists(dir: &Path) -> Result<bool> {
    if !dir.is_dir() {
        return Ok(false);
    }
    let directory = MmapDirectory::open(dir)?;
    Ok(Index::exists(&directory).map_err(tantivy::TantivyError::from)?)
}

/// Opens the index in `dir` (which must exist), creating it when absent.
pub(crate) fn open_or_create_index(dir: &Path) -> Result<Index> {
    if index_exists(dir)? {
        return open_existing_index(dir);
    }
    Ok(Index::create(
        MmapDirectory::open(dir)?,
        schema(),
        IndexSettings::default(),
    )?)
}

/// A search index with a shared reader.
pub struct SearchIndex {
    index: Index,
    reader: IndexReader,
    fields: Fields,
    permits: Arc<Semaphore>,
    generation: Option<u64>,
}

impl std::fmt::Debug for SearchIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchIndex")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl SearchIndex {
    /// Opens the index in `dir`, creating the directory and index if needed.
    /// An index with another schema is [`SearchError::SchemaMismatch`].
    pub fn open_or_create(dir: &Path) -> Result<SearchIndex> {
        std::fs::create_dir_all(dir)?;
        Self::from_index(open_or_create_index(dir)?, new_permits(), None)
    }

    /// Opens the existing index in `dir`; never creates anything.
    pub fn open(dir: &Path) -> Result<SearchIndex> {
        Self::from_index(open_existing_index(dir)?, new_permits(), None)
    }

    /// Creates an empty in-memory index (for tests and tools).
    pub fn create_in_ram() -> Result<SearchIndex> {
        Self::from_index(Index::create_in_ram(schema()), new_permits(), None)
    }

    /// Opens an existing generation, sharing `permits` with other indexes.
    pub(crate) fn open_generation(
        dir: &Path,
        generation: u64,
        permits: Arc<Semaphore>,
    ) -> Result<SearchIndex> {
        Self::from_index(open_existing_index(dir)?, permits, Some(generation))
    }

    /// Creates a new index in the empty directory `dir`.
    pub(crate) fn create_generation(dir: &Path, generation: u64) -> Result<SearchIndex> {
        let index = Index::create(
            MmapDirectory::open(dir)?,
            schema(),
            IndexSettings::default(),
        )?;
        Self::from_index(index, new_permits(), Some(generation))
    }

    fn from_index(
        index: Index,
        permits: Arc<Semaphore>,
        generation: Option<u64>,
    ) -> Result<SearchIndex> {
        let tokenizers = index.tokenizers();
        tokenizers.register(TOKENIZER_NAME, Dc4Tokenizer::new());
        tokenizers.register(CJK1_TOKENIZER_NAME, Cjk1Tokenizer::new());
        let fields = Fields::from_schema(&index.schema())?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        Ok(SearchIndex {
            index,
            reader,
            fields,
            permits,
            generation,
        })
    }

    /// The generation this index was opened as, when it came from an
    /// [`IndexRoot`](crate::IndexRoot).
    pub fn generation(&self) -> Option<u64> {
        self.generation
    }

    #[cfg(test)]
    pub(crate) fn permits(&self) -> &Arc<Semaphore> {
        &self.permits
    }

    /// Opens the single writer. `heap_bytes` is split between indexing
    /// threads (Tantivy requires at least 15 MB per thread). Fails with a
    /// Tantivy lock error while another writer, in any process, is open.
    pub fn writer(&self, heap_bytes: usize) -> Result<IndexWriterHandle> {
        let writer = self.index.writer::<TantivyDocument>(heap_bytes)?;
        let checkpoint = self.checkpoint()?;
        Ok(IndexWriterHandle {
            writer,
            reader: self.reader.clone(),
            fields: self.fields,
            checkpoint,
        })
    }

    /// The checkpoint stored with the last commit (0 when there is none).
    pub fn checkpoint(&self) -> Result<i64> {
        let metas = self.index.load_metas()?;
        match metas.payload.as_deref() {
            None | Some("") => Ok(0),
            Some(p) => serde_json::from_str::<CommitPayload>(p)
                .map(|c| c.checkpoint)
                .map_err(|e| SearchError::Payload(e.to_string())),
        }
    }

    /// Number of live documents visible to searches.
    pub fn doc_count(&self) -> u64 {
        self.reader.searcher().num_docs()
    }

    /// Number of segments visible to searches.
    pub fn segment_count(&self) -> usize {
        self.reader.searcher().segment_readers().len()
    }

    /// Makes the latest commit visible to searches now.
    pub fn reload(&self) -> Result<()> {
        self.reader.reload()?;
        Ok(())
    }

    /// Reloads when `meta.json` lists other segments or deletes than the
    /// searcher has open. Returns whether it reloaded. Blocking.
    pub fn reload_if_changed(&self) -> Result<bool> {
        let metas = self.index.load_metas()?;
        let searcher = self.reader.searcher();
        let open = searcher.generation().segments();
        let unchanged = open.len() == metas.segments.len()
            && metas
                .segments
                .iter()
                .all(|m| open.get(&m.id()) == Some(&m.delete_opstamp()));
        if unchanged {
            return Ok(false);
        }
        self.reader.reload()?;
        Ok(true)
    }

    /// Snapshots the index version searches currently see. Cheap: one
    /// `reader.searcher()` Arc clone plus a memory-only clone of the
    /// handful-entry segments map. No I/O.
    pub fn stamp(&self) -> IndexStamp {
        self.reader.searcher().generation().segments().clone()
    }

    /// True when `stamp` matches the index version searches currently see.
    /// One `reader.searcher()` Arc clone and a map comparison; no clone
    /// of the map, no I/O.
    pub fn stamp_matches(&self, stamp: &IndexStamp) -> bool {
        self.reader.searcher().generation().segments() == stamp
    }

    /// Runs a search on the calling thread.
    ///
    /// Returns the results with the stamp of the searcher that ran the
    /// search, captured from the same `searcher` it queries.
    pub fn search(&self, q: &SearchQuery) -> Result<(SearchResults, IndexStamp)> {
        let parsed = parse_query(&q.text)?;
        self.search_parsed(q, &parsed)
    }

    /// Runs a search on the calling thread with an already-parsed query.
    /// Saves the second `parse_query` on the prefix-gate path (B-005).
    ///
    /// Returns the results with the stamp of the searcher that ran the
    /// search, captured from the same `searcher` it queries.
    pub fn search_parsed(
        &self,
        q: &SearchQuery,
        parsed: &ParsedQuery,
    ) -> Result<(SearchResults, IndexStamp)> {
        let offset = q.offset()?;
        let searcher = self.reader.searcher();
        let stamp = searcher.generation().segments().clone();
        let query = self.build_query(parsed, &searcher)?;
        let per_page = usize::try_from(q.per_page).unwrap_or(usize::MAX);
        let top = TopDocs::for_doc_range(offset..offset.saturating_add(per_page))
            .order_by(RankKeyComputer { sort: q.sort });
        let (total, top) = searcher.search(&query, &(Count, top))?;
        let hits = top
            .into_iter()
            .map(|(key, _addr)| Hit {
                id: key.2,
                score: key.0,
            })
            .collect();
        Ok((
            SearchResults {
                hits,
                total: u64::try_from(total).unwrap_or(u64::MAX),
            },
            stamp,
        ))
    }

    /// Runs a search on the blocking pool, at most
    /// [`MAX_CONCURRENT_SEARCHES`] at once, giving up after `timeout`
    /// (which includes the wait for a free slot). Indexes opened through
    /// one [`SearchHandle`](crate::SearchHandle) share the limit.
    pub async fn search_async(
        self: &Arc<Self>,
        q: SearchQuery,
        timeout: Duration,
    ) -> Result<(SearchResults, IndexStamp)> {
        let this = Arc::clone(self);
        let permits = Arc::clone(&self.permits);
        let run = async move {
            let permit = permits
                .acquire_owned()
                .await
                .map_err(|e| SearchError::Task(e.to_string()))?;
            // The permit moves into the task, so a search that outlives its
            // timeout still counts against the limit until it finishes.
            let task = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                this.search(&q)
            });
            task.await.map_err(|e| SearchError::Task(e.to_string()))?
        };
        tokio::time::timeout(timeout, run)
            .await
            .map_err(|_| SearchError::Timeout)?
    }

    /// Runs [`search_parsed`](Self::search_parsed) on the blocking pool with
    /// the same limit and timeout as [`search_async`](Self::search_async).
    pub async fn search_parsed_async(
        self: &Arc<Self>,
        q: SearchQuery,
        parsed: ParsedQuery,
        timeout: Duration,
    ) -> Result<(SearchResults, IndexStamp)> {
        let this = Arc::clone(self);
        let permits = Arc::clone(&self.permits);
        let run = async move {
            let permit = permits
                .acquire_owned()
                .await
                .map_err(|e| SearchError::Task(e.to_string()))?;
            // The permit moves into the task, so a search that outlives its
            // timeout still counts against the limit until it finishes.
            let task = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                this.search_parsed(&q, &parsed)
            });
            task.await.map_err(|e| SearchError::Task(e.to_string()))?
        };
        tokio::time::timeout(timeout, run)
            .await
            .map_err(|_| SearchError::Timeout)?
    }

    fn build_query(&self, parsed: &ParsedQuery, searcher: &Searcher) -> Result<BooleanQuery> {
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for word in parsed.positive() {
            let (name_field, files_field) = self.word_fields(word);
            let name = self.field_query(name_field, word, searcher)?;
            let files = self.field_query(files_field, word, searcher)?;
            let either: Box<dyn Query> = Box::new(BooleanQuery::new(vec![
                (Occur::Should, Box::new(BoostQuery::new(name, NAME_BOOST))),
                (Occur::Should, files),
            ]));
            clauses.push((Occur::Must, either));
        }
        if clauses.is_empty() {
            return Err(QueryError::NoTerms.into());
        }
        for word in parsed.negative() {
            let (name_field, files_field) = self.word_fields(word);
            let name = exact_query(name_field, &word.tokens);
            let files = exact_query(files_field, &word.tokens);
            if let (Some(name), Some(files)) = (name, files) {
                let either: Box<dyn Query> = Box::new(BooleanQuery::new(vec![
                    (Occur::Should, name),
                    (Occur::Should, files),
                ]));
                clauses.push((Occur::MustNot, either));
            }
        }
        Ok(BooleanQuery::new(clauses))
    }

    /// The (name, files) fields a word searches: the unigram fields for a
    /// one-character CJK word, the word fields otherwise.
    fn word_fields(&self, word: &QueryWord) -> (Field, Field) {
        if word.is_cjk_unigram() {
            (self.fields.name_cjk1, self.fields.files_cjk1)
        } else {
            (self.fields.name, self.fields.files)
        }
    }

    /// Indexed terms of the name/files word fields starting with `prefix`:
    /// what a trailing single-token prefix word can match. Used to apply
    /// the content policy to prefix expansions, which whole-token gates
    /// cannot see.
    pub fn prefix_expansions(&self, prefix: &str) -> Result<Vec<String>> {
        if prefix.is_empty() {
            // An empty prefix matches every indexed term; callers only ever
            // pass a real trailing token, so refuse instead of disclosing
            // up to 400 vocabulary terms.
            return Ok(Vec::new());
        }
        let searcher = self.reader.searcher();
        let mut found = BTreeSet::new();
        for field in [self.fields.name, self.fields.files] {
            found.extend(expand_prefix(&searcher, field, prefix)?);
        }
        // The union budget is two per-field budgets: each field query can
        // match a full [`PREFIX_MAX_EXPANSIONS`] terms, so the policy gate
        // must see both fields whole. Truncating the union to one budget
        // could hide a blocked term the query still matches.
        Ok(found.into_iter().take(2 * PREFIX_MAX_EXPANSIONS).collect())
    }

    fn field_query(
        &self,
        field: Field,
        word: &QueryWord,
        searcher: &Searcher,
    ) -> Result<Box<dyn Query>> {
        let exact = exact_query(field, &word.tokens).ok_or(QueryError::NoTerms)?;
        if !word.prefix {
            return Ok(exact);
        }
        let prefix: Option<Box<dyn Query>> = match word.tokens.as_slice() {
            [] => None,
            [single] => {
                let expansions = expand_prefix(searcher, field, single)?;
                if expansions.is_empty() {
                    None
                } else {
                    let terms = expansions.iter().map(|t| Term::from_field_text(field, t));
                    Some(Box::new(BoostQuery::new(
                        Box::new(TermSetQuery::new(terms)),
                        PREFIX_BOOST,
                    )))
                }
            }
            many => {
                let terms: Vec<Term> = many
                    .iter()
                    .map(|t| Term::from_field_text(field, t))
                    .collect();
                let mut q = PhrasePrefixQuery::new(terms);
                q.set_max_expansions(u32::try_from(PREFIX_MAX_EXPANSIONS).unwrap_or(u32::MAX));
                Some(Box::new(BoostQuery::new(
                    Box::new(SafeSeekQuery(Box::new(q))),
                    PREFIX_BOOST,
                )))
            }
        };
        Ok(match prefix {
            None => exact,
            Some(p) => Box::new(BooleanQuery::new(vec![
                (Occur::Should, exact),
                (Occur::Should, p),
            ])),
        })
    }
}

/// A term query for one token, a phrase query for several, `None` for none.
fn exact_query(field: Field, tokens: &[String]) -> Option<Box<dyn Query>> {
    match tokens {
        [] => None,
        [single] => Some(Box::new(TermQuery::new(
            Term::from_field_text(field, single),
            IndexRecordOption::WithFreqs,
        ))),
        many => Some(Box::new(SafeSeekQuery(Box::new(PhraseQuery::new(
            many.iter()
                .map(|t| Term::from_field_text(field, t))
                .collect(),
        ))))),
    }
}

/// The smallest byte string greater than every string starting with `prefix`,
/// or `None` if there is none.
fn prefix_upper_bound(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut upper = prefix.to_vec();
    while let Some(last) = upper.pop() {
        if let Some(next) = last.checked_add(1) {
            upper.push(next);
            return Some(upper);
        }
    }
    None
}

/// Up to [`PREFIX_MAX_EXPANSIONS`] indexed terms of `field` that start with
/// `prefix`: the globally smallest terms across all segments, in
/// lexicographic order.
fn expand_prefix(searcher: &Searcher, field: Field, prefix: &str) -> Result<Vec<String>> {
    let upper = prefix_upper_bound(prefix.as_bytes());
    let mut found: BTreeSet<String> = BTreeSet::new();
    for segment in searcher.segment_readers() {
        let inverted = segment.inverted_index(field)?;
        let mut range = inverted.terms().range().ge(prefix.as_bytes());
        if let Some(upper) = &upper {
            range = range.lt(upper);
        }
        let mut stream = range.into_stream()?;
        while stream.advance() {
            // Segment streams are lexicographically ordered: once the set
            // holds a full budget and the cursor has passed its largest
            // term, nothing later in this segment can displace anything,
            // so stop scanning it. The budget is global, not per segment:
            // later segments may still hold smaller terms.
            if found.len() >= PREFIX_MAX_EXPANSIONS
                && found
                    .last()
                    .is_some_and(|max| stream.key() > max.as_bytes())
            {
                break;
            }
            if let Ok(term) = std::str::from_utf8(stream.key()) {
                found.insert(term.to_owned());
            }
        }
    }
    Ok(found.into_iter().take(PREFIX_MAX_EXPANSIONS).collect())
}

/// Sort key: (relevance, field value, id); larger is better.
type RankKey = (Score, u64, i64);

struct RankKeyComputer {
    sort: Sort,
}

struct RankSegment {
    sort: Sort,
    primary: Column<u64>,
    ids: Column<i64>,
}

impl SortKeyComputer for RankKeyComputer {
    type SortKey = RankKey;
    type Child = RankSegment;
    type Comparator = NaturalComparator;

    fn requires_scoring(&self) -> bool {
        self.sort == Sort::Relevance
    }

    fn segment_sort_key_computer(&self, reader: &SegmentReader) -> tantivy::Result<RankSegment> {
        let fast = reader.fast_fields();
        let primary = match self.sort {
            Sort::Relevance | Sort::Seen | Sort::Seeders => fast.u64(field_names::SEEN)?,
            Sort::Size => fast.u64(field_names::SIZE)?,
            Sort::Newest => fast.i64(field_names::CREATED)?.to_u64_monotonic(),
        };
        Ok(RankSegment {
            sort: self.sort,
            primary,
            ids: fast.i64(field_names::ID)?,
        })
    }
}

/// `1 + POPULARITY_WEIGHT * log10(1 + seen)`.
pub fn popularity_multiplier(seen: u64) -> f64 {
    // u64 -> f64 may round, which is irrelevant for a logarithm.
    #[allow(clippy::cast_precision_loss)]
    let seen = seen as f64;
    1.0 + POPULARITY_WEIGHT * (1.0 + seen).log10()
}

/// `1 + SEEDER_WEIGHT * log10(1 + seeders_est)`: the web-side seeder boost
/// for relevance order (bep33.md §7). Fresh estimates only; stale or
/// missing estimates rank exactly as today.
pub fn seeder_multiplier(seeders_est: u64) -> f64 {
    // u64 -> f64 may round, which is irrelevant for a logarithm.
    #[allow(clippy::cast_precision_loss)]
    let est = seeders_est as f64;
    1.0 + SEEDER_WEIGHT * (1.0 + est).log10()
}

impl SegmentSortKeyComputer for RankSegment {
    type SortKey = RankKey;
    type SegmentSortKey = RankKey;
    type SegmentComparator = NaturalComparator;

    fn segment_sort_key(&mut self, doc: DocId, score: Score) -> RankKey {
        let id = self.ids.first(doc).unwrap_or(i64::MIN);
        let value = self.primary.first(doc).unwrap_or(0);
        match self.sort {
            Sort::Relevance => {
                #[allow(clippy::cast_possible_truncation)]
                let tweaked = (f64::from(score) * popularity_multiplier(value)) as Score;
                (tweaked, 0, id)
            }
            Sort::Newest | Sort::Size | Sort::Seen | Sort::Seeders => (0.0, value, id),
        }
    }

    fn convert_segment_sort_key(&self, key: RankKey) -> RankKey {
        key
    }
}

/// The single writer. Changes become visible (and durable) on
/// [`commit`](Self::commit).
pub struct IndexWriterHandle {
    writer: IndexWriter<TantivyDocument>,
    reader: IndexReader,
    fields: Fields,
    checkpoint: i64,
}

impl std::fmt::Debug for IndexWriterHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexWriterHandle")
            .field("checkpoint", &self.checkpoint)
            .finish_non_exhaustive()
    }
}

impl IndexWriterHandle {
    /// Replaces any document with the same id by `doc`.
    pub fn upsert(&mut self, doc: &IndexDoc) -> Result<()> {
        let f = self.fields;
        self.writer.delete_term(Term::from_field_i64(f.id, doc.id));
        let files = truncate_on_char_boundary(&doc.files, FILES_TEXT_MAX_BYTES);
        let mut d = TantivyDocument::new();
        d.add_i64(f.id, doc.id);
        d.add_text(f.name, &doc.name);
        d.add_text(f.files, files);
        d.add_text(f.name_cjk1, &doc.name);
        d.add_text(f.files_cjk1, files);
        d.add_u64(f.size, doc.size);
        d.add_i64(f.created, doc.created);
        d.add_u64(f.seen, doc.seen);
        d.add_u64(f.file_count, doc.file_count);
        self.writer.add_document(d)?;
        Ok(())
    }

    /// Removes the document with this id, if any.
    pub fn delete(&mut self, id: i64) -> Result<()> {
        self.writer
            .delete_term(Term::from_field_i64(self.fields.id, id));
        Ok(())
    }

    /// Commits pending changes together with `checkpoint`, then reloads the
    /// reader so searches see them.
    pub fn commit(&mut self, checkpoint: i64) -> Result<()> {
        let payload = serde_json::to_string(&CommitPayload { checkpoint })
            .map_err(|e| SearchError::Payload(e.to_string()))?;
        let mut prepared = self.writer.prepare_commit()?;
        prepared.set_payload(&payload);
        prepared.commit()?;
        self.checkpoint = checkpoint;
        self.reader.reload()?;
        tracing::debug!(checkpoint, "search index committed");
        Ok(())
    }

    /// The checkpoint of the last successful commit.
    pub fn checkpoint(&self) -> i64 {
        self.checkpoint
    }

    /// Drops uncommitted changes, waits for running merges, and releases the
    /// writer lock. Use it for a clean shutdown.
    pub fn wait_merging_threads(self) -> Result<()> {
        self.writer.wait_merging_threads()?;
        Ok(())
    }
}

#[cfg(test)]
mod stamp_tests {
    use super::{IndexDoc, SearchIndex};
    use crate::query::SearchQuery;

    const HEAP: usize = 20 * 1024 * 1024;

    fn doc(id: i64, name: &str) -> IndexDoc {
        IndexDoc {
            id,
            name: name.into(),
            ..IndexDoc::default()
        }
    }

    fn generation_id(index: &SearchIndex) -> u64 {
        index.reader.searcher().generation().generation_id()
    }

    #[test]
    fn stamp_is_stable_without_commits() {
        let index = SearchIndex::create_in_ram().unwrap();
        let first = index.stamp();
        assert_eq!(index.stamp(), first);
        assert!(index.stamp_matches(&first));
        // A search neither changes the stamp nor reports a different one.
        let (results, stamped) = index.search(&SearchQuery::new("nothingmatches")).unwrap();
        assert!(results.hits.is_empty());
        assert_eq!(stamped, first);
    }

    #[test]
    fn stamp_changes_after_commit_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        let index = SearchIndex::open_or_create(dir.path()).unwrap();
        let before = index.stamp();
        let mut writer = index.writer(HEAP).unwrap();
        writer.upsert(&doc(1, "fresh upload")).unwrap();
        writer.commit(1).unwrap();
        index.reload().unwrap();
        let after = index.stamp();
        assert_ne!(after, before);
        // `stamp_matches` agrees with `stamp() != stored` on both sides.
        assert!(!index.stamp_matches(&before));
        assert!(index.stamp_matches(&after));
        assert_eq!(index.stamp_matches(&before), index.stamp() == before);
        assert_eq!(index.stamp_matches(&after), index.stamp() == after);
        // A delete changes the stamp again through the delete opstamp.
        writer.delete(1).unwrap();
        writer.commit(2).unwrap();
        index.reload().unwrap();
        assert_ne!(index.stamp(), after);
        assert!(!index.stamp_matches(&after));
    }

    #[test]
    fn reload_if_changed_reports_no_change_after_reload() {
        let dir = tempfile::tempdir().unwrap();
        let index = SearchIndex::open_or_create(dir.path()).unwrap();
        let mut writer = index.writer(HEAP).unwrap();
        writer.upsert(&doc(1, "fresh upload")).unwrap();
        writer.commit(1).unwrap();
        index.reload().unwrap();
        // Nothing changed on disk since the reload: no I/O-heavy reload.
        assert!(!index.reload_if_changed().unwrap());
        assert!(index.stamp_matches(&index.stamp()));
    }

    #[test]
    fn two_readers_over_identical_segments_share_a_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let writer_side = SearchIndex::open_or_create(dir.path()).unwrap();
        let mut writer = writer_side.writer(HEAP).unwrap();
        writer.upsert(&doc(1, "fresh upload")).unwrap();
        writer.commit(1).unwrap();
        writer_side.reload().unwrap();
        // A second reader over the same directory: a fresh per-reader
        // counter over the same segments.
        let reader_side = SearchIndex::open(dir.path()).unwrap();
        // The trap is armed: tantivy bumps `generation_id` on every reload
        // unconditionally (reader/mod.rs `reload` always builds a new
        // searcher), so the two readers hold different ids ...
        assert_ne!(
            generation_id(&writer_side),
            generation_id(&reader_side),
            "trap disarmed: expected distinct generation_id counters"
        );
        // ... yet the stamps compare equal: the id is not part of the stamp.
        assert_eq!(writer_side.stamp(), reader_side.stamp());
        assert!(writer_side.stamp_matches(&reader_side.stamp()));
        assert!(reader_side.stamp_matches(&writer_side.stamp()));
    }
}

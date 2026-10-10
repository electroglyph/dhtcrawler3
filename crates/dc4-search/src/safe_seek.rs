//! A query wrapper that hides `seek_danger` from its inner scorer.
//!
//! Tantivy 0.26.2's `PhraseScorer::seek_danger` debug-asserts
//! `target >= self.doc()`, but the `seek_danger` contract allows the docset to
//! be left past later targets (for example when a phrase is used in an
//! exclusion). Debug builds then panic on ordinary queries such as
//! `linux -"linux iso"`. Wrapping the phrase makes callers go through the
//! default `seek_danger`, which only uses the always-valid `seek`.

use std::fmt;

use tantivy::query::{EnableScoring, Explanation, Query, Scorer, Weight};
use tantivy::{DocId, DocSet, Score, SegmentReader, Term};

/// Wraps `inner` so its scorers never receive `seek_danger` calls.
pub(crate) struct SafeSeekQuery(pub(crate) Box<dyn Query>);

impl Clone for SafeSeekQuery {
    fn clone(&self) -> Self {
        Self(self.0.box_clone())
    }
}

impl fmt::Debug for SafeSeekQuery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("SafeSeekQuery").field(&self.0).finish()
    }
}

impl Query for SafeSeekQuery {
    fn weight(&self, enable_scoring: EnableScoring<'_>) -> tantivy::Result<Box<dyn Weight>> {
        Ok(Box::new(SafeSeekWeight(self.0.weight(enable_scoring)?)))
    }

    fn query_terms<'a>(&'a self, visitor: &mut dyn FnMut(&'a Term, bool)) {
        self.0.query_terms(visitor);
    }
}

struct SafeSeekWeight(Box<dyn Weight>);

impl Weight for SafeSeekWeight {
    fn scorer(&self, reader: &SegmentReader, boost: Score) -> tantivy::Result<Box<dyn Scorer>> {
        Ok(Box::new(SafeSeekScorer(self.0.scorer(reader, boost)?)))
    }

    fn explain(&self, reader: &SegmentReader, doc: DocId) -> tantivy::Result<Explanation> {
        self.0.explain(reader, doc)
    }
}

struct SafeSeekScorer(Box<dyn Scorer>);

impl DocSet for SafeSeekScorer {
    fn advance(&mut self) -> DocId {
        self.0.advance()
    }

    fn seek(&mut self, target: DocId) -> DocId {
        let doc = self.0.doc();
        if doc >= target {
            return doc;
        }
        self.0.seek(target)
    }

    fn doc(&self) -> DocId {
        self.0.doc()
    }

    fn size_hint(&self) -> u32 {
        self.0.size_hint()
    }

    fn cost(&self) -> u64 {
        self.0.cost()
    }
}

impl Scorer for SafeSeekScorer {
    fn score(&mut self) -> Score {
        self.0.score()
    }
}

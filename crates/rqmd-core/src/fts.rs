//! Tantivy FTS wrapper.
//!
//! Field weights mirror qmd's SQLite FTS5 weights: filepath=1.5, title=4.0, body=1.0.
//! Results are joined back to rusqlite by `filepath` = "collection/path".

use anyhow::{Context, Result};
use std::path::Path;
use tantivy::{
    Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument, Term,
    collector::TopDocs,
    doc,
    query::{BooleanQuery, ConstScoreQuery, PhraseQuery, Query, QueryParser, TermQuery},
    schema::{FAST, Field, IndexRecordOption, STORED, Schema, SchemaBuilder, TEXT, Value},
};

pub struct FtsSchema {
    pub schema: Schema,
    pub filepath: Field,
    pub title: Field,
    pub body: Field,
    /// Stored-only field holding the rusqlite document ID for fast join-back.
    pub doc_id: Field,
}

impl FtsSchema {
    pub fn build() -> Self {
        let mut builder = SchemaBuilder::new();
        // filepath is stored + tokenized (enables path-based filtering)
        let filepath = builder.add_text_field("filepath", TEXT | STORED);
        let title = builder.add_text_field("title", TEXT | STORED);
        // body is tokenized but not stored — body comes from rusqlite content table
        let body = builder.add_text_field("body", TEXT);
        // doc_id stored as an i64 fast field for retrieval
        let doc_id = builder.add_i64_field("doc_id", STORED | FAST);
        let schema = builder.build();
        Self {
            schema,
            filepath,
            title,
            body,
            doc_id,
        }
    }
}

pub struct FtsIndex {
    pub schema: FtsSchema,
    pub index: Index,
    pub reader: IndexReader,
    // Writer is acquired lazily — only needed for indexing, not search.
    // This allows multiple Store instances to open the same Tantivy dir
    // concurrently as long as at most one calls add_document/commit.
    writer: Option<IndexWriter>,
    pub query_parser: QueryParser,
}

impl FtsIndex {
    /// Open or create the Tantivy index at `dir`.
    pub fn open_or_create(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir).context("create tantivy dir")?;
        let schema = FtsSchema::build();

        let index = Index::open_or_create(
            tantivy::directory::MmapDirectory::open(dir).context("mmap directory")?,
            schema.schema.clone(),
        )
        .context("open_or_create tantivy index")?;

        // Manual reload policy: we call reader.reload() explicitly in commit()
        // so searches immediately after indexing see the new documents.
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
            .context("index reader")?;

        let mut query_parser =
            QueryParser::for_index(&index, vec![schema.filepath, schema.title, schema.body]);
        // BM25 field boosts matching qmd's `bm25(documents_fts, 1.5, 4.0, 1.0)`.
        query_parser.set_field_boost(schema.filepath, 1.5);
        query_parser.set_field_boost(schema.title, 4.0);
        query_parser.set_field_boost(schema.body, 1.0);

        Ok(Self {
            schema,
            index,
            reader,
            writer: None,
            query_parser,
        })
    }

    /// Acquire the writer on first call; subsequent calls reuse it.
    fn writer_mut(&mut self) -> Result<&mut IndexWriter> {
        if self.writer.is_none() {
            self.writer = Some(self.index.writer(50_000_000).context("index writer")?);
        }
        Ok(self.writer.as_mut().unwrap())
    }

    /// Add or update a document in the index.
    /// Callers should call `commit()` after batching inserts.
    pub fn add_document(
        &mut self,
        filepath: &str,
        title: &str,
        body: &str,
        doc_id: i64,
    ) -> Result<()> {
        // Extract Copy fields before borrowing the writer.
        let (f_filepath, f_title, f_body, f_doc_id) = (
            self.schema.filepath,
            self.schema.title,
            self.schema.body,
            self.schema.doc_id,
        );
        self.delete_by_filepath(filepath)?;
        let w = self.writer_mut()?;
        w.add_document(doc!(
            f_filepath => filepath,
            f_title => title,
            f_body => body,
            f_doc_id => doc_id,
        ))?;
        Ok(())
    }

    /// Delete every previously-indexed document under this exact `filepath`.
    /// Callers should call `commit()` afterward. A no-op (not an error) if
    /// nothing is currently indexed under this filepath.
    ///
    /// Used both to clear the stale entry before re-indexing an updated
    /// document (`add_document`) and to sweep entries for files removed or
    /// renamed on disk (`Store::update`'s prune pass).
    pub fn delete_by_filepath(&mut self, filepath: &str) -> Result<()> {
        if let Some(query) = self.exact_filepath_query(filepath)? {
            let w = self.writer_mut()?;
            w.delete_query(query).context("delete tantivy doc")?;
        }
        Ok(())
    }

    /// Build a query that matches only documents indexed under exactly this
    /// `filepath` string.
    ///
    /// `filepath` is a tokenized `TEXT` field (not a raw/`STRING` field), so a
    /// term built from the whole untokenized string — as
    /// `Term::from_field_text(f_filepath, filepath)` naively does — can never
    /// match anything: the term dictionary only contains the post-tokenizer
    /// sub-tokens. Adding an untokenized field to disambiguate would change
    /// the Tantivy schema, forcing every existing user to fully reindex
    /// (detected only by an opaque open failure) — not worth it. Instead,
    /// tokenize `filepath` the same way indexing did and build an exact
    /// phrase query (positions taken from the real token stream, not assumed
    /// consecutive indices, so a >40-char segment dropped by
    /// `RemoveLongFilter` can't shift a false match onto a different path).
    /// A plain intersection (AND) of the tokens would over-match any other
    /// path whose token set is a superset of this one's — fine for a search
    /// pushdown with a post-filter (see `collection_pushdown_clause`), but
    /// not for a destructive delete. Returns `None` when tokenization yields
    /// no usable terms (matches nothing).
    fn exact_filepath_query(&self, filepath: &str) -> Result<Option<Box<dyn Query>>> {
        let terms = self.path_terms(filepath)?;

        match terms.len() {
            0 => Ok(None),
            1 => {
                let (_, term) = terms.into_iter().next().unwrap();
                Ok(Some(Box::new(TermQuery::new(
                    term,
                    IndexRecordOption::Basic,
                ))))
            }
            _ => Ok(Some(Box::new(PhraseQuery::new_with_offset(terms)))),
        }
    }

    /// Commit buffered writes so they become searchable.
    /// Explicitly reloads the reader so that searches immediately after commit
    /// see the new documents (OnCommitWithDelay uses a background thread and
    /// has a non-zero delay that causes stale reads in the same process).
    pub fn commit(&mut self) -> Result<()> {
        self.writer_mut()?.commit().context("tantivy commit")?;
        self.reader.reload().context("tantivy reader reload")?;
        Ok(())
    }

    /// Full-text search. Returns (filepath, doc_id, bm25_score) sorted by score descending.
    /// `collection_filter` restricts to a single collection. Thin wrapper over
    /// `search_fts_multi` — a one-element slice reproduces this exact behavior.
    pub fn search_fts(
        &self,
        query_text: &str,
        limit: usize,
        collection_filter: Option<&str>,
    ) -> Result<Vec<(String, i64, f32)>> {
        let owned = collection_filter.map(|c| [c.to_string()]);
        self.search_fts_multi(query_text, limit, owned.as_ref().map(|a| a.as_slice()))
    }

    /// Same as `search_fts`, but matches any of several collections. `None` or an
    /// empty slice searches every collection. Backs the MCP server's `collections`
    /// filter (multi-collection parity with qmd 2.6.3).
    pub fn search_fts_multi(
        &self,
        query_text: &str,
        limit: usize,
        collections: Option<&[String]>,
    ) -> Result<Vec<(String, i64, f32)>> {
        // `TopDocs::with_limit` asserts `limit != 0` and panics otherwise. A
        // panic here unwinds through the caller's `Mutex<Store>` guard (the
        // MCP server holds one per tool call) and poisons it, wedging every
        // subsequent query until the daemon is restarted — so this must be
        // handled before it ever reaches tantivy, not just documented as a
        // caller obligation. "0 results requested" is a well-defined,
        // non-panicking answer: none.
        if limit == 0 {
            return Ok(Vec::new());
        }

        let searcher = self.reader.searcher();

        // `parse_query` fails hard on a fragment tantivy reads as a field specifier
        // (e.g. "error: connection refused" — the colon looks like `field:value`).
        // `parse_query_lenient` degrades that fragment to a best-effort clause
        // instead of returning zero results with no explanation; surface each
        // degradation as a warning so it's diagnosable.
        let (mut query, parse_errors) = self.query_parser.parse_query_lenient(query_text);
        for err in &parse_errors {
            tracing::warn!("fts query {query_text:?} parsed leniently: {err}");
        }

        // Pushdown: require the requested collection(s) to match (tokenized the
        // same way the filepath field itself is indexed) so `TopDocs::with_limit`
        // can't drop a small collection's hits before the exact-prefix filter
        // below ever sees them. This can over-match (a collection named "notes"
        // could also match "other/my-notes.md") — that's fine, the post-filter
        // is still the final guarantee. Wrapped in a ConstScoreQuery so filtering
        // never perturbs BM25 ranking.
        if let Some(cols) = collections.filter(|c| !c.is_empty()) {
            let mut clause_groups: Vec<Box<dyn Query>> = Vec::new();
            for c in cols {
                if let Some(clause) = self.collection_pushdown_clause(c)? {
                    clause_groups.push(clause);
                }
            }
            if !clause_groups.is_empty() {
                let pushdown = BooleanQuery::union(clause_groups);
                query = Box::new(BooleanQuery::intersection(vec![
                    query,
                    Box::new(ConstScoreQuery::new(Box::new(pushdown), 0.0)),
                ]));
            }
        }

        // The pushdown clause above is best-effort (it can over-match, e.g. a
        // collection named "notes" also matching "other/my-notes.md"), so the
        // exact-prefix filter below is still the authority. That means
        // `TopDocs::with_limit(limit)` can cut off the candidate set before the
        // filter ever sees enough in-scope hits, under-returning even when more
        // matches exist further down the global ranking. Over-fetch when a
        // collection filter is active so the filter has a large enough pool to
        // draw `limit` results from; uncapped (all-collections) queries need no
        // overscan since every candidate already counts.
        const FETCH_OVERSCAN_FACTOR: usize = 20;
        const FETCH_OVERSCAN_CAP: usize = 5000;
        let scoped = collections.is_some_and(|c| !c.is_empty());
        let fetch_limit = if scoped {
            limit
                .saturating_mul(FETCH_OVERSCAN_FACTOR)
                .clamp(limit, FETCH_OVERSCAN_CAP)
        } else {
            limit
        };

        let collector = TopDocs::with_limit(fetch_limit).order_by_score();
        let top_docs = searcher
            .search(&query, &collector)
            .context("tantivy search")?;

        let mut results = Vec::new();
        for (score, addr) in top_docs {
            if results.len() >= limit {
                break;
            }
            let retrieved: TantivyDocument = searcher.doc(addr).context("retrieve tantivy doc")?;

            let filepath = retrieved
                .get_first(self.schema.filepath)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            // Apply collection filter at result level (filepath = "collection/path").
            // No filter, or an empty list, means "search all collections".
            if let Some(cols) = collections
                && !cols.is_empty()
                && !cols.iter().any(|cf| {
                    let prefix = format!("{cf}/");
                    filepath.starts_with(&prefix) || filepath == cf.as_str()
                })
            {
                continue;
            }

            let doc_id = retrieved
                .get_first(self.schema.doc_id)
                .and_then(|v| v.as_i64())
                .unwrap_or(0);

            // Normalize Tantivy's raw BM25 score (positive, unbounded) to [0,1) using
            // the same monotonic squash qmd applies at its searchFTS boundary:
            //   score = |bm25| / (1 + |bm25|)
            // (qmd src/store.ts:3620 — "Monotonic and query-independent — no per-query
            // normalization needed").  Tantivy scores are positive (higher = better), so
            // |x| = x here.  This ensures format_score never renders > 100%.
            let norm = score / (1.0 + score);
            results.push((filepath, doc_id, norm));
        }

        Ok(results)
    }

    /// Build a filter clause requiring every token of `collection` (tokenized the
    /// same way the filepath field itself is indexed) to appear on that field.
    /// Returns `None` when the collection name yields no usable tokens, in which
    /// case there is no clause that could ever match and callers should skip
    /// pushdown for it, relying on the exact-prefix post-filter alone.
    fn collection_pushdown_clause(&self, collection: &str) -> Result<Option<Box<dyn Query>>> {
        let terms = self.path_terms(collection)?;
        if terms.is_empty() {
            return Ok(None);
        }

        let must_all: Vec<Box<dyn Query>> = terms
            .into_iter()
            .map(|(_, t)| Box::new(TermQuery::new(t, IndexRecordOption::Basic)) as Box<dyn Query>)
            .collect();
        Ok(Some(Box::new(BooleanQuery::intersection(must_all))))
    }

    /// Tokenize `value` the same way indexing tokenized the `filepath` field,
    /// returning each surviving token's real position alongside its `Term`.
    /// Shared by `exact_filepath_query` (needs positions for an exact
    /// `PhraseQuery` — a >40-char token dropped by `RemoveLongFilter` must not
    /// shift a later position onto the wrong index) and
    /// `collection_pushdown_clause` (positions unused, but the same
    /// tokenize-and-length-filter logic applies).
    fn path_terms(&self, value: &str) -> Result<Vec<(usize, Term)>> {
        let mut analyzer = self
            .index
            .tokenizer_for_field(self.schema.filepath)
            .context("tokenizer for filepath field")?;

        let mut terms = Vec::new();
        let mut stream = analyzer.token_stream(value);
        stream.process(&mut |token| {
            // Defensive — RemoveLongFilter(40) already drops tokens over 40
            // chars before indexing, so this token could never be in the
            // index; including it would guarantee zero matches.
            if token.text.len() <= 40 {
                terms.push((
                    token.position,
                    Term::from_field_text(self.schema.filepath, &token.text),
                ));
            }
        });
        Ok(terms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Re-indexing the same filepath must not leave the previous body's
    /// terms searchable, and must not leave a ghost document behind — the
    /// live doc count must return to 1, not accumulate to 2.
    #[test]
    fn add_document_replaces_stale_entry_for_same_filepath() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut idx = FtsIndex::open_or_create(dir.path()).unwrap();

        idx.add_document("notes/a.md", "Title", "uniquealphatoken", 1)
            .unwrap();
        idx.commit().unwrap();
        assert_eq!(idx.reader.searcher().num_docs(), 1);

        idx.add_document("notes/a.md", "Title", "uniquebetatoken", 1)
            .unwrap();
        idx.commit().unwrap();
        assert_eq!(
            idx.reader.searcher().num_docs(),
            1,
            "stale entry from the first index must be deleted, not accumulated"
        );

        let alpha = idx.search_fts("uniquealphatoken", 10, None).unwrap();
        let beta = idx.search_fts("uniquebetatoken", 10, None).unwrap();
        assert_eq!(alpha.len(), 0, "old body's terms must no longer match");
        assert_eq!(beta.len(), 1, "new body's terms must match");
    }

    /// Regression test: `limit: 0` (e.g. from an MCP client, or `rqmd search
    /// -n 0`) must return an empty result, not panic. `TopDocs::with_limit`
    /// asserts on a zero limit, and a panic here would unwind through the
    /// MCP server's `Mutex<Store>` guard and poison it, wedging every
    /// subsequent tool call until the daemon is restarted.
    #[test]
    fn search_fts_zero_limit_returns_empty_not_panic() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut idx = FtsIndex::open_or_create(dir.path()).unwrap();
        idx.add_document("notes/a.md", "Title", "uniquealphatoken", 1)
            .unwrap();
        idx.commit().unwrap();

        let results = idx.search_fts("uniquealphatoken", 0, None).unwrap();
        assert_eq!(results.len(), 0);
    }

    /// A path whose tokenization yields exactly one term (below the
    /// PhraseQuery two-term minimum) must still be deletable/replaceable.
    #[test]
    fn add_document_replaces_single_token_filepath() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut idx = FtsIndex::open_or_create(dir.path()).unwrap();

        idx.add_document("readme", "Title", "uniquealphatoken", 1)
            .unwrap();
        idx.commit().unwrap();
        assert_eq!(idx.reader.searcher().num_docs(), 1);

        idx.add_document("readme", "Title", "uniquebetatoken", 1)
            .unwrap();
        idx.commit().unwrap();
        assert_eq!(idx.reader.searcher().num_docs(), 1);

        let alpha = idx.search_fts("uniquealphatoken", 10, None).unwrap();
        let beta = idx.search_fts("uniquebetatoken", 10, None).unwrap();
        assert_eq!(alpha.len(), 0);
        assert_eq!(beta.len(), 1);
    }

    /// Deleting a filepath that was never indexed must be a no-op, not an
    /// error.
    #[test]
    fn delete_by_filepath_missing_is_noop() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut idx = FtsIndex::open_or_create(dir.path()).unwrap();
        idx.delete_by_filepath("never/indexed.md").unwrap();
        idx.commit().unwrap();
        assert_eq!(idx.reader.searcher().num_docs(), 0);
    }

    /// A collection-scoped query must not under-return: if more than `limit`
    /// in-scope documents match, the post-filter (which runs after
    /// `TopDocs::with_limit`) must not be starved of candidates by
    /// higher-scoring out-of-scope hits crowding the top of the global
    /// ranking.
    ///
    /// The out-of-scope docs live at `other/my-notes-N.md` — the default
    /// tokenizer splits that path into tokens including "notes", so they
    /// satisfy `collection_pushdown_clause("notes")`'s MUST-match-token
    /// requirement (the documented over-match: "a collection named 'notes'
    /// could also match 'other/my-notes.md'") and reach `TopDocs` alongside
    /// the genuine `notes/docN.md` hits. Only the exact-prefix post-filter
    /// tells them apart. Without the overscan fix, `TopDocs::with_limit`
    /// picks its top `limit` from the combined pool by score alone; since
    /// the out-of-scope docs repeat "shared" and score higher, they can fill
    /// the entire collector and leave zero in-scope survivors even though 15
    /// genuine in-scope matches exist.
    #[test]
    fn search_fts_multi_scoped_query_does_not_under_return() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut idx = FtsIndex::open_or_create(dir.path()).unwrap();

        for i in 0..30 {
            idx.add_document(
                &format!("other/my-notes-{i}.md"),
                "Title",
                "shared shared shared shared shared",
                1000 + i,
            )
            .unwrap();
        }
        for i in 0..15 {
            idx.add_document(&format!("notes/doc{i}.md"), "Title", "shared", i)
                .unwrap();
        }
        idx.commit().unwrap();

        let results = idx
            .search_fts_multi("shared", 10, Some(&["notes".to_string()]))
            .unwrap();
        assert_eq!(
            results.len(),
            10,
            "expected the full requested limit of in-scope hits, got {results:?}"
        );
        assert!(
            results
                .iter()
                .all(|(path, _, _)| path.starts_with("notes/"))
        );
    }
}

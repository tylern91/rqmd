//! Tantivy FTS wrapper.
//!
//! Field weights mirror qmd's SQLite FTS5 weights: filepath=1.5, title=4.0, body=1.0.
//! Results are joined back to rusqlite by `filepath` = "collection/path".

use anyhow::{Context, Result};
use std::path::Path;
use tantivy::{
    collector::TopDocs,
    doc,
    query::{BooleanQuery, ConstScoreQuery, Query, QueryParser, TermQuery},
    schema::{Field, IndexRecordOption, Schema, SchemaBuilder, Value, FAST, STORED, TEXT},
    Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument, Term,
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
        let term = tantivy::Term::from_field_text(f_filepath, filepath);
        let w = self.writer_mut()?;
        w.delete_term(term);
        w.add_document(doc!(
            f_filepath => filepath,
            f_title => title,
            f_body => body,
            f_doc_id => doc_id,
        ))?;
        Ok(())
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

        let collector = TopDocs::with_limit(limit).order_by_score();
        let top_docs = searcher
            .search(&query, &collector)
            .context("tantivy search")?;

        let mut results = Vec::new();
        for (score, addr) in top_docs {
            let retrieved: TantivyDocument = searcher.doc(addr).context("retrieve tantivy doc")?;

            let filepath = retrieved
                .get_first(self.schema.filepath)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            // Apply collection filter at result level (filepath = "collection/path").
            // No filter, or an empty list, means "search all collections".
            if let Some(cols) = collections {
                if !cols.is_empty()
                    && !cols.iter().any(|cf| {
                        let prefix = format!("{cf}/");
                        filepath.starts_with(&prefix) || filepath == cf.as_str()
                    })
                {
                    continue;
                }
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
        let mut analyzer = self
            .index
            .tokenizer_for_field(self.schema.filepath)
            .context("tokenizer for filepath field")?;

        let mut terms = Vec::new();
        {
            let mut stream = analyzer.token_stream(collection);
            stream.process(&mut |token| {
                // Defensive — the "default" tokenizer's RemoveLongFilter(40) already
                // drops tokens over 40 chars before indexing, so this is normally a
                // no-op. But a token that long can never appear in the index, so a
                // MUST clause requiring one would guarantee zero matches; skip it.
                if token.text.len() <= 40 {
                    terms.push(Term::from_field_text(self.schema.filepath, &token.text));
                }
            });
        }

        if terms.is_empty() {
            return Ok(None);
        }

        let must_all: Vec<Box<dyn Query>> = terms
            .into_iter()
            .map(|t| Box::new(TermQuery::new(t, IndexRecordOption::Basic)) as Box<dyn Query>)
            .collect();
        Ok(Some(Box::new(BooleanQuery::intersection(must_all))))
    }
}

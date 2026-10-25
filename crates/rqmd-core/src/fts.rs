//! Tantivy FTS wrapper.
//!
//! Field weights mirror qmd's SQLite FTS5 weights: filepath=1.5, title=4.0, body=1.0.
//! Results are joined back to rusqlite by `filepath` = "collection/path".

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tantivy::{
    Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument, TantivyError, Term,
    collector::{DocSetCollector, FilterCollector, TopDocs},
    directory::error::LockError,
    doc,
    query::{BooleanQuery, ConstScoreQuery, PhraseQuery, Query, QueryParser, TermQuery},
    schema::{FAST, Field, IndexRecordOption, STORED, Schema, SchemaBuilder, TEXT, Value},
    tokenizer::{LowerCaser, RemoveLongFilter, SimpleTokenizer, TextAnalyzer},
};
use unicode_normalization::UnicodeNormalization;

use crate::cjk_tokenizer::CjkBigramFilter;

/// Normalize to Unicode NFC (canonical composition). Tantivy's default
/// tokenizer treats a combining mark as a token separator, so the same text
/// encoded as NFD (e.g. "impôt" as 'o' + U+0302 rather than the single
/// codepoint U+00F4) tokenizes differently and can fail to match text
/// encoded the other way — a document indexed in one form and queried in the
/// other otherwise misses (qmd#966 parity, rqmd#86). Applying this
/// consistently at both index time (`add_document`'s title/body) and query
/// time (`search_fts_multi`) means every stored and queried term goes
/// through tokenization in one canonical form. This does not touch the
/// `filepath` field — normalizing on-disk paths raises its own questions
/// (e.g. macOS's HFS+/APFS historically returning NFD from directory
/// listings) that are out of scope here.
///
/// Only applies going forward: `Store::index_document_fts_only_with_raw`
/// skips the `add_document` call entirely when a file's content hash is
/// unchanged, so a document already indexed in NFD form before this existed
/// keeps its old, unnormalized Tantivy entry until its content changes on
/// disk or its collection is removed and re-added — there's no dedicated
/// "reindex FTS only" command to force it otherwise.
fn normalize_nfc(s: &str) -> String {
    s.nfc().collect()
}

/// How long `writer_mut` waits for another process's `IndexWriter` to release
/// the Tantivy lock before giving up. `0` disables waiting (fail immediately,
/// matching the old behavior). Overridable via `RQMD_LOCK_WAIT_SECS`.
const DEFAULT_LOCK_WAIT_SECS: u64 = 30;

fn lock_wait_budget() -> Duration {
    let secs = std::env::var("RQMD_LOCK_WAIT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_LOCK_WAIT_SECS);
    Duration::from_secs(secs)
}

/// Tantivy's `default` analyzer with CJK runs split into bigrams ahead of the
/// length filter. Registered under `default` so every `TEXT` field — indexing,
/// query parsing and `path_terms` alike — picks it up. Changing it changes
/// what the index must contain: bump [`crate::store::FTS_TOKENIZER_VERSION`].
/// Filler words dropped from a plain natural-language keyword query. Kept
/// small and English-only on purpose: a word here is removed from the query
/// whenever the query is long enough to qualify, so only words that never
/// carry a topic belong in it.
const KEYWORD_STOPWORDS: &[&str] = &[
    "a", "about", "an", "and", "are", "as", "at", "be", "been", "but", "by", "can", "did", "do",
    "does", "for", "from", "had", "has", "have", "how", "i", "in", "into", "is", "it", "its", "of",
    "on", "or", "that", "the", "their", "there", "these", "this", "to", "was", "were", "what",
    "when", "where", "which", "who", "why", "will", "with", "would",
];

/// Query-time weight of the `frontmatter` field relative to `body` (1.0).
const FRONTMATTER_BOOST: f32 = 2.0;

/// Fewest whitespace-separated terms a query needs before filler words are
/// dropped; shorter queries are usually deliberate keywords.
const STOPWORD_MIN_TERMS: usize = 4;

/// Remove filler words from a plain natural-language keyword query.
///
/// A query is left exactly as typed when it has fewer than
/// `STOPWORD_MIN_TERMS` terms, or uses any explicit syntax: quotes, a field
/// specifier, grouping, `+`/`-`/`*`/`^`/`~` modifiers, or an uppercase
/// `AND`/`OR`/`NOT`. Only terms made entirely of ASCII letters are ever
/// removed, so a term containing CJK text is always kept. If every term would
/// be removed the original query is returned.
fn strip_keyword_stopwords(query: &str) -> std::borrow::Cow<'_, str> {
    let terms: Vec<&str> = query.split_whitespace().collect();
    let has_syntax = query.contains(|c: char| {
        matches!(
            c,
            '"' | ':' | '(' | ')' | '[' | ']' | '{' | '}' | '+' | '-' | '*' | '^' | '~' | '!'
        )
    }) || terms.iter().any(|t| matches!(*t, "AND" | "OR" | "NOT"));
    if terms.len() < STOPWORD_MIN_TERMS || has_syntax {
        return std::borrow::Cow::Borrowed(query);
    }
    let kept: Vec<&str> = terms
        .iter()
        .copied()
        .filter(|t| {
            let filler = t.chars().all(|c| c.is_ascii_alphabetic())
                && KEYWORD_STOPWORDS.contains(&t.to_ascii_lowercase().as_str());
            !filler
        })
        .collect();
    if kept.is_empty() {
        return std::borrow::Cow::Borrowed(query);
    }
    std::borrow::Cow::Owned(kept.join(" "))
}

fn analyzer() -> TextAnalyzer {
    TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(CjkBigramFilter)
        .filter(RemoveLongFilter::limit(40))
        .filter(LowerCaser)
        .build()
}

pub struct FtsSchema {
    pub schema: Schema,
    pub filepath: Field,
    pub title: Field,
    pub body: Field,
    /// Curated frontmatter terms (tags and aliases), searched at a higher
    /// weight than prose. `None` for an index built before the field existed:
    /// that index keeps working, without the field, until it is rebuilt.
    pub frontmatter: Option<Field>,
    /// Stored-only field holding the rusqlite document ID for fast join-back.
    pub doc_id: Field,
}

impl FtsSchema {
    /// Read the fields back out of an existing index's schema.
    fn from_schema(schema: Schema) -> Result<Self> {
        let field = |name: &str| {
            schema
                .get_field(name)
                .with_context(|| format!("tantivy index has no `{name}` field"))
        };
        Ok(Self {
            filepath: field("filepath")?,
            title: field("title")?,
            body: field("body")?,
            frontmatter: schema.get_field("frontmatter").ok(),
            doc_id: field("doc_id")?,
            schema,
        })
    }

    pub fn build() -> Self {
        let mut builder = SchemaBuilder::new();
        // filepath is stored + tokenized (enables path-based filtering)
        let filepath = builder.add_text_field("filepath", TEXT | STORED);
        let title = builder.add_text_field("title", TEXT | STORED);
        // body is tokenized but not stored — body comes from rusqlite content table
        let body = builder.add_text_field("body", TEXT);
        let frontmatter = builder.add_text_field("frontmatter", TEXT);
        // doc_id stored as an i64 fast field for retrieval
        let doc_id = builder.add_i64_field("doc_id", STORED | FAST);
        let schema = builder.build();
        Self {
            schema,
            filepath,
            title,
            body,
            frontmatter: Some(frontmatter),
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
    /// Index directory, kept only to name it in the LockBusy error message.
    dir: PathBuf,
}

impl FtsIndex {
    /// Open or create the Tantivy index at `dir`.
    pub fn open_or_create(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir).context("create tantivy dir")?;
        let mmap = || tantivy::directory::MmapDirectory::open(dir).context("mmap directory");

        // An index that already exists keeps the schema it was built with, so a
        // read-only opener never trips over an index from an older binary; the
        // current schema is only used for a new index, or after `rebuild`.
        let (index, schema) = if Index::exists(&mmap()?).context("probe tantivy index")? {
            let index = Index::open(mmap()?).context("open tantivy index")?;
            let schema = FtsSchema::from_schema(index.schema())?;
            (index, schema)
        } else {
            let schema = FtsSchema::build();
            let index = Index::create(
                mmap()?,
                schema.schema.clone(),
                tantivy::IndexSettings::default(),
            )
            .context("create tantivy index")?;
            (index, schema)
        };

        let (reader, query_parser) = Self::reader_and_parser(&index, &schema)?;
        Ok(Self {
            schema,
            index,
            reader,
            writer: None,
            query_parser,
            dir: dir.to_path_buf(),
        })
    }

    fn reader_and_parser(index: &Index, schema: &FtsSchema) -> Result<(IndexReader, QueryParser)> {
        index.tokenizers().register("default", analyzer());

        // Manual reload policy: we call reader.reload() explicitly in commit()
        // so searches immediately after indexing see the new documents.
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
            .context("index reader")?;

        let mut fields = vec![schema.filepath, schema.title, schema.body];
        fields.extend(schema.frontmatter);
        let mut query_parser = QueryParser::for_index(index, fields);
        // BM25 field boosts: filepath 1.5, title 4.0, frontmatter 2.0, body 1.0.
        query_parser.set_field_boost(schema.filepath, 1.5);
        query_parser.set_field_boost(schema.title, 4.0);
        query_parser.set_field_boost(schema.body, 1.0);
        if let Some(frontmatter) = schema.frontmatter {
            query_parser.set_field_boost(frontmatter, FRONTMATTER_BOOST);
        }
        Ok((reader, query_parser))
    }

    /// Whether this index was built without the `frontmatter` field, so
    /// [`FtsIndex::rebuild`] has to recreate it with the current schema.
    pub fn is_legacy_schema(&self) -> bool {
        self.schema.frontmatter.is_none()
    }

    /// Acquire the writer on first call; subsequent calls reuse it.
    ///
    /// On `LockBusy` (another process's `IndexWriter` is open on this same
    /// directory), retries with backoff up to `RQMD_LOCK_WAIT_SECS` (default
    /// 30s, `0` disables waiting) instead of failing on the first attempt —
    /// a short-lived concurrent writer (e.g. a background reindex hook) is
    /// expected to release the lock well within that window. Any other error
    /// fails immediately, as before.
    fn writer_mut(&mut self) -> Result<&mut IndexWriter> {
        if self.writer.is_none() {
            let budget = lock_wait_budget();
            let deadline = Instant::now() + budget;
            let mut backoff = Duration::from_millis(100);
            loop {
                match self.index.writer(50_000_000) {
                    Ok(w) => {
                        self.writer = Some(w);
                        break;
                    }
                    Err(TantivyError::LockFailure(LockError::LockBusy, _))
                        if Instant::now() < deadline =>
                    {
                        std::thread::sleep(backoff.min(deadline - Instant::now()));
                        backoff = (backoff * 2).min(Duration::from_secs(2));
                    }
                    Err(TantivyError::LockFailure(LockError::LockBusy, _)) => {
                        anyhow::bail!(
                            "another rqmd process is writing to {}; wait for it to finish or set RQMD_LOCK_WAIT_SECS",
                            self.dir.display()
                        );
                    }
                    Err(e) => return Err(e).context("index writer"),
                }
            }
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
        self.add_document_with_frontmatter(filepath, title, body, "", doc_id)
    }

    /// Like [`FtsIndex::add_document`], with the document's frontmatter terms
    /// (tags and aliases, space-separated) indexed in their own weighted field.
    /// An index built before that field existed ignores `frontmatter`.
    pub fn add_document_with_frontmatter(
        &mut self,
        filepath: &str,
        title: &str,
        body: &str,
        frontmatter: &str,
        doc_id: i64,
    ) -> Result<()> {
        self.delete_by_filepath(filepath)?;
        self.insert_document(filepath, title, body, frontmatter, doc_id)
    }

    /// Add an entry without first deleting `filepath`. For a caller that has
    /// already staged the deletes for every path it writes.
    pub fn insert_document(
        &mut self,
        filepath: &str,
        title: &str,
        body: &str,
        frontmatter: &str,
        doc_id: i64,
    ) -> Result<()> {
        let title = normalize_nfc(title);
        let body = normalize_nfc(body);
        let frontmatter_field = self.schema.frontmatter;
        let mut document = doc!(
            self.schema.filepath => filepath,
            self.schema.title => title,
            self.schema.body => body,
            self.schema.doc_id => doc_id,
        );
        if let Some(field) = frontmatter_field.filter(|_| !frontmatter.is_empty()) {
            document.add_text(field, normalize_nfc(frontmatter));
        }
        self.writer_mut()?.add_document(document)?;
        Ok(())
    }

    /// Replace the whole index with `docs` — `(filepath, title, body,
    /// frontmatter, doc_id)` — committing once at the end. An index built
    /// before the `frontmatter` field existed is recreated with the current
    /// schema first. If `docs` fails partway the staged deletion and additions
    /// are rolled back, so the previous contents stay searchable (a recreated
    /// index starts empty and is not rolled back).
    pub fn rebuild(
        &mut self,
        docs: impl IntoIterator<Item = Result<(String, String, String, String, i64)>>,
    ) -> Result<()> {
        if self.is_legacy_schema() {
            self.recreate_with_current_schema()?;
        }
        self.writer_mut()?
            .delete_all_documents()
            .context("delete all tantivy docs")?;
        for doc in docs {
            let staged = doc.and_then(|(filepath, title, body, frontmatter, doc_id)| {
                self.insert_document(&filepath, &title, &body, &frontmatter, doc_id)
            });
            if let Err(e) = staged {
                self.writer_mut()?
                    .rollback()
                    .context("roll back partial tantivy rebuild")?;
                return Err(e);
            }
        }
        self.commit()
    }

    /// Throw the on-disk index away and start an empty one with the current
    /// schema. Needs the writer lock, which it waits for like any write.
    fn recreate_with_current_schema(&mut self) -> Result<()> {
        // Taking the lock first keeps another writer from racing the delete.
        self.writer_mut()?;
        self.writer = None;
        for entry in std::fs::read_dir(&self.dir).context("read tantivy dir")? {
            let path = entry.context("tantivy dir entry")?.path();
            if path
                .file_name()
                .is_some_and(|n| n == ".tantivy-writer.lock")
            {
                continue;
            }
            if path.is_dir() {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            }
            .with_context(|| format!("remove {}", path.display()))?;
        }
        let schema = FtsSchema::build();
        let index = Index::create(
            tantivy::directory::MmapDirectory::open(&self.dir).context("mmap directory")?,
            schema.schema.clone(),
            tantivy::IndexSettings::default(),
        )
        .context("recreate tantivy index")?;
        let (reader, query_parser) = Self::reader_and_parser(&index, &schema)?;
        self.schema = schema;
        self.index = index;
        self.reader = reader;
        self.query_parser = query_parser;
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

    /// The stored `filepath` of every live document in the committed index.
    /// Staged, uncommitted writes are not visible here.
    pub fn indexed_filepaths(&self) -> Result<Vec<String>> {
        let searcher = self.reader.searcher();
        let mut paths = Vec::new();
        for segment in searcher.segment_readers() {
            let store = segment
                .get_store_reader(1)
                .context("tantivy store reader")?;
            for doc in store.iter::<TantivyDocument>(segment.alive_bitset()) {
                let doc = doc.context("read tantivy doc")?;
                if let Some(path) = doc.get_first(self.schema.filepath).and_then(|v| v.as_str()) {
                    paths.push(path.to_string());
                }
            }
        }
        Ok(paths)
    }

    /// The stored `filepath` of every committed entry that
    /// [`FtsIndex::delete_by_filepath`] would remove for `filepath`. The match
    /// is a token phrase, so a longer path that contains this one
    /// (`proj/docs/a.md` for `docs/a.md`) is included.
    pub fn filepaths_matched_by(&self, filepath: &str) -> Result<Vec<String>> {
        let Some(query) = self.exact_filepath_query(filepath)? else {
            return Ok(Vec::new());
        };
        let searcher = self.reader.searcher();
        let mut paths = Vec::new();
        for addr in searcher.search(query.as_ref(), &DocSetCollector)? {
            let doc: TantivyDocument = searcher.doc(addr).context("read tantivy doc")?;
            if let Some(path) = doc.get_first(self.schema.filepath).and_then(|v| v.as_str()) {
                paths.push(path.to_string());
            }
        }
        Ok(paths)
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
    ///
    /// A `None` writer means `add_document`/`delete_by_filepath` were never
    /// called since this `FtsIndex` was opened — nothing is staged. Returning
    /// early avoids acquiring the exclusive Tantivy writer lock (and waiting
    /// on `writer_mut`'s retry loop) just to commit an empty segment.
    pub fn commit(&mut self) -> Result<()> {
        let Some(writer) = self.writer.as_mut() else {
            return Ok(());
        };
        writer.commit().context("tantivy commit")?;
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
        self.search_fts_scoped(query_text, limit, collections, None)
    }

    /// Like `search_fts_multi`, but with the exact set of document ids that are
    /// in scope. The id set is applied while collecting, on the `doc_id` fast
    /// field, so the top-`limit` cut only ever counts in-scope documents and
    /// needs neither the tokenized path pushdown nor an overscan. `allowed:
    /// None` keeps the pushdown-and-overscan path of `search_fts_multi`.
    pub fn search_fts_scoped(
        &self,
        query_text: &str,
        limit: usize,
        collections: Option<&[String]>,
        allowed: Option<&Arc<HashSet<i64>>>,
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

        // Same NFC normalization `add_document` applies to title/body — a
        // query must go through tokenization in the same canonical form as
        // whatever's indexed, or the two can silently miss each other.
        let query_text = normalize_nfc(query_text);
        let query_text = strip_keyword_stopwords(&query_text);

        // `parse_query` fails hard on a fragment tantivy reads as a field specifier
        // (e.g. "error: connection refused" — the colon looks like `field:value`).
        // `parse_query_lenient` degrades that fragment to a best-effort clause
        // instead of returning zero results with no explanation; surface each
        // degradation as a warning so it's diagnosable.
        let (mut query, parse_errors) = self.query_parser.parse_query_lenient(&query_text);
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
        if let Some(cols) = collections
            .filter(|c| !c.is_empty())
            .filter(|_| allowed.is_none())
        {
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
        let scoped = allowed.is_none() && collections.is_some_and(|c| !c.is_empty());
        let fetch_limit = if scoped {
            // `Ord::clamp` panics if its min bound exceeds its max bound, which
            // `limit` alone would do here once `limit > FETCH_OVERSCAN_CAP` — a
            // caller-supplied `limit` (e.g. an MCP client's request) must not be
            // able to panic this. `saturating_mul(FACTOR)` is always >= `limit`
            // for `limit >= 1`, so `.min(FETCH_OVERSCAN_CAP.max(limit))` yields
            // the same result as the old clamp when `limit <= CAP`, and falls
            // back to exactly `limit` (no overscan) once `limit` alone exceeds
            // the cap, instead of panicking.
            limit
                .saturating_mul(FETCH_OVERSCAN_FACTOR)
                .min(FETCH_OVERSCAN_CAP.max(limit))
        } else {
            limit
        };

        let collector = TopDocs::with_limit(fetch_limit).order_by_score();
        let top_docs = match allowed {
            Some(ids) => {
                let ids = Arc::clone(ids);
                let filtered = FilterCollector::new(
                    "doc_id".to_string(),
                    move |id: i64| ids.contains(&id),
                    collector,
                );
                searcher.search(&query, &filtered)
            }
            None => searcher.search(&query, &collector),
        }
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

    /// A substring of an unsegmented CJK run must match; the default
    /// tokenizer indexes the run as one token (dropped past 40 bytes).
    #[test]
    fn a_substring_of_an_unsegmented_cjk_run_matches() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut idx = FtsIndex::open_or_create(dir.path()).unwrap();
        idx.add_document("notes/jp.md", "Title", "私は昨日東京都庁を訪れました", 1)
            .unwrap();
        idx.commit().unwrap();

        assert_eq!(idx.search_fts("東京都", 10, None).unwrap().len(), 1);
        assert_eq!(idx.search_fts("都庁を", 10, None).unwrap().len(), 1);
        assert_eq!(
            idx.search_fts("私は昨日東京都庁を訪れました", 10, None)
                .unwrap()
                .len(),
            1
        );
        assert!(idx.search_fts("大阪府", 10, None).unwrap().is_empty());
    }

    #[test]
    fn cjk_bigrams_do_not_match_across_a_gap() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut idx = FtsIndex::open_or_create(dir.path()).unwrap();
        idx.add_document("a.md", "T", "東京と大阪", 1).unwrap();
        idx.commit().unwrap();

        // 東大 is not an adjacent pair in the source; 京と is.
        assert!(idx.search_fts("東大", 10, None).unwrap().is_empty());
        assert_eq!(idx.search_fts("京と", 10, None).unwrap().len(), 1);
    }

    #[test]
    fn phrase_positions_stay_consistent_around_a_cjk_run() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut idx = FtsIndex::open_or_create(dir.path()).unwrap();
        idx.add_document("a.md", "T", "abc 東京都庁 def", 1)
            .unwrap();
        idx.commit().unwrap();

        assert_eq!(
            idx.search_fts("\"abc 東京都庁 def\"", 10, None)
                .unwrap()
                .len(),
            1
        );
        assert!(idx.search_fts("\"abc def\"", 10, None).unwrap().is_empty());
    }

    #[test]
    fn replacing_and_deleting_a_document_with_a_cjk_path_works() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut idx = FtsIndex::open_or_create(dir.path()).unwrap();
        idx.add_document("notes/東京都庁.md", "T", "first body", 1)
            .unwrap();
        idx.commit().unwrap();
        idx.add_document("notes/東京都庁.md", "T", "second body", 1)
            .unwrap();
        idx.commit().unwrap();
        assert_eq!(idx.reader.searcher().num_docs(), 1);

        idx.delete_by_filepath("notes/東京都庁.md").unwrap();
        idx.commit().unwrap();
        assert_eq!(idx.reader.searcher().num_docs(), 0);
    }

    #[test]
    fn a_failed_rebuild_leaves_the_previous_index_searchable() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut idx = FtsIndex::open_or_create(dir.path()).unwrap();
        idx.add_document("keep.md", "T", "keepmeterm", 1).unwrap();
        idx.commit().unwrap();

        let err = idx
            .rebuild(vec![
                Ok((
                    "new.md".to_string(),
                    "T".to_string(),
                    "newterm".to_string(),
                    String::new(),
                    2,
                )),
                Err(anyhow::anyhow!("row unreadable")),
            ])
            .unwrap_err();
        assert!(err.to_string().contains("row unreadable"));
        idx.commit().unwrap();

        assert_eq!(idx.search_fts("keepmeterm", 10, None).unwrap().len(), 1);
        assert!(idx.search_fts("newterm", 10, None).unwrap().is_empty());
    }

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

    /// A `commit()` with nothing staged (writer never acquired) must succeed
    /// without ever creating an `IndexWriter` — regression test for the
    /// `LockBusy` failure reported on `rqmd update` when every file in a
    /// collection hashed `Unchanged`: the old code unconditionally acquired
    /// the exclusive writer lock just to commit an empty segment.
    #[test]
    fn commit_with_nothing_staged_does_not_acquire_writer() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut idx = FtsIndex::open_or_create(dir.path()).unwrap();

        idx.commit().unwrap();
        assert!(
            idx.writer.is_none(),
            "a no-op commit must not lazily create an IndexWriter"
        );
    }

    /// A second `FtsIndex` on the same directory must be able to open and
    /// commit a genuine no-op while the first still holds its writer open —
    /// this is exactly the concurrent-hook-vs-manual-run scenario that
    /// produced the reported `LockBusy` error, minus the actual write.
    #[test]
    fn second_index_no_op_flush_succeeds_while_first_holds_writer() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut first = FtsIndex::open_or_create(dir.path()).unwrap();
        first
            .add_document("notes/a.md", "Title", "body", 1)
            .unwrap();
        // Force the writer to be acquired without committing (releasing) it.
        first.writer_mut().unwrap();

        let mut second = FtsIndex::open_or_create(dir.path()).unwrap();
        second
            .commit()
            .expect("no-op commit must not need the writer lock");
    }

    /// When the second index genuinely has something to commit, it must wait
    /// (not fail instantly) for the first index's writer to be released, and
    /// succeed once it is. Uses a short `RQMD_LOCK_WAIT_SECS` budget so the
    /// test doesn't burn the default 30s wait.
    #[test]
    fn writer_mut_waits_for_lock_release_within_budget() {
        // SAFETY: test-only env mutation; this test does not run concurrently
        // with other tests reading this var (fts.rs tests are single-threaded
        // in practice here since each opens its own TempDir and none else
        // reads RQMD_LOCK_WAIT_SECS).
        unsafe {
            std::env::set_var("RQMD_LOCK_WAIT_SECS", "5");
        }

        let dir = tempfile::TempDir::new().unwrap();
        let mut first = FtsIndex::open_or_create(dir.path()).unwrap();
        first
            .add_document("notes/a.md", "Title", "body", 1)
            .unwrap();
        first.writer_mut().unwrap(); // holds the lock, never commits

        let released = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let released_writer = released.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(first);
            released_writer.store(true, std::sync::atomic::Ordering::SeqCst);
        });

        let mut second = FtsIndex::open_or_create(dir.path()).unwrap();
        second
            .add_document("notes/b.md", "Title", "other", 2)
            .unwrap();
        second
            .commit()
            .expect("must wait for the first writer to release, then succeed");
        assert!(
            released.load(std::sync::atomic::Ordering::SeqCst),
            "commit must not have returned before the lock was actually released"
        );

        handle.join().unwrap();
        unsafe {
            std::env::remove_var("RQMD_LOCK_WAIT_SECS");
        }
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

    /// AC-2 regression (github.com/tylern91/rqmd#86): a scoped search whose
    /// `limit` exceeds `FETCH_OVERSCAN_CAP` must not panic. Before the fix,
    /// `.clamp(limit, FETCH_OVERSCAN_CAP)` panicked as soon as `limit` (the
    /// clamp's min bound) exceeded the 5000 cap (its max bound) — reachable
    /// from any MCP client that sends a large `limit` alongside a non-empty
    /// `collections` filter.
    #[test]
    fn search_fts_multi_scoped_query_with_limit_above_overscan_cap_does_not_panic() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut idx = FtsIndex::open_or_create(dir.path()).unwrap();
        idx.add_document("notes/doc.md", "Title", "shared", 0)
            .unwrap();
        idx.commit().unwrap();

        let results = idx
            .search_fts_multi("shared", 5001, Some(&["notes".to_string()]))
            .unwrap();
        assert_eq!(results.len(), 1);
    }

    /// qmd#966 parity (rqmd#86): "impôt" NFC is a single U+00F4 codepoint;
    /// NFD is 'o' + U+0302 (combining circumflex). Tantivy's default
    /// tokenizer treats a combining mark as a separator, so an unnormalized
    /// NFD query tokenizes to "impo"+"t" instead of one "impôt" token and
    /// misses an NFC-indexed document — confirmed to fail before
    /// `normalize_nfc` was applied at both index and query time. Covers both
    /// directions: whichever form is indexed, a query in the other form must
    /// still match, since both go through the same NFC normalization.
    #[test]
    fn nfd_and_nfc_forms_of_the_same_text_match_each_other() {
        let nfc = "impôt"; // U+0069 U+006D U+0070 U+00F4 U+0074
        let nfd = "impo\u{0302}t"; // U+0069 U+006D U+0070 U+006F U+0302 U+0074
        assert_ne!(
            nfc.chars().count(),
            nfd.chars().count(),
            "sanity: the two encodings must actually differ"
        );

        let dir = tempfile::TempDir::new().unwrap();
        let mut idx = FtsIndex::open_or_create(dir.path()).unwrap();
        idx.add_document("notes/nfc.md", "Title", nfc, 1).unwrap();
        idx.add_document("notes/nfd.md", "Title", nfd, 2).unwrap();
        idx.commit().unwrap();

        for (query, label) in [(nfd, "NFD query"), (nfc, "NFC query")] {
            let hits = idx.search_fts(query, 10, None).unwrap();
            assert_eq!(
                hits.len(),
                2,
                "{label} {query:?} must match both the NFC- and NFD-indexed document"
            );
        }
    }

    #[test]
    fn ac_1_question_shaped_query_ranks_the_content_words_first() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut idx = FtsIndex::open_or_create(dir.path()).unwrap();
        // The content words are common across the corpus, so each one is a
        // weak signal; the filler words are rare, so left in the query they
        // would pull the filler-heavy document to the top.
        idx.add_document(
            "notes/filler.md",
            "Chatter",
            "what with the what with the what with the",
            1,
        )
        .unwrap();
        for i in 0..12 {
            idx.add_document(
                &format!("notes/launch-{i}.md"),
                "Postmortem",
                "the launch went wrong because the rollout skipped a step",
                10 + i,
            )
            .unwrap();
        }
        idx.commit().unwrap();

        let hits = idx
            .search_fts_multi("what went wrong with the launch", 1, None)
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].0.starts_with("notes/launch-"), "got {hits:?}");
    }

    #[test]
    fn ac_2_short_or_syntax_queries_are_left_exactly_as_typed() {
        for q in [
            "the launch",
            "what is the",
            "\"what went wrong with the launch\"",
            "what went wrong with the -launch",
            "title:what went wrong with the launch",
            "what went wrong AND the launch",
            "what went wrong with (the launch)",
            "what went wrong with the launch*",
        ] {
            assert_eq!(
                strip_keyword_stopwords(q),
                q,
                "query {q:?} must be unchanged"
            );
        }
        assert_eq!(
            strip_keyword_stopwords("what went wrong with the launch"),
            "went wrong launch"
        );
    }

    #[test]
    fn ac_3_a_query_of_only_stopwords_keeps_its_original_text() {
        let q = "what is the with that";
        assert_eq!(strip_keyword_stopwords(q), q);
    }

    #[test]
    fn ac_4_terms_containing_cjk_text_are_never_removed() {
        assert_eq!(
            strip_keyword_stopwords("what is the 東京 launch"),
            "東京 launch"
        );
        assert_eq!(
            strip_keyword_stopwords("the 日本語 with 検索 is"),
            "日本語 検索"
        );
        // A term that mixes letters and CJK text is not a plain stopword.
        assert_eq!(
            strip_keyword_stopwords("the isの a launch plan"),
            "isの launch plan"
        );
    }

    #[test]
    fn fm_ac_1_a_frontmatter_match_outranks_a_single_body_mention() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut idx = FtsIndex::open_or_create(dir.path()).unwrap();
        idx.add_document(
            "notes/prose.md",
            "Prose",
            "a long note about gardening that mentions zirconia once in passing",
            1,
        )
        .unwrap();
        idx.add_document_with_frontmatter(
            "notes/tagged.md",
            "Tagged",
            "a long note about gardening with nothing else to say here today",
            "zirconia",
            2,
        )
        .unwrap();
        idx.commit().unwrap();

        let hits = idx.search_fts("zirconia", 10, None).unwrap();
        assert_eq!(hits.len(), 2, "got {hits:?}");
        assert_eq!(hits[0].0, "notes/tagged.md", "got {hits:?}");
    }

    #[test]
    fn fm_ac_2_a_document_without_frontmatter_is_indexed_and_ranked_as_before() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut idx = FtsIndex::open_or_create(dir.path()).unwrap();
        idx.add_document("a.md", "A", "quartz quartz quartz crystal", 1)
            .unwrap();
        idx.add_document(
            "b.md",
            "B",
            "quartz and a long unrelated sentence follows",
            2,
        )
        .unwrap();
        idx.commit().unwrap();

        let hits = idx.search_fts("quartz", 10, None).unwrap();
        let paths: Vec<_> = hits.iter().map(|h| h.0.as_str()).collect();
        assert_eq!(paths, ["a.md", "b.md"]);
    }

    /// Build a Tantivy index with the field set this crate used before the
    /// `frontmatter` field existed.
    fn write_legacy_index(dir: &Path) {
        let mut builder = SchemaBuilder::new();
        let filepath = builder.add_text_field("filepath", TEXT | STORED);
        let title = builder.add_text_field("title", TEXT | STORED);
        let body = builder.add_text_field("body", TEXT);
        let doc_id = builder.add_i64_field("doc_id", STORED | FAST);
        let index = Index::create_in_dir(dir, builder.build()).unwrap();
        index.tokenizers().register("default", analyzer());
        let mut writer: IndexWriter = index.writer(15_000_000).unwrap();
        writer
            .add_document(
                doc!(filepath => "old.md", title => "Old", body => "legacyterm", doc_id => 1i64),
            )
            .unwrap();
        writer.commit().unwrap();
    }

    #[test]
    fn fm_ac_3_a_legacy_index_still_opens_and_rebuilds_into_the_current_schema() {
        let dir = tempfile::TempDir::new().unwrap();
        write_legacy_index(dir.path());

        let mut idx = FtsIndex::open_or_create(dir.path()).unwrap();
        assert!(idx.is_legacy_schema());
        assert_eq!(idx.search_fts("legacyterm", 10, None).unwrap().len(), 1);

        idx.rebuild(vec![Ok((
            "old.md".to_string(),
            "Old".to_string(),
            "legacyterm".to_string(),
            "taggedterm".to_string(),
            1,
        ))])
        .unwrap();
        assert!(!idx.is_legacy_schema());
        assert_eq!(idx.search_fts("taggedterm", 10, None).unwrap().len(), 1);

        let reopened = FtsIndex::open_or_create(dir.path()).unwrap();
        assert!(!reopened.is_legacy_schema());
        assert_eq!(
            reopened.search_fts("taggedterm", 10, None).unwrap().len(),
            1
        );
    }
}

//! Shared document-resolution logic for `get`/`multi-get`, used by both the
//! CLI and the MCP server so path matching has a single implementation.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use globset::{Glob, GlobMatcher};
use rusqlite::Connection;

use crate::db;
use crate::types::Document;

/// Compile a glob pattern. Wildcards match path separators too — globset's
/// `literal_separator` is off by default, so `docs/*` matches `docs/a/b.md`
/// as well as `docs/a.md`, preserving the cross-`/` matching of the
/// hand-rolled `*`-only matcher this replaced.
fn compile_glob(pattern: &str) -> Result<GlobMatcher> {
    Ok(Glob::new(pattern)
        .with_context(|| format!("invalid glob pattern: {pattern}"))?
        .compile_matcher())
}

/// Counts, for plain `multi-get` entries, how many of the documents
/// `db::find_documents_by_needles` returned each entry matches, using the
/// same four arms as that query's SQL: the `=` arms compare bytes exactly,
/// the `LIKE '%/' || needle` arms fold ASCII case as SQLite's `LIKE` does.
/// The query itself does not say which needle produced which row, so this
/// rebuilds that per entry; it must stay in lockstep with `NEEDLE_CLAUSE` in
/// `db.rs`.
///
/// Built once per call: every document's exact keys and every `/`-anchored
/// suffix go into a map, so counting an entry is a lookup rather than a scan
/// of every document.
struct NeedleIndex {
    /// `path` and `collection/path`, as stored.
    exact: HashMap<String, Vec<usize>>,
    /// The ASCII-lowercased text after each `/` in `path` and
    /// `collection/path`.
    suffix: HashMap<String, Vec<usize>>,
}

impl NeedleIndex {
    fn new(docs: &[Document]) -> Self {
        let mut exact: HashMap<String, Vec<usize>> = HashMap::new();
        let mut suffix: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, doc) in docs.iter().enumerate() {
            let full = format!("{}/{}", doc.collection, doc.path);
            for key in [&doc.path, &full] {
                exact.entry(key.clone()).or_default().push(i);
                let lower = key.to_ascii_lowercase();
                for (at, _) in lower.match_indices('/') {
                    suffix
                        .entry(lower[at + 1..].to_string())
                        .or_default()
                        .push(i);
                }
            }
        }
        Self { exact, suffix }
    }

    /// Distinct documents `needle` matches.
    fn count(&self, needle: &str) -> usize {
        let mut hits: Vec<usize> = self
            .exact
            .get(needle)
            .into_iter()
            .chain(self.suffix.get(&needle.to_ascii_lowercase()))
            .flatten()
            .copied()
            .collect();
        hits.sort_unstable();
        hits.dedup();
        hits.len()
    }
}

/// One entry of a `multi-get` pattern, classified by its form alone.
enum Entry<'a> {
    /// `#hex`: the hex part.
    Docid(&'a str),
    /// Contains a globset metacharacter; `rqmd://` stripped.
    Glob(&'a str),
    /// Anything else; `rqmd://` stripped.
    Path(&'a str),
}

impl<'a> Entry<'a> {
    fn classify(entry: &'a str) -> Self {
        if let Some(hex) = entry.strip_prefix('#') {
            Entry::Docid(hex)
        } else if entry.contains(['*', '?', '[', '{']) {
            // Any globset metacharacter, not just `*` — `compile_glob` uses
            // `globset::Glob`, which also treats `?`, `[a-z]`, and `{a,b}` as
            // wildcards. Classifying on `*` alone routed those patterns into
            // the path (literal-fragment) branch instead, silently matching
            // the wrong documents or none.
            Entry::Glob(entry.trim_start_matches("rqmd://"))
        } else {
            Entry::Path(entry.trim_start_matches("rqmd://"))
        }
    }
}

/// A plain path entry that matched more than one document. All of them are
/// returned; this only says the entry was not specific.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AmbiguousEntry {
    /// The entry as written in the pattern, trimmed.
    pub entry: String,
    /// How many documents it matched.
    pub matches: usize,
}

/// The result of `resolve_multi_get_report`: the documents, plus the entries
/// a caller may want to look at again.
#[derive(Debug, Clone, Default)]
pub struct MultiGetReport {
    /// Exactly what `resolve_multi_get` returns for the same arguments:
    /// every match, each document once, sorted by (collection, path).
    pub documents: Vec<Document>,
    /// Entries, as written and trimmed, that matched no document: a docid
    /// with no document, or a glob or path entry with no match among the
    /// selected collections. In pattern order, each entry once.
    pub unmatched: Vec<String>,
    /// Path entries that matched more than one document, in pattern order,
    /// each entry once. Docid entries never appear here (a shared prefix
    /// resolves to one document), and neither do globs, which are meant to
    /// match many.
    pub ambiguous: Vec<AmbiguousEntry>,
}

#[cfg(test)]
thread_local! {
    /// Full document listings taken by the glob branch on this thread.
    static GLOB_LISTINGS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Resolve a `multi-get` pattern — a comma-separated list mixing `#docid`,
/// glob, and plain path/name entries — against the document set.
///
/// Thin wrapper over `resolve_multi_get_report` that keeps only the
/// documents; see there for the rules.
pub fn resolve_multi_get(
    conn: &Connection,
    collections: Option<&[String]>,
    pattern: &str,
) -> Result<Vec<Document>> {
    Ok(resolve_multi_get_report(conn, collections, pattern)?.documents)
}

/// Resolve a `multi-get` pattern and report which entries matched nothing and
/// which plain entries matched more than one document.
///
/// Each entry's kind comes from its form alone (no fallback between kinds):
/// `#hex` is a docid, an entry containing `*`, `?`, `[` or `{` is a glob, and
/// anything else is a plain path. `rqmd://` is stripped from glob and path
/// entries.
///
/// - A docid entry resolves via `db::get_document_by_docid_prefix`: one
///   document, the first by (collection, path) when a prefix is shared. It
///   is not narrowed by `collections`.
/// - A plain entry resolves via `db::find_documents_by_needles`: every
///   document whose path or `collection/path` equals it or ends with `/` plus
///   it, so a fragment like "SYNTAX.md" never matches "OLD-SYNTAX.md". An
///   exact match does not hide suffix matches; all are returned. Plain
///   entries are looked up in bounded batches, so a list of any length works.
/// - A glob entry matches `collection/path` (see `compile_glob`). All globs
///   are tested against one listing of the documents.
///
/// No kind outranks another: the documents are the union of every entry's
/// matches, de-duplicated by id and sorted by (collection, path), not in
/// entry order.
pub fn resolve_multi_get_report(
    conn: &Connection,
    collections: Option<&[String]>,
    pattern: &str,
) -> Result<MultiGetReport> {
    let entries: Vec<(&str, Entry)> = pattern
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|p| (p, Entry::classify(p)))
        .collect();
    // Documents each entry matched, indexed like `entries`.
    let mut counts = vec![0usize; entries.len()];
    let mut docs: Vec<Document> = Vec::new();

    for (i, (_, entry)) in entries.iter().enumerate() {
        if let Entry::Docid(hex) = entry {
            match db::get_document_by_docid_prefix(conn, hex)? {
                Some(doc) => {
                    docs.push(doc);
                    counts[i] = 1;
                }
                None => tracing::debug!("multi_get: no document found for docid #{hex}"),
            }
        }
    }

    let (path_slots, needles): (Vec<usize>, Vec<&str>) = entries
        .iter()
        .enumerate()
        .filter_map(|(i, (_, e))| match e {
            Entry::Path(needle) => Some((i, *needle)),
            _ => None,
        })
        .unzip();
    if !needles.is_empty() {
        let matched = db::find_documents_by_needles(conn, collections, &needles)?;
        let index = NeedleIndex::new(&matched);
        for (&slot, needle) in path_slots.iter().zip(&needles) {
            counts[slot] = index.count(needle);
            if counts[slot] == 0 {
                tracing::debug!("multi_get: no document matched needle {needle:?}");
            }
        }
        docs.extend(matched);
    }

    let (glob_slots, globs): (Vec<usize>, Vec<&str>) = entries
        .iter()
        .enumerate()
        .filter_map(|(i, (_, e))| match e {
            Entry::Glob(glob) => Some((i, *glob)),
            _ => None,
        })
        .unzip();
    if !globs.is_empty() {
        let matchers = globs
            .iter()
            .map(|g| compile_glob(g))
            .collect::<Result<Vec<_>>>()?;
        #[cfg(test)]
        GLOB_LISTINGS.with(|n| n.set(n.get() + 1));
        for doc in db::list_documents_multi(conn, collections)? {
            let filepath = format!("{}/{}", doc.collection, doc.path);
            let mut any = false;
            for (&slot, matcher) in glob_slots.iter().zip(&matchers) {
                if matcher.is_match(&filepath) {
                    counts[slot] += 1;
                    any = true;
                }
            }
            if any {
                docs.push(doc);
            }
        }
        for (&slot, glob) in glob_slots.iter().zip(&globs) {
            if counts[slot] == 0 {
                tracing::debug!("multi_get: glob {glob:?} matched no documents");
            }
        }
    }

    docs.sort_by(|a, b| {
        (a.collection.as_str(), a.path.as_str()).cmp(&(b.collection.as_str(), b.path.as_str()))
    });
    docs.dedup_by_key(|d| d.id);

    let mut report = MultiGetReport {
        documents: docs,
        ..MultiGetReport::default()
    };
    let mut seen: HashSet<&str> = HashSet::new();
    for ((written, entry), &count) in entries.iter().zip(&counts) {
        let ambiguous = matches!(entry, Entry::Path(_)) && count > 1;
        if (count == 0 || ambiguous) && seen.insert(written) {
            if count == 0 {
                report.unmatched.push((*written).to_string());
            } else {
                report.ambiguous.push(AmbiguousEntry {
                    entry: (*written).to_string(),
                    matches: count,
                });
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tracing::field::{Field, Visit};

    /// Serializes the tests that reach the "no document matched" `debug!`
    /// callsites. `tracing` caches a callsite's interest when it is first
    /// hit; a test hitting one with no subscriber while
    /// `resolve_multi_get_warns_on_unmatched_docid_needle_and_glob` installs
    /// its recorder can cache "never" and hide the event from it.
    static UNMATCHED_LOG_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn compile_glob_wildcard_and_exact() {
        assert!(
            compile_glob("docs/*.md")
                .unwrap()
                .is_match("docs/SYNTAX.md")
        );
        assert!(
            !compile_glob("docs/*.md")
                .unwrap()
                .is_match("other/SYNTAX.md")
        );
        assert!(compile_glob("*").unwrap().is_match("anything/at/all.md"));
        assert!(compile_glob("exact").unwrap().is_match("exact"));
        assert!(!compile_glob("exact").unwrap().is_match("not-exact"));
    }

    #[test]
    fn compile_glob_invalid_pattern_errors() {
        assert!(compile_glob("docs/[unterminated").is_err());
    }

    fn open_test_db(dir: &TempDir) -> Connection {
        let conn = db::open_db(&dir.path().join("test.sqlite")).unwrap();
        for (collection, path) in [("docs", "SYNTAX.md"), ("docs", "guide/INTRO.md")] {
            let hash = db::content_hash(path);
            db::upsert_content(&conn, &hash, "body", "t").unwrap();
            db::upsert_document(&conn, collection, path, "Title", &hash, "t").unwrap();
        }
        conn
    }

    /// Regression test for B1-a: `resolve_multi_get` classified only `*` as a
    /// glob metacharacter, so patterns using `?` or `[a-z]` — both valid
    /// `globset` syntax — fell through to the needle (literal-fragment)
    /// branch and silently matched the wrong documents or none. (`{a,b}`
    /// brace alternation can't be exercised through this entry point: the
    /// top-level pattern string is itself comma-split before classification,
    /// so a literal `,` inside `{...}` is indistinguishable from the
    /// multi-get list separator.)
    #[test]
    fn resolve_multi_get_routes_non_star_metacharacters_to_the_glob_branch() {
        let dir = TempDir::new().unwrap();
        let conn = open_test_db(&dir);

        let docs = resolve_multi_get(&conn, None, "docs/SYNTA?.md").unwrap();
        assert_eq!(docs.len(), 1, "`?` must be treated as a glob wildcard");
        assert_eq!(docs[0].path, "SYNTAX.md");

        let docs = resolve_multi_get(&conn, None, "docs/SYNTA[X].md").unwrap();
        assert_eq!(
            docs.len(),
            1,
            "`[a-z]` character classes must be treated as a glob"
        );
        assert_eq!(docs[0].path, "SYNTAX.md");
    }

    /// Minimal capturing `tracing::Subscriber` — records the formatted message
    /// of every event so a test can assert a `tracing::warn!` fired without
    /// pulling in a dev-dependency just for this.
    #[derive(Default, Clone)]
    struct RecordingSubscriber {
        messages: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    struct MessageVisitor<'a>(&'a mut String);
    impl Visit for MessageVisitor<'_> {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                *self.0 = format!("{value:?}");
            }
        }
    }

    impl tracing::Subscriber for RecordingSubscriber {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            let mut msg = String::new();
            event.record(&mut MessageVisitor(&mut msg));
            self.messages.lock().unwrap().push(msg);
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    /// Regression test for B1-b: an unmatched `#docid`, needle, or glob
    /// pattern used to be silently dropped from the result set with no
    /// signal — indistinguishable from "the document has no content." The
    /// log line is now `debug`, since `resolve_multi_get_report` hands the
    /// same entries to the caller.
    #[test]
    fn resolve_multi_get_warns_on_unmatched_docid_needle_and_glob() {
        let _serial = UNMATCHED_LOG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = TempDir::new().unwrap();
        let conn = open_test_db(&dir);

        let subscriber = RecordingSubscriber::default();
        let messages = subscriber.messages.clone();
        let docs = tracing::subscriber::with_default(subscriber, || {
            resolve_multi_get(
                &conn,
                None,
                "#deadbeef,does-not-exist.md,docs/nope-*.md,docs/SYNTAX.md",
            )
            .unwrap()
        });

        assert_eq!(docs.len(), 1, "only the real match should be returned");
        assert_eq!(docs[0].path, "SYNTAX.md");

        let messages = messages.lock().unwrap();
        assert!(
            messages.iter().any(|m| m.contains("deadbeef")),
            "missing docid warning: {messages:?}"
        );
        assert!(
            messages.iter().any(|m| m.contains("does-not-exist.md")),
            "missing needle warning: {messages:?}"
        );
        assert!(
            messages.iter().any(|m| m.contains("nope-*.md")),
            "missing glob warning: {messages:?}"
        );
    }

    fn insert_docs(conn: &Connection, docs: &[(&str, &str)]) {
        for (collection, path) in docs {
            let body = format!("{collection}/{path}");
            let hash = db::content_hash(&body);
            db::upsert_content(conn, &hash, &body, "t").unwrap();
            db::upsert_document(conn, collection, path, "Title", &hash, "t").unwrap();
        }
    }

    /// github.com/tylern91/rqmd#154, AC-4: unmatched entries of every kind
    /// and plain entries matching several documents are reported, in pattern
    /// order and once each, without changing the documents returned.
    #[test]
    fn report_lists_unmatched_and_ambiguous_entries() {
        let _serial = UNMATCHED_LOG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = TempDir::new().unwrap();
        let conn = open_test_db(&dir);
        insert_docs(
            &conn,
            &[("notes", "README.md"), ("notes", "guide/README.md")],
        );

        let pattern = "README.md, #deadbeef, docs/SYNTAX.md, nope/*.md, missing.md, \
                       README.md, missing.md, docs/guide/*";
        let report = resolve_multi_get_report(&conn, None, pattern).unwrap();

        assert_eq!(
            report.unmatched,
            ["#deadbeef", "nope/*.md", "missing.md"],
            "every unmatched kind, in pattern order, once each"
        );
        assert_eq!(
            report.ambiguous,
            [AmbiguousEntry {
                entry: "README.md".into(),
                matches: 2,
            }],
            "a glob matching one document and an exact path are not ambiguous"
        );
        let docs = resolve_multi_get(&conn, None, pattern).unwrap();
        let ids = |d: &[Document]| d.iter().map(|d| d.id).collect::<Vec<_>>();
        assert_eq!(ids(&report.documents), ids(&docs));
        assert_eq!(report.documents.len(), 4);
    }

    /// The `LIKE` arms of the needle query fold ASCII case, so an entry that
    /// differs from a path only in case does match it; the report must agree
    /// with the query rather than call that entry unmatched.
    #[test]
    fn report_counts_needles_the_way_the_sql_matches_them() {
        let _serial = UNMATCHED_LOG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = TempDir::new().unwrap();
        let conn = open_test_db(&dir);
        insert_docs(&conn, &[("other", "x/syntax.md")]);

        let report = resolve_multi_get_report(&conn, None, "syntax.md").unwrap();
        let paths: Vec<_> = report.documents.iter().map(|d| d.path.as_str()).collect();
        assert_eq!(paths, ["SYNTAX.md", "x/syntax.md"]);
        assert!(report.unmatched.is_empty(), "{report:?}");
        assert_eq!(report.ambiguous[0].matches, 2);

        // The exact arms are case-sensitive: "Docs/SYNTAX.md" is neither an
        // exact path nor a `/`-suffix of any document's path.
        let report = resolve_multi_get_report(&conn, None, "Docs/SYNTAX.md").unwrap();
        let direct = db::find_documents_by_needles(&conn, None, &["Docs/SYNTAX.md"]).unwrap();
        assert_eq!(report.documents.len(), direct.len());
        assert_eq!(report.unmatched.is_empty(), !direct.is_empty());
    }

    /// github.com/tylern91/rqmd#154, AC-6: every glob in a pattern is tested
    /// against one listing of the documents, not one listing per glob.
    #[test]
    fn globs_share_one_document_listing() {
        let _serial = UNMATCHED_LOG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = TempDir::new().unwrap();
        let conn = open_test_db(&dir);

        GLOB_LISTINGS.with(|n| n.set(0));
        let report =
            resolve_multi_get_report(&conn, None, "docs/S*, docs/guide/*, nope/*, docs/*").unwrap();
        assert_eq!(GLOB_LISTINGS.with(|n| n.get()), 1);
        assert_eq!(report.documents.len(), 2);
        assert_eq!(report.unmatched, ["nope/*"]);

        GLOB_LISTINGS.with(|n| n.set(0));
        resolve_multi_get_report(&conn, None, "docs/SYNTAX.md, #abc").unwrap();
        assert_eq!(GLOB_LISTINGS.with(|n| n.get()), 0, "no globs, no listing");
    }
}

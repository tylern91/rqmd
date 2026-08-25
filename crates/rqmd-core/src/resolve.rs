//! Shared document-resolution logic for `get`/`multi-get`, used by both the
//! CLI and the MCP server so path matching has a single implementation.

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

/// Does `doc` match `needle` the same way `db::find_documents_by_needles`'s
/// SQL clause does (exact path, exact "collection/path", or a `/`-anchored
/// suffix of either)? Used only to tell, after the fact, which needles in a
/// batch produced zero hits — kept in lockstep with the SQL by hand since the
/// query itself doesn't report per-needle matches.
fn needle_matches(doc: &Document, needle: &str) -> bool {
    let full = format!("{}/{}", doc.collection, doc.path);
    let suffix = format!("/{needle}");
    doc.path == needle || full == needle || doc.path.ends_with(&suffix) || full.ends_with(&suffix)
}

/// Resolve a `multi-get` pattern — a comma-separated list mixing `#docid`,
/// glob (`*`), and plain path/name entries — against the document set.
///
/// Plain entries are resolved via `db::find_documents_by_needles`, which
/// anchors matches at a path segment boundary (`/`) so a fragment like
/// "SYNTAX.md" can no longer silently match "OLD-SYNTAX.md" — the previous
/// behavior was an unanchored substring match that could return the wrong
/// document with no error. Docid entries resolve deterministically (see
/// `db::get_document_by_docid_prefix`). Results are deduplicated by document
/// id and returned sorted by (collection, path).
pub fn resolve_multi_get(
    conn: &Connection,
    collections: Option<&[String]>,
    pattern: &str,
) -> Result<Vec<Document>> {
    let patterns: Vec<&str> = pattern
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();

    let mut docs: Vec<Document> = Vec::new();
    let mut needles: Vec<&str> = Vec::new();
    let mut globs: Vec<&str> = Vec::new();

    for p in &patterns {
        if let Some(hex) = p.strip_prefix('#') {
            match db::get_document_by_docid_prefix(conn, hex)? {
                Some(doc) => docs.push(doc),
                // No `else` here previously meant a typo'd docid silently
                // shrank the result set with no signal — indistinguishable
                // from "document has no content." Warn so it's diagnosable.
                None => tracing::warn!("multi_get: no document found for docid #{hex}"),
            }
        } else if p.contains(['*', '?', '[', '{']) {
            // Any globset metacharacter, not just `*` — `compile_glob` below
            // uses `globset::Glob`, which also treats `?`, `[a-z]`, and
            // `{a,b}` as wildcards. Classifying on `*` alone routed those
            // patterns into the needle (literal-fragment) branch instead,
            // silently matching the wrong documents or none.
            globs.push(p.trim_start_matches("rqmd://"));
        } else {
            needles.push(p.trim_start_matches("rqmd://"));
        }
    }

    if !needles.is_empty() {
        let matched = db::find_documents_by_needles(conn, collections, &needles)?;
        for needle in &needles {
            let hit = matched.iter().any(|d| needle_matches(d, needle));
            if !hit {
                tracing::warn!("multi_get: no document matched needle {needle:?}");
            }
        }
        docs.extend(matched);
    }

    if !globs.is_empty() {
        let matchers = globs
            .iter()
            .map(|g| compile_glob(g))
            .collect::<Result<Vec<_>>>()?;
        for (pattern, matcher) in globs.iter().zip(matchers.iter()) {
            let mut any = false;
            for doc in db::list_documents_multi(conn, collections)? {
                let filepath = format!("{}/{}", doc.collection, doc.path);
                if matcher.is_match(&filepath) {
                    docs.push(doc);
                    any = true;
                }
            }
            if !any {
                tracing::warn!("multi_get: glob {pattern:?} matched no documents");
            }
        }
    }

    docs.sort_by(|a, b| {
        (a.collection.as_str(), a.path.as_str()).cmp(&(b.collection.as_str(), b.path.as_str()))
    });
    docs.dedup_by_key(|d| d.id);
    Ok(docs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tracing::field::{Field, Visit};

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
    /// signal — indistinguishable from "the document has no content."
    #[test]
    fn resolve_multi_get_warns_on_unmatched_docid_needle_and_glob() {
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
}

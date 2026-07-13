//! Shared document-resolution logic for `get`/`multi-get`, used by both the
//! CLI and the MCP server so path matching has a single implementation.

use anyhow::Result;
use rusqlite::Connection;

use crate::db;
use crate::types::Document;

/// Simple glob: only `*` as wildcard (matches any chars, including `/`).
pub fn glob_match(pattern: &str, target: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return target == pattern;
    }
    let mut rest = target;
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            if !rest.starts_with(part) {
                return false;
            }
            rest = &rest[part.len()..];
        } else if i == parts.len() - 1 {
            return rest.ends_with(part);
        } else if let Some(pos) = rest.find(part) {
            rest = &rest[pos + part.len()..];
        } else {
            return false;
        }
    }
    true
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
            if let Some(doc) = db::get_document_by_docid_prefix(conn, hex)? {
                docs.push(doc);
            }
        } else if p.contains('*') {
            globs.push(p.trim_start_matches("rqmd://"));
        } else {
            needles.push(p.trim_start_matches("rqmd://"));
        }
    }

    if !needles.is_empty() {
        docs.extend(db::find_documents_by_needles(conn, collections, &needles)?);
    }

    if !globs.is_empty() {
        for doc in db::list_documents_multi(conn, collections)? {
            let filepath = format!("{}/{}", doc.collection, doc.path);
            if globs.iter().any(|g| glob_match(g, &filepath)) {
                docs.push(doc);
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

    #[test]
    fn glob_match_wildcard_and_exact() {
        assert!(glob_match("docs/*.md", "docs/SYNTAX.md"));
        assert!(!glob_match("docs/*.md", "other/SYNTAX.md"));
        assert!(glob_match("*", "anything/at/all.md"));
        assert!(glob_match("exact", "exact"));
        assert!(!glob_match("exact", "not-exact"));
    }
}

//! Per-collection embed-exclusion rules (#147).
//!
//! A collection can keep some of its documents out of `rqmd embed` while they
//! stay in the full-text index: documents whose collection-relative path
//! matches one of its `embed_ignore` globs, and documents whose indexed text
//! (what the embedder would see) is longer than its `embed_max_bytes` cap.
//!
//! The rules are deliberately not part of the embed fingerprint: adding,
//! changing or clearing one never re-embeds anything. With no rule on any
//! collection, every function here returns "nothing excluded" without running
//! a query of its own, and the callers fall back to their unfiltered queries.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use rusqlite::{Connection, params_from_iter};

use crate::types::Collection;

/// Compile gitignore-style glob patterns into a [`GlobSet`], matched against a
/// collection-relative path. Patterns that fail to parse are skipped, so a
/// stored bad pattern never stops a command; the commands that store patterns
/// reject them up front. The walker's `--ignore` set and the embed-exclusion
/// set are both built here, so the two use one syntax.
pub fn build_ignore_set(patterns: &[String]) -> GlobSet {
    let mut builder = GlobSetBuilder::new();
    for pat in patterns {
        if let Ok(g) = Glob::new(pat) {
            builder.add(g);
        }
    }
    builder
        .build()
        .unwrap_or_else(|_| GlobSetBuilder::new().build().unwrap())
}

/// Why a document is kept out of `rqmd embed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExclusionReason {
    /// Its path matches one of the collection's `embed_ignore` globs.
    Rule,
    /// Its indexed text is longer than the collection's `embed_max_bytes`.
    SizeCap,
}

struct CollectionRules {
    globs: GlobSet,
    has_globs: bool,
    max_bytes: Option<u64>,
}

/// The embed-exclusion rules of every collection that has at least one.
#[derive(Default)]
pub struct EmbedExclusions {
    by_collection: HashMap<String, CollectionRules>,
}

impl EmbedExclusions {
    pub fn from_collections(cols: &[Collection]) -> Self {
        let by_collection = cols
            .iter()
            .filter(|c| !c.embed_ignore.is_empty() || c.embed_max_bytes.is_some())
            .map(|c| {
                (
                    c.name.clone(),
                    CollectionRules {
                        globs: build_ignore_set(&c.embed_ignore),
                        has_globs: !c.embed_ignore.is_empty(),
                        max_bytes: c.embed_max_bytes,
                    },
                )
            })
            .collect();
        Self { by_collection }
    }

    /// Read the rules stored with the index's collections.
    pub fn load(conn: &Connection) -> Result<Self> {
        let cols = crate::db::list_collections(conn).context("list collections")?;
        Ok(Self::from_collections(&cols))
    }

    /// True when no collection has a rule or a cap.
    pub fn is_empty(&self) -> bool {
        self.by_collection.is_empty()
    }

    /// Why the document at `path` in `collection`, whose indexed text is
    /// `indexed_bytes` long, is not embedded; `None` when it is. A path rule
    /// is reported before the size cap.
    pub fn reason(
        &self,
        collection: &str,
        path: &str,
        indexed_bytes: u64,
    ) -> Option<ExclusionReason> {
        let rules = self.by_collection.get(collection)?;
        if rules.has_globs && rules.globs.is_match(path) {
            return Some(ExclusionReason::Rule);
        }
        match rules.max_bytes {
            Some(cap) if indexed_bytes > cap => Some(ExclusionReason::SizeCap),
            _ => None,
        }
    }
}

/// The active, non-empty documents the rules keep out of `rqmd embed`.
/// (An empty document is never embedded anyway and is not counted.)
#[derive(Debug, Default)]
pub struct ExcludedDocs {
    pub ids: HashSet<i64>,
    pub by_rule: usize,
    pub by_size: usize,
}

impl ExcludedDocs {
    pub fn total(&self) -> usize {
        self.ids.len()
    }
}

/// Every active document `exclusions` keeps out of `rqmd embed`. Reads only
/// the collections that have a rule, and runs no query when none has.
pub fn excluded_docs(conn: &Connection, exclusions: &EmbedExclusions) -> Result<ExcludedDocs> {
    let mut out = ExcludedDocs::default();
    if exclusions.is_empty() {
        return Ok(out);
    }
    let names: Vec<&String> = exclusions.by_collection.keys().collect();
    let sql = format!(
        "SELECT d.id, d.collection, d.path, octet_length(c.doc) FROM documents d \
         JOIN content c ON c.hash = d.hash \
         WHERE d.active = 1 AND octet_length(c.doc) > 0 AND d.collection IN ({})",
        vec!["?"; names.len()].join(",")
    );
    let mut stmt = conn.prepare(&sql).context("prepare excluded documents")?;
    let rows = stmt
        .query_map(params_from_iter(names), |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("read excluded documents")?;
    for (id, collection, path, bytes) in rows {
        match exclusions.reason(&collection, &path, bytes.max(0) as u64) {
            Some(ExclusionReason::Rule) => out.by_rule += 1,
            Some(ExclusionReason::SizeCap) => out.by_size += 1,
            None => continue,
        }
        out.ids.insert(id);
    }
    Ok(out)
}

/// Distinct content hashes still waiting for `rqmd embed`, leaving out the
/// documents the stored rules exclude. The single count `rqmd status`,
/// `rqmd doctor`, `rqmd update`, the embed fast path and the MCP `status`
/// tool share, so they agree with the set `rqmd embed` walks.
pub fn pending_embed_count(
    conn: &Connection,
    base_fingerprint: &str,
    ast: &[crate::db::AstFingerprint],
) -> Result<i64> {
    let exclusions = EmbedExclusions::load(conn)?;
    let excluded = excluded_docs(conn, &exclusions)?;
    crate::db::count_docs_needing_embed_excluding(conn, base_fingerprint, ast, &excluded.ids)
        .context("count documents needing embedding")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ignore: &[&str], cap: Option<u64>) -> Collection {
        Collection {
            name: name.into(),
            path: format!("/{name}"),
            pattern: "**/*.md".into(),
            ignore: vec![],
            include_by_default: true,
            update_command: None,
            allow_hidden: false,
            respect_gitignore: false,
            max_file_bytes: None,
            embed_ignore: ignore.iter().map(|s| s.to_string()).collect(),
            embed_max_bytes: cap,
        }
    }

    #[test]
    fn no_rule_means_nothing_is_excluded() {
        let rules = EmbedExclusions::from_collections(&[col("a", &[], None)]);
        assert!(rules.is_empty());
        assert_eq!(rules.reason("a", "x.md", u64::MAX), None);
    }

    #[test]
    fn a_rule_applies_only_to_its_own_collection() {
        let rules = EmbedExclusions::from_collections(&[
            col("a", &["generated/**"], None),
            col("b", &[], None),
        ]);
        assert_eq!(
            rules.reason("a", "generated/x.md", 1),
            Some(ExclusionReason::Rule)
        );
        assert_eq!(rules.reason("a", "notes/x.md", 1), None);
        assert_eq!(rules.reason("b", "generated/x.md", 1), None);
    }

    #[test]
    fn the_cap_is_exclusive_and_a_rule_is_reported_first() {
        let rules = EmbedExclusions::from_collections(&[col("a", &["*.log"], Some(10))]);
        assert_eq!(rules.reason("a", "x.md", 10), None);
        assert_eq!(
            rules.reason("a", "x.md", 11),
            Some(ExclusionReason::SizeCap)
        );
        assert_eq!(rules.reason("a", "x.log", 11), Some(ExclusionReason::Rule));
    }

    #[test]
    fn collections_round_trip_through_serde_without_new_keys_by_default() {
        let json = serde_json::to_string(&col("a", &[], None)).unwrap();
        assert!(!json.contains("embed_"), "{json}");
        let back: Collection = serde_json::from_str(&json).unwrap();
        assert!(back.embed_ignore.is_empty() && back.embed_max_bytes.is_none());
    }
}

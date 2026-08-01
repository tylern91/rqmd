//! Unifies the walk → read → frontmatter-parse → title-resolve pipeline that
//! both `collection add` and `index update` need. Before this module existed
//! the two commands carried separate, near-duplicate copies of this logic
//! that had already drifted into different bugs (naive titles, inconsistent
//! glob matching, silently-dropped unreadable files) — this is now the single
//! place both call through, so a fix here fixes both commands at once.

use std::io;
use std::path::{Path, PathBuf};

use globset::GlobSet;
use walkdir::WalkDir;

use crate::exclusions;

/// A file that has been read and had its title/searchable text resolved,
/// ready to hand to `Store::index_document_fts_only_with_raw`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedDoc {
    /// Path relative to the collection root, as stored in `documents.path`.
    pub rel_path: String,
    pub title: String,
    /// Frontmatter stripped, `tags:`/`aliases:` values appended as plain
    /// terms — used for hashing, BM25 indexing, and change detection.
    pub indexed_text: String,
    /// Verbatim file content — stored for retrieval (`rqmd get`).
    pub raw: String,
}

/// Why a candidate file was not indexed. Callers must count these, not
/// swallow them — silently dropping unreadable files without a counter is
/// exactly the "reports success but is wrong" bug class this module exists
/// to close.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    NotUtf8,
    PermissionDenied,
    NotFound,
    Io(io::ErrorKind),
}

impl SkipReason {
    fn from_io_error(e: &io::Error) -> Self {
        match e.kind() {
            // `std::fs::read_to_string` reports non-UTF-8 content as `InvalidData` —
            // there is no dedicated ErrorKind for it in stable std.
            io::ErrorKind::InvalidData => SkipReason::NotUtf8,
            io::ErrorKind::PermissionDenied => SkipReason::PermissionDenied,
            io::ErrorKind::NotFound => SkipReason::NotFound,
            other => SkipReason::Io(other),
        }
    }
}

/// Per-run tally of skip reasons, for honest "Indexed N, skipped M (...)" summaries.
#[derive(Debug, Default, Clone, Copy)]
pub struct SkipCounts {
    pub not_utf8: usize,
    pub permission_denied: usize,
    pub not_found: usize,
    pub other_io: usize,
}

impl SkipCounts {
    pub fn record(&mut self, reason: SkipReason) {
        match reason {
            SkipReason::NotUtf8 => self.not_utf8 += 1,
            SkipReason::PermissionDenied => self.permission_denied += 1,
            SkipReason::NotFound => self.not_found += 1,
            SkipReason::Io(_) => self.other_io += 1,
        }
    }

    pub fn total(&self) -> usize {
        self.not_utf8 + self.permission_denied + self.not_found + self.other_io
    }

    /// Render the non-zero buckets as `"3 unreadable, 2 not UTF-8"` for a summary line.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        let unreadable = self.permission_denied + self.not_found + self.other_io;
        if unreadable > 0 {
            parts.push(format!("{unreadable} unreadable"));
        }
        if self.not_utf8 > 0 {
            parts.push(format!("{} not UTF-8", self.not_utf8));
        }
        parts.join(", ")
    }
}

/// Walk `root`, returning candidate file paths that pass exclusion and
/// include-glob filtering. Does not read file contents — a walk-time failure
/// (permission denied listing a directory, broken symlink) is simply skipped
/// by `WalkDir` itself, while a *read* failure on a file that IS listed is
/// classified per-file by [`prepare`], since those are different failure modes.
pub fn collect_candidates(
    root: &Path,
    include: &GlobSet,
    ignore: &GlobSet,
    allow_hidden: bool,
) -> Vec<PathBuf> {
    WalkDir::new(root)
        .follow_links(true)
        .into_iter()
        .filter_map(|e| e.ok())
        .map(|e| e.into_path())
        .filter(|p| p.is_file())
        .filter(|p| !exclusions::is_excluded(p, root, ignore, allow_hidden))
        .filter(|p| {
            let rel = p.strip_prefix(root).unwrap_or(p);
            include.is_match(rel)
        })
        .collect()
}

/// Read `abs` and resolve its title + indexed/raw text pair.
///
/// Title precedence: frontmatter `title:` scalar → first `#` heading in the
/// body → filename stem. `indexed_text` has the frontmatter block stripped
/// (so it no longer pollutes BM25 body matching) with `tags:`/`aliases:`
/// values appended as plain search terms; `raw` is the verbatim file,
/// unconditionally preserved for storage/retrieval.
pub fn prepare(abs: &Path, root: &Path) -> Result<PreparedDoc, SkipReason> {
    let raw = std::fs::read_to_string(abs).map_err(|e| SkipReason::from_io_error(&e))?;
    let rel_path = abs
        .strip_prefix(root)
        .unwrap_or(abs)
        .to_string_lossy()
        .to_string();

    let (frontmatter, body) = frontmatter::split(&raw);
    let title = frontmatter
        .as_ref()
        .and_then(|fm| fm.title.clone())
        .or_else(|| first_heading(body))
        .unwrap_or_else(|| filename_stem(abs, &rel_path));

    let indexed_text = match &frontmatter {
        Some(fm) if !fm.terms.is_empty() => format!("{body}\n{}", fm.terms.join(" ")),
        _ => body.to_string(),
    };

    Ok(PreparedDoc {
        rel_path,
        title,
        indexed_text,
        raw,
    })
}

/// First ATX heading (`# ...`) in `body`, with leading `#`s and whitespace trimmed.
fn first_heading(body: &str) -> Option<String> {
    body.lines().find_map(|line| {
        let trimmed = line.trim_start();
        if !trimmed.starts_with('#') {
            return None;
        }
        let heading = trimmed.trim_start_matches('#').trim().to_string();
        (!heading.is_empty()).then_some(heading)
    })
}

fn filename_stem(abs: &Path, rel_path: &str) -> String {
    abs.file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| rel_path.to_string())
}

// ── Frontmatter ────────────────────────────────────────────────────────────

/// A hand-rolled scalar scan over `---`-fenced YAML frontmatter — deliberately
/// NOT a full YAML parser. Obsidian frontmatter can contain arbitrary nested
/// structures (multi-line block scalars, nested maps) that a strict parser
/// would reject outright, turning a file that indexes fine today into a hard
/// failure on some unrelated field this code doesn't even care about. This
/// scan only looks for what indexing actually needs (`title`, `tags`,
/// `aliases`) and silently ignores everything else it doesn't recognize,
/// degrading to "no title found" rather than erroring.
mod frontmatter {
    pub struct Frontmatter {
        pub title: Option<String>,
        /// Flattened tag/alias scalar values, in document order.
        pub terms: Vec<String>,
    }

    /// Split `raw` into (frontmatter, remaining body). Returns `(None, raw)`
    /// when `raw` doesn't open with a `---` fence on its own first line, or
    /// the fence is never closed.
    pub fn split(raw: &str) -> (Option<Frontmatter>, &str) {
        if raw.lines().next() != Some("---") {
            return (None, raw);
        }
        let after_open = match raw.find('\n') {
            Some(i) => i + 1,
            None => return (None, raw),
        };

        // Scan line-by-line from just past the opening fence for a closing "---".
        let mut pos = after_open;
        loop {
            let line_end = raw[pos..].find('\n').map(|i| pos + i);
            let line = match line_end {
                Some(end) => &raw[pos..end],
                None => &raw[pos..], // final line, no trailing newline
            };
            if line.trim_end_matches('\r') == "---" {
                let block = &raw[after_open..pos];
                let body_start = match line_end {
                    Some(end) => end + 1,
                    None => raw.len(),
                };
                return (Some(parse_block(block)), &raw[body_start..]);
            }
            match line_end {
                Some(end) => pos = end + 1,
                None => return (None, raw), // reached EOF without a closing fence
            }
        }
    }

    /// Scan the frontmatter block's lines for top-level `key: value` pairs,
    /// tracking `tags:`/`aliases:` block-list continuations (`  - item` lines
    /// with no inline value on the key line itself).
    fn parse_block(block: &str) -> Frontmatter {
        let mut title = None;
        let mut terms = Vec::new();
        let mut list_key: Option<&str> = None;

        for line in block.lines() {
            let trimmed = line.trim_end_matches('\r');
            if trimmed.trim().is_empty() {
                continue;
            }

            // An indented "- item" line continues the previous list key, if any;
            // any other indented line is a continuation we don't understand and
            // simply skip, without resetting list_key (a blank scalar continuation
            // shouldn't end an in-progress list).
            if trimmed.starts_with(' ') || trimmed.starts_with('-') {
                if let Some(key) = list_key {
                    let t = trimmed.trim();
                    if let Some(item) = t.strip_prefix('-') {
                        let value = unquote(item.trim());
                        if !value.is_empty() && (key == "tags" || key == "aliases") {
                            terms.push(value);
                        }
                    }
                }
                continue;
            }

            let Some((key, value)) = trimmed.split_once(':') else {
                list_key = None;
                continue;
            };
            let key = key.trim();
            let value = value.trim();

            if value.is_empty() {
                // Opens a block list on the following indented lines.
                list_key = Some(key);
                continue;
            }
            list_key = None;

            match key {
                "title" if title.is_none() => {
                    let v = unquote(value);
                    if !v.is_empty() {
                        title = Some(v);
                    }
                }
                "tags" | "aliases" => {
                    for item in flow_list_items(value) {
                        let v = unquote(item);
                        if !v.is_empty() {
                            terms.push(v);
                        }
                    }
                }
                _ => {}
            }
        }

        Frontmatter { title, terms }
    }

    /// Split a `[a, b, c]` flow list into its items, or treat the whole value
    /// as a single scalar item when it isn't bracketed (`tags: solo-tag`).
    fn flow_list_items(value: &str) -> Vec<&str> {
        match value.strip_prefix('[').and_then(|v| v.strip_suffix(']')) {
            Some(inner) => inner
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .collect(),
            None => vec![value],
        }
    }

    /// Strip a single layer of matching `'...'`/`"..."` quoting, if present.
    fn unquote(s: &str) -> String {
        let bytes = s.as_bytes();
        if bytes.len() >= 2 {
            let (first, last) = (bytes[0], bytes[bytes.len() - 1]);
            if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
                return s[1..s.len() - 1].to_string();
            }
        }
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        path
    }

    // ── Frontmatter round-trip ──────────────────────────────────────────────

    #[test]
    fn title_from_frontmatter_scalar() {
        let dir = tempdir().unwrap();
        let path = write(
            dir.path(),
            "note.md",
            "---\ntitle: My Real Title\ntags: [a, b]\n---\n# Heading\nBody text.\n",
        );
        let doc = prepare(&path, dir.path()).unwrap();
        assert_eq!(doc.title, "My Real Title");
    }

    #[test]
    fn title_falls_back_to_first_heading() {
        let dir = tempdir().unwrap();
        let path = write(
            dir.path(),
            "note.md",
            "---\ntags: [a]\n---\n# Heading Title\nBody text.\n",
        );
        let doc = prepare(&path, dir.path()).unwrap();
        assert_eq!(doc.title, "Heading Title");
    }

    #[test]
    fn title_falls_back_to_filename_stem() {
        let dir = tempdir().unwrap();
        let path = write(
            dir.path(),
            "my-note.md",
            "---\ntags: [a]\n---\nJust body text.\n",
        );
        let doc = prepare(&path, dir.path()).unwrap();
        assert_eq!(doc.title, "my-note");
    }

    #[test]
    fn no_frontmatter_falls_back_to_heading_then_stem() {
        let dir = tempdir().unwrap();
        let path = write(dir.path(), "plain.md", "# Plain Heading\nBody.\n");
        let doc = prepare(&path, dir.path()).unwrap();
        assert_eq!(doc.title, "Plain Heading");

        let path2 = write(dir.path(), "no-heading.md", "Just some text, no heading.\n");
        let doc2 = prepare(&path2, dir.path()).unwrap();
        assert_eq!(doc2.title, "no-heading");
    }

    #[test]
    fn indexed_text_strips_frontmatter_and_appends_tags_and_aliases() {
        let dir = tempdir().unwrap();
        let path = write(
            dir.path(),
            "note.md",
            "---\ntitle: T\ntags: [alpha, beta]\naliases:\n  - Old Name\n---\nBody content here.\n",
        );
        let doc = prepare(&path, dir.path()).unwrap();
        assert!(!doc.indexed_text.contains("---"));
        assert!(!doc.indexed_text.contains("title: T"));
        assert!(doc.indexed_text.contains("Body content here."));
        assert!(doc.indexed_text.contains("alpha"));
        assert!(doc.indexed_text.contains("beta"));
        assert!(doc.indexed_text.contains("Old Name"));
        // Raw content preserves the frontmatter verbatim for retrieval.
        assert!(doc.raw.contains("title: T"));
    }

    #[test]
    fn body_with_no_frontmatter_fence_is_untouched() {
        let dir = tempdir().unwrap();
        let path = write(dir.path(), "plain.md", "No frontmatter here.\n");
        let doc = prepare(&path, dir.path()).unwrap();
        assert_eq!(doc.indexed_text, "No frontmatter here.\n");
        assert_eq!(doc.raw, doc.indexed_text);
    }

    #[test]
    fn unterminated_fence_degrades_to_whole_file_as_body() {
        let dir = tempdir().unwrap();
        let path = write(
            dir.path(),
            "broken.md",
            "---\ntitle: Unterminated\nno closing fence\n",
        );
        let doc = prepare(&path, dir.path()).unwrap();
        // Degrades gracefully: no crash, whole file treated as body, filename-stem title.
        assert_eq!(doc.title, "broken");
        assert!(doc.indexed_text.contains("Unterminated"));
    }

    // ── Content-hash stability ──────────────────────────────────────────────

    #[test]
    fn frontmatter_only_edit_does_not_change_indexed_text_hash() {
        let dir = tempdir().unwrap();
        let v1 = write(
            dir.path(),
            "a.md",
            "---\ntitle: T\nupdated: 2026-01-01\n---\nSame body.\n",
        );
        let doc1 = prepare(&v1, dir.path()).unwrap();
        let hash1 = rqmd_core::db::content_hash(&doc1.indexed_text);

        let v2 = write(
            dir.path(),
            "a.md",
            "---\ntitle: T\nupdated: 2026-06-15\n---\nSame body.\n",
        );
        let doc2 = prepare(&v2, dir.path()).unwrap();
        let hash2 = rqmd_core::db::content_hash(&doc2.indexed_text);

        assert_eq!(
            hash1, hash2,
            "frontmatter-only edit must not change the hash"
        );
    }

    #[test]
    fn body_edit_changes_indexed_text_hash() {
        let dir = tempdir().unwrap();
        let v1 = write(dir.path(), "a.md", "---\ntitle: T\n---\nOriginal body.\n");
        let doc1 = prepare(&v1, dir.path()).unwrap();
        let hash1 = rqmd_core::db::content_hash(&doc1.indexed_text);

        let v2 = write(dir.path(), "a.md", "---\ntitle: T\n---\nChanged body.\n");
        let doc2 = prepare(&v2, dir.path()).unwrap();
        let hash2 = rqmd_core::db::content_hash(&doc2.indexed_text);

        assert_ne!(hash1, hash2, "a real body edit must change the hash");
    }

    // ── Skip-reason classification ───────────────────────────────────────────

    #[test]
    fn skip_reason_classifies_not_utf8() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("binary.md");
        std::fs::write(&path, [0xFF, 0xFE, 0x00, 0xFF]).unwrap();
        let err = prepare(&path, dir.path()).unwrap_err();
        assert_eq!(err, SkipReason::NotUtf8);
    }

    #[test]
    fn skip_reason_classifies_permission_denied() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let path = dir.path().join("secret.md");
        std::fs::write(&path, "body").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();

        let result = prepare(&path, dir.path());
        // Root (and some sandboxes) ignore permission bits entirely — only assert
        // the classification when the OS actually enforced them, so this test
        // isn't flaky when run as root.
        if let Err(reason) = result {
            assert_eq!(reason, SkipReason::PermissionDenied);
        }
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644));
    }

    #[test]
    fn skip_counts_describe_breaks_down_by_reason() {
        let mut counts = SkipCounts::default();
        counts.record(SkipReason::NotUtf8);
        counts.record(SkipReason::NotUtf8);
        counts.record(SkipReason::PermissionDenied);
        assert_eq!(counts.total(), 3);
        assert_eq!(counts.describe(), "1 unreadable, 2 not UTF-8");
    }

    // ── collect_candidates: glob + exclusion integration ─────────────────────

    #[test]
    fn collect_candidates_respects_include_and_ignore_sets() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("docs")).unwrap();
        write(dir.path(), "a.md", "a");
        write(dir.path().join("docs").as_path(), "e.md", "e");
        write(dir.path(), "b.txt", "b");

        let include = exclusions::build_include_set("**/*.md").unwrap();
        let ignore = exclusions::build_ignore_set(&["docs/**".to_string()]);
        let found = collect_candidates(dir.path(), &include, &ignore, false);
        let names: Vec<String> = found
            .iter()
            .map(|p| {
                p.strip_prefix(dir.path())
                    .unwrap()
                    .to_string_lossy()
                    .to_string()
            })
            .collect();
        assert_eq!(names, vec!["a.md"]);
    }
}

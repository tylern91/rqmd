//! Unifies the walk → read → frontmatter-parse → title-resolve pipeline that
//! both `collection add` and `index update` need. Before this module existed
//! the two commands carried separate, near-duplicate copies of this logic
//! that had already drifted into different bugs (naive titles, inconsistent
//! glob matching, silently-dropped unreadable files) — this is now the single
//! place both call through, so a fix here fixes both commands at once.

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

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
    /// Files over the collection's `max_file_bytes`, left out by the walk.
    pub too_large: usize,
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
        self.not_utf8 + self.permission_denied + self.not_found + self.other_io + self.too_large
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
        if self.too_large > 0 {
            parts.push(format!("{} over the size limit", self.too_large));
        }
        parts.join(", ")
    }
}

/// Largest file `update` will read when a collection sets no `max_file_bytes`.
pub const DEFAULT_MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;

/// How a collection's tree is walked.
#[derive(Debug, Clone, Copy)]
pub struct WalkOptions {
    pub allow_hidden: bool,
    pub respect_gitignore: bool,
    pub max_file_bytes: u64,
}

impl Default for WalkOptions {
    fn default() -> Self {
        Self {
            allow_hidden: false,
            respect_gitignore: false,
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
        }
    }
}

impl WalkOptions {
    pub fn for_collection(col: &rqmd_core::Collection) -> Self {
        Self {
            allow_hidden: col.allow_hidden,
            respect_gitignore: col.respect_gitignore,
            max_file_bytes: col.max_file_bytes.unwrap_or(DEFAULT_MAX_FILE_BYTES),
        }
    }
}

/// Result of a walk: the files to index, how many otherwise-matching files
/// were left out for exceeding `max_file_bytes`, and the folders the built-in
/// rule left out.
#[derive(Debug, Default)]
pub struct Candidates {
    pub files: Vec<PathBuf>,
    pub too_large: usize,
    /// Built-in excluded folders the walk did not descend into, relative to the
    /// root and sorted. A folder inside one of them is not listed separately.
    pub excluded_dirs: Vec<String>,
}

/// Longest list of left-out folders a summary line names.
const REPORTED_DIRS: usize = 5;

/// `"left out 2 folder(s) by the built-in rule (build/, node_modules/)"`, or
/// `None` when nothing was left out. Names past [`REPORTED_DIRS`] are counted,
/// and control characters in a directory name are shown as `?` so a name cannot
/// write escape sequences into the terminal.
pub fn excluded_dirs_clause(dirs: &[String]) -> Option<String> {
    if dirs.is_empty() {
        return None;
    }
    let names: Vec<String> = dirs
        .iter()
        .take(REPORTED_DIRS)
        .map(|d| {
            let clean: String = d
                .chars()
                .map(|c| if c.is_control() { '?' } else { c })
                .collect();
            format!("{clean}/")
        })
        .collect();
    let mut list = names.join(", ");
    if dirs.len() > REPORTED_DIRS {
        list.push_str(&format!(" +{} more", dirs.len() - REPORTED_DIRS));
    }
    Some(format!(
        "left out {} folder(s) by the built-in rule ({list})",
        dirs.len()
    ))
}

/// Built-in excluded folders the walk skipped, shared with the walker's filter.
type PrunedDirs = Arc<Mutex<Vec<String>>>;

/// Walk `root`, returning candidate file paths that pass exclusion and
/// include-glob filtering. Does not read file contents — a walk-time failure
/// (permission denied listing a directory, broken symlink) is simply skipped
/// by `WalkDir` itself, while a *read* failure on a file that IS listed is
/// classified per-file by [`prepare`], since those are different failure modes.
///
/// `follow_links(true)` lets a symlink *inside* the tree resolve to content
/// elsewhere on disk — useful for e.g. a shared notes directory linked into
/// several collections. Without a containment check that also lets a
/// symlink escape the collection root entirely (`ln -s /etc/passwd leak.md`),
/// so every candidate's fully-resolved real path is required to stay under
/// `root`'s real path before it is indexed (github.com/tylern91/rqmd#86, AC-3).
pub fn collect_candidates(
    root: &Path,
    include: &GlobSet,
    ignore: &GlobSet,
    opts: WalkOptions,
) -> Candidates {
    let canonical_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut found = Candidates::default();
    let pruned = PrunedDirs::default();
    for p in walk_paths(root, opts, &pruned) {
        if !p.is_file()
            || !is_contained_in(&p, &canonical_root)
            || exclusions::is_excluded(&p, root, ignore, opts.allow_hidden)
        {
            continue;
        }
        let rel = p.strip_prefix(root).unwrap_or(&p);
        if !include.is_match(rel) {
            continue;
        }
        // Counted only for files that would otherwise have been indexed, so the
        // summary reports what the size cap cost, not every large file on disk.
        if std::fs::metadata(&p).is_ok_and(|m| m.len() > opts.max_file_bytes) {
            found.too_large += 1;
            continue;
        }
        found.files.push(p);
    }
    found.excluded_dirs = pruned.lock().map(|d| d.clone()).unwrap_or_default();
    found.excluded_dirs.sort();
    found.excluded_dirs.dedup();
    found
}

/// Every path under `root`, following symlinks. With `respect_gitignore`, paths
/// matched by a `.gitignore` inside the tree are left out (and only those
/// files are consulted: no global ignore, no `.ignore`, no hidden-file rule —
/// hidden handling stays in [`exclusions::is_excluded`]).
///
/// A directory that [`exclusions::prunes_dir`] rejects is not descended into
/// (the root itself never is rejected): every path beneath it would fail
/// `is_excluded` anyway, so the candidates are the same and only the walk is
/// shorter. Built-in excluded ones are recorded in `pruned`.
fn walk_paths(
    root: &Path,
    opts: WalkOptions,
    pruned: &PrunedDirs,
) -> Box<dyn Iterator<Item = PathBuf>> {
    let pruner = Arc::new(Pruner {
        root: root.to_path_buf(),
        allow_hidden: opts.allow_hidden,
        found: Arc::clone(pruned),
    });
    if opts.respect_gitignore {
        Box::new(
            ignore::WalkBuilder::new(root)
                .standard_filters(false)
                .git_ignore(true)
                .require_git(false)
                .follow_links(true)
                .filter_entry(move |e| {
                    let is_dir = e.file_type().is_some_and(|t| t.is_dir());
                    pruner.keep(e.depth(), is_dir, e.file_name(), e.path())
                })
                .build()
                .filter_map(|e| e.ok())
                .map(|e| e.into_path()),
        )
    } else {
        Box::new(
            WalkDir::new(root)
                .follow_links(true)
                .into_iter()
                .filter_entry(move |e| {
                    pruner.keep(e.depth(), e.file_type().is_dir(), e.file_name(), e.path())
                })
                .filter_map(|e| e.ok())
                .map(|e| e.into_path()),
        )
    }
}

/// The walker's directory filter.
struct Pruner {
    root: PathBuf,
    allow_hidden: bool,
    found: PrunedDirs,
}

impl Pruner {
    /// False for a directory the walk should not descend into.
    fn keep(&self, depth: usize, is_dir: bool, name: &OsStr, path: &Path) -> bool {
        if depth == 0 || !is_dir || !exclusions::prunes_dir(name, self.allow_hidden) {
            return true;
        }
        if name
            .to_str()
            .is_some_and(exclusions::is_builtin_excluded_dir)
            && let Ok(mut found) = self.found.lock()
        {
            found.push(rel_path_of(path, &self.root));
        }
        false
    }
}

/// True if `p`'s fully-resolved (symlink-followed) real path is under
/// `canonical_root`. A path that fails to canonicalize (dangling symlink,
/// permission error mid-resolution) is treated as not contained rather than
/// indexed on a guess.
fn is_contained_in(p: &Path, canonical_root: &Path) -> bool {
    std::fs::canonicalize(p)
        .map(|real| real.starts_with(canonical_root))
        .unwrap_or(false)
}

/// A file's `(size, mtime_ns)`, the cheap change signal checked before a file
/// is read and hashed. The content hash stays the source of truth: a stat
/// match only ever lets `update` skip work, a mismatch always falls back to it.
pub type FileStat = (i64, i64);

/// A file modified within this window of "now" is not trusted as unchanged:
/// filesystems with coarse mtime granularity can record a second write in the
/// same tick with an identical size and timestamp.
const MTIME_TRUST_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);

/// Stat `abs` (following symlinks, like the read that follows it). Take this
/// *before* reading the file: a write landing between the stat and the read
/// then leaves a stale stat next to newer content, which the next run
/// re-indexes, whereas stat-after-read could pin old content to a new stat.
pub fn stat_file(abs: &Path) -> Option<FileStat> {
    let meta = std::fs::metadata(abs).ok()?;
    let mtime = meta.modified().ok()?;
    let nanos = mtime.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos();
    Some((i64::try_from(meta.len()).ok()?, i64::try_from(nanos).ok()?))
}

/// The stat to persist for a file just indexed: `None` while its mtime is too
/// recent to trust (see [`MTIME_TRUST_WINDOW`]), so the next run re-hashes it.
pub fn trusted_stat(stat: FileStat) -> Option<FileStat> {
    let mtime =
        std::time::UNIX_EPOCH + std::time::Duration::from_nanos(u64::try_from(stat.1).ok()?);
    let age = std::time::SystemTime::now().duration_since(mtime).ok()?;
    (age > MTIME_TRUST_WINDOW).then_some(stat)
}

/// Path of `abs` relative to the collection `root`, as stored in `documents.path`.
pub fn rel_path_of(abs: &Path, root: &Path) -> String {
    abs.strip_prefix(root)
        .unwrap_or(abs)
        .to_string_lossy()
        .to_string()
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
    let rel_path = rel_path_of(abs, root);

    let (frontmatter, body) = rqmd_core::frontmatter::split(&raw);
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

#[cfg(test)]
mod tests {
    fn stat_with_mtime(mtime: std::time::SystemTime) -> FileStat {
        let nanos = mtime
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        (7, i64::try_from(nanos).unwrap())
    }

    #[test]
    fn trusted_stat_rejects_recent_and_future_mtimes_but_keeps_old_ones() {
        let now = std::time::SystemTime::now();
        let hour = std::time::Duration::from_secs(3600);

        let old = stat_with_mtime(now - hour);
        assert_eq!(trusted_stat(old), Some(old));
        assert_eq!(trusted_stat(stat_with_mtime(now)), None);
        assert_eq!(trusted_stat(stat_with_mtime(now + hour)), None);
    }

    use super::*;
    use std::collections::BTreeSet;
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
        let found = collect_candidates(dir.path(), &include, &ignore, WalkOptions::default()).files;
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

    /// AC-3 (github.com/tylern91/rqmd#86): a symlink whose real target
    /// resolves outside the collection root must not be walked in — even
    /// though `follow_links(true)` makes `WalkDir` list it as a regular file.
    #[test]
    #[cfg(unix)]
    fn collect_candidates_excludes_a_symlink_escaping_the_root() {
        let outside = tempdir().unwrap();
        write(outside.path(), "secret.md", "outside the collection");

        let root = tempdir().unwrap();
        write(root.path(), "a.md", "inside the collection");
        std::os::unix::fs::symlink(
            outside.path().join("secret.md"),
            root.path().join("leak.md"),
        )
        .unwrap();

        let include = exclusions::build_include_set("**/*.md").unwrap();
        let ignore = exclusions::build_ignore_set(&[]);
        let found =
            collect_candidates(root.path(), &include, &ignore, WalkOptions::default()).files;
        let names: Vec<String> = found
            .iter()
            .map(|p| {
                p.strip_prefix(root.path())
                    .unwrap()
                    .to_string_lossy()
                    .to_string()
            })
            .collect();
        assert_eq!(names, vec!["a.md"], "leak.md must be excluded: {names:?}");
    }

    /// A symlink that stays inside the collection root (e.g. a shared file
    /// linked from elsewhere in the same tree) must still be walked in — the
    /// containment check is about escaping `root`, not symlinks per se.
    #[test]
    #[cfg(unix)]
    fn collect_candidates_includes_a_symlink_contained_within_the_root() {
        let root = tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("real")).unwrap();
        write(root.path().join("real").as_path(), "target.md", "shared");
        std::os::unix::fs::symlink(
            root.path().join("real").join("target.md"),
            root.path().join("linked.md"),
        )
        .unwrap();

        let include = exclusions::build_include_set("**/*.md").unwrap();
        let ignore = exclusions::build_ignore_set(&[]);
        let found =
            collect_candidates(root.path(), &include, &ignore, WalkOptions::default()).files;
        let names: Vec<String> = found
            .iter()
            .map(|p| {
                p.strip_prefix(root.path())
                    .unwrap()
                    .to_string_lossy()
                    .to_string()
            })
            .collect();
        assert!(
            names.contains(&"linked.md".to_string()),
            "in-tree symlink must still be indexed: {names:?}"
        );
    }

    // ── Directory pruning ────────────────────────────────────────────────────

    fn rel_names(paths: impl IntoIterator<Item = PathBuf>, root: &Path) -> BTreeSet<String> {
        paths.into_iter().map(|p| rel_path_of(&p, root)).collect()
    }

    /// A tree that exercises every way a folder can be left out: built-in
    /// names at depth 1 and deeper, nested inside each other, a hidden folder,
    /// a symlinked folder carrying a built-in name, a symlink to a folder that
    /// itself holds one, a file that only looks like a built-in name, a
    /// `.gitignore`d folder and an ignore-glob folder.
    fn mixed_tree() -> tempfile::TempDir {
        let dir = tempdir().unwrap();
        let r = dir.path();
        for d in [
            "notes",
            "node_modules/pkg/build",
            "docs/build/deep/target",
            "a/b/vendor",
            ".obsidian/plugins",
            ".cache/x",
            "real/dist",
            "generated",
            "private",
        ] {
            std::fs::create_dir_all(r.join(d)).unwrap();
        }
        for f in [
            "root.md",
            "notes/n.md",
            "node_modules/pkg/readme.md",
            "node_modules/pkg/build/b.md",
            "docs/d.md",
            "docs/build/deep/target/t.md",
            "docs/build/y.md",
            "a/b/vendor/v.md",
            "a/b/ok.md",
            ".obsidian/plugins/p.md",
            ".obsidian/o.md",
            ".cache/x/c.md",
            ".hidden.md",
            "real/dist/z.md",
            "real/r.md",
            "build.md",
            "generated/g.md",
            "private/p.md",
        ] {
            write(r, f, "body");
        }
        std::fs::write(r.join(".gitignore"), "generated/\n").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(r.join("real"), r.join("linked")).unwrap();
            std::os::unix::fs::symlink(r.join("notes"), r.join("build")).unwrap();
            std::os::unix::fs::symlink(r.join("notes"), r.join("target")).unwrap();
        }
        dir
    }

    /// The walk as it was before directories were pruned: every path under the
    /// root, filtered file by file.
    fn candidates_without_pruning(
        root: &Path,
        include: &GlobSet,
        ignore: &GlobSet,
        opts: WalkOptions,
    ) -> BTreeSet<String> {
        let canonical_root = std::fs::canonicalize(root).unwrap();
        let all: Vec<PathBuf> = if opts.respect_gitignore {
            ignore::WalkBuilder::new(root)
                .standard_filters(false)
                .git_ignore(true)
                .require_git(false)
                .follow_links(true)
                .build()
                .filter_map(|e| e.ok())
                .map(|e| e.into_path())
                .collect()
        } else {
            WalkDir::new(root)
                .follow_links(true)
                .into_iter()
                .filter_map(|e| e.ok())
                .map(|e| e.into_path())
                .collect()
        };
        let kept = all.into_iter().filter(|p| {
            p.is_file()
                && is_contained_in(p, &canonical_root)
                && !exclusions::is_excluded(p, root, ignore, opts.allow_hidden)
                && include.is_match(p.strip_prefix(root).unwrap_or(p))
        });
        rel_names(kept, root)
    }

    #[test]
    fn pruning_leaves_the_candidate_set_unchanged() {
        let dir = mixed_tree();
        let include = exclusions::build_include_set("**/*.md").unwrap();
        for ignore_patterns in [vec![], vec!["private/**".to_string()]] {
            let ignore = exclusions::build_ignore_set(&ignore_patterns);
            for allow_hidden in [false, true] {
                for respect_gitignore in [false, true] {
                    let opts = WalkOptions {
                        allow_hidden,
                        respect_gitignore,
                        ..WalkOptions::default()
                    };
                    let expected = candidates_without_pruning(dir.path(), &include, &ignore, opts);
                    let got = rel_names(
                        collect_candidates(dir.path(), &include, &ignore, opts).files,
                        dir.path(),
                    );
                    assert!(!expected.is_empty());
                    assert_eq!(
                        got, expected,
                        "hidden={allow_hidden} gitignore={respect_gitignore} ignore={ignore_patterns:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_walk_does_not_descend_into_left_out_folders() {
        let dir = mixed_tree();
        for respect_gitignore in [false, true] {
            for allow_hidden in [false, true] {
                let opts = WalkOptions {
                    allow_hidden,
                    respect_gitignore,
                    ..WalkOptions::default()
                };
                let walked = rel_names(
                    walk_paths(dir.path(), opts, &PrunedDirs::default()),
                    dir.path(),
                );
                for under in [
                    "node_modules/",
                    "docs/build/",
                    "a/b/vendor/",
                    ".cache/",
                    "real/dist/",
                ] {
                    assert!(
                        !walked.iter().any(|p| p.starts_with(under)),
                        "walked into {under} (hidden={allow_hidden}, gitignore={respect_gitignore}): {walked:?}"
                    );
                }
                assert!(walked.contains("notes/n.md") && walked.contains("a/b/ok.md"));
                assert_eq!(
                    walked.iter().any(|p| p.starts_with(".obsidian/")),
                    allow_hidden,
                    "hidden folders follow --hidden"
                );
            }
        }
    }

    #[test]
    fn built_in_folders_are_reported_by_relative_path_without_their_contents() {
        let dir = mixed_tree();
        let include = exclusions::build_include_set("**/*.md").unwrap();
        let ignore = exclusions::build_ignore_set(&[]);
        let found = collect_candidates(dir.path(), &include, &ignore, WalkOptions::default());
        // `.cache` is a built-in name too. Hidden-only folders are not reported,
        // and neither are folders inside an already left-out folder.
        let mut expected = vec![
            ".cache",
            "a/b/vendor",
            "docs/build",
            "node_modules",
            "real/dist",
            "linked/dist",
        ];
        #[cfg(unix)]
        expected.extend(["build", "target"]);
        expected.sort();
        assert_eq!(found.excluded_dirs, expected);
    }

    #[test]
    fn the_collection_root_is_never_reported_or_pruned() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("build");
        std::fs::create_dir_all(root.join("notes")).unwrap();
        write(&root, "notes/n.md", "body");
        let include = exclusions::build_include_set("**/*.md").unwrap();
        let ignore = exclusions::build_ignore_set(&[]);
        let found = collect_candidates(&root, &include, &ignore, WalkOptions::default());
        assert_eq!(found.files.len(), 1);
        assert!(found.excluded_dirs.is_empty());
    }

    #[test]
    fn excluded_dirs_clause_caps_the_names_and_strips_control_characters() {
        assert_eq!(excluded_dirs_clause(&[]), None);
        let one = vec!["build".to_string()];
        assert_eq!(
            excluded_dirs_clause(&one).unwrap(),
            "left out 1 folder(s) by the built-in rule (build/)"
        );
        let many: Vec<String> = (0..8).map(|i| format!("p{i}/build")).collect();
        assert_eq!(
            excluded_dirs_clause(&many).unwrap(),
            "left out 8 folder(s) by the built-in rule (p0/build/, p1/build/, p2/build/, p3/build/, p4/build/ +3 more)"
        );
        let hostile = vec!["x\u{1b}[2Jy/build".to_string()];
        let clause = excluded_dirs_clause(&hostile).unwrap();
        assert!(!clause.contains('\u{1b}'), "{clause:?}");
    }
}

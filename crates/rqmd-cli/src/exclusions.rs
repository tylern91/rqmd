use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::path::Path;

// Directories that are never worth indexing — checked by path component.
const BUILTIN_EXCLUDED_DIRS: &[&str] = &[
    "node_modules",
    "vendor",
    "dist",
    "build",
    "target",
    ".cache",
];

/// Compile a list of gitignore-style glob patterns into a [GlobSet].
/// Patterns that fail to parse are silently skipped — appropriate for
/// *ignore* patterns (an unmatchable extra exclusion is harmless), but this
/// leniency must NOT be reused for *include* patterns; see [`build_include_set`].
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

/// Compile a collection's `pattern` mask into a [GlobSet] of alternatives.
///
/// Supports two ways of expressing "match any of several globs":
/// - globset's own brace syntax within a single pattern (`**/*.{md,mdx,txt}`)
/// - top-level comma-separated alternatives (`**/*.md,**/*.txt`)
///
/// The split on `,` is brace-depth-aware so it never breaks the first form —
/// commas inside `{...}` are left alone, only commas outside any brace are
/// treated as pattern separators.
///
/// Unlike [`build_ignore_set`], a malformed glob here is a loud error: this
/// set decides what gets indexed at all, so silently dropping an alternative
/// (as the old per-command `mask_to_extension` string-splitting effectively
/// did for anything beyond a single extension) would mean documents vanish
/// from the index with no diagnostic.
pub fn build_include_set(pattern: &str) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for part in split_top_level_commas(pattern) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let glob = Glob::new(part).with_context(|| {
            format!("invalid glob pattern in mask: '{part}' (from '{pattern}')")
        })?;
        builder.add(glob);
    }
    builder
        .build()
        .with_context(|| format!("failed to compile mask '{pattern}'"))
}

/// Split `pattern` on top-level commas — commas that appear outside any `{...}`
/// brace-alternation group. `**/*.{md,mdx,txt}` stays whole (one glob using
/// globset's native brace syntax); `**/*.md,**/*.txt` splits into two.
fn split_top_level_commas(pattern: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    for (i, c) in pattern.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(&pattern[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&pattern[start..]);
    parts
}

/// Returns `true` if `path` should be excluded from indexing.
///
/// Exclusions applied (in order), all evaluated against `path`'s components
/// *relative to `root`* — an absolute collection root (e.g. `~/.dotfiles`) must
/// never itself trigger the hidden-dot-directory rule below:
/// 1. Unless `allow_hidden` is set, any relative path component that starts
///    with `.` (hidden files/dirs).
/// 2. Any relative path component that matches a built-in excluded directory name.
/// 3. The relative path matches any user-provided glob in `ignore`.
pub fn is_excluded(path: &Path, root: &Path, ignore: &GlobSet, allow_hidden: bool) -> bool {
    let rel = path.strip_prefix(root).unwrap_or(path);
    for component in rel.components() {
        if let std::path::Component::Normal(name) = component {
            let s = match name.to_str() {
                Some(s) => s,
                None => return true, // non-UTF-8 — exclude; can't index as text anyway
            };
            if !allow_hidden && s.starts_with('.') {
                return true;
            }
            if BUILTIN_EXCLUDED_DIRS.contains(&s) {
                return true;
            }
        }
    }
    ignore.is_match(rel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn empty_ignore() -> GlobSet {
        build_ignore_set(&[])
    }

    // ── P0 regression: dot-directory root must not exclude everything ─────────

    #[test]
    fn dot_directory_root_indexes_normal_files_by_default() {
        let dir = tempdir().unwrap();
        let root = dir.path().join(".dotfiles");
        fs::create_dir_all(&root).unwrap();
        let file = root.join("notes.md");
        fs::write(&file, "body").unwrap();

        let ignore = empty_ignore();
        assert!(
            !is_excluded(&file, &root, &ignore, false),
            "a file directly under a dot-named root must index with zero flags"
        );
    }

    #[test]
    fn dot_directory_inside_tree_excluded_by_default_included_with_hidden() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("vault");
        let hidden_dir = root.join(".obsidian");
        fs::create_dir_all(&hidden_dir).unwrap();
        let file = hidden_dir.join("config.md");
        fs::write(&file, "body").unwrap();

        let ignore = empty_ignore();
        assert!(
            is_excluded(&file, &root, &ignore, false),
            "a dot-directory nested inside the tree must be excluded by default"
        );
        assert!(
            !is_excluded(&file, &root, &ignore, true),
            "--hidden must allow a dot-directory nested inside the tree"
        );
    }

    // ── build_include_set: five glob-pattern cases ─────────────────────────────

    fn make_tree(root: &Path) {
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("a.md"), "a").unwrap();
        fs::write(root.join("b.mdx"), "b").unwrap();
        fs::write(root.join("c.txt"), "c").unwrap();
        fs::write(root.join("d.png"), "d").unwrap();
        fs::write(root.join("docs/e.md"), "e").unwrap();
        fs::write(root.join("Makefile"), "m").unwrap();
        fs::write(root.join("sub/Makefile"), "m").unwrap();
    }

    fn matches(root: &Path, pattern: &str) -> Vec<String> {
        let set = build_include_set(pattern).unwrap();
        let mut found: Vec<String> = walkdir::WalkDir::new(root)
            .into_iter()
            .filter_map(|e| e.ok())
            .map(|e| e.into_path())
            .filter(|p| p.is_file())
            .filter(|p| {
                let rel = p.strip_prefix(root).unwrap_or(p);
                set.is_match(rel)
            })
            .map(|p| p.strip_prefix(root).unwrap().to_string_lossy().to_string())
            .collect();
        found.sort();
        found
    }

    #[test]
    fn glob_single_extension_works() {
        let dir = tempdir().unwrap();
        make_tree(dir.path());
        assert_eq!(matches(dir.path(), "**/*.md"), vec!["a.md", "docs/e.md"]);
    }

    #[test]
    fn glob_brace_alternation_matches_all_listed_extensions() {
        let dir = tempdir().unwrap();
        make_tree(dir.path());
        assert_eq!(
            matches(dir.path(), "**/*.{md,mdx,txt}"),
            vec!["a.md", "b.mdx", "c.txt", "docs/e.md"]
        );
    }

    #[test]
    fn glob_comma_separated_patterns_both_match() {
        let dir = tempdir().unwrap();
        make_tree(dir.path());
        assert_eq!(
            matches(dir.path(), "**/*.md,**/*.txt"),
            vec!["a.md", "c.txt", "docs/e.md"]
        );
    }

    #[test]
    fn glob_prefixed_pattern_only_matches_under_prefix() {
        let dir = tempdir().unwrap();
        make_tree(dir.path());
        assert_eq!(matches(dir.path(), "docs/**/*.md"), vec!["docs/e.md"]);
    }

    #[test]
    fn glob_extensionless_filename_matches_everywhere() {
        let dir = tempdir().unwrap();
        make_tree(dir.path());
        assert_eq!(
            matches(dir.path(), "**/Makefile"),
            vec!["Makefile", "sub/Makefile"]
        );
    }

    #[test]
    fn build_include_set_bails_on_malformed_glob() {
        // An unbalanced brace is invalid glob syntax — must error, not silently
        // compile to an empty (match-nothing) set.
        assert!(build_include_set("**/*.{md,mdx").is_err());
    }
}

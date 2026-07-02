use globset::{GlobSet, GlobSetBuilder};
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
/// Patterns that fail to parse are silently skipped.
pub fn build_ignore_set(patterns: &[String]) -> GlobSet {
    let mut builder = GlobSetBuilder::new();
    for pat in patterns {
        if let Ok(g) = globset::Glob::new(pat) {
            builder.add(g);
        }
    }
    builder
        .build()
        .unwrap_or_else(|_| GlobSetBuilder::new().build().unwrap())
}

/// Returns `true` if `path` should be excluded from indexing.
///
/// Exclusions applied (in order):
/// 1. Any path component that starts with `.` (hidden files/dirs).
/// 2. Any path component that matches a built-in excluded directory name.
/// 3. The relative path matches any user-provided glob in `ignore`.
pub fn is_excluded(path: &Path, root: &Path, ignore: &GlobSet) -> bool {
    for component in path.components() {
        if let std::path::Component::Normal(name) = component {
            let s = match name.to_str() {
                Some(s) => s,
                None => return true, // non-UTF-8 — exclude; can't index as text anyway
            };
            if s.starts_with('.') {
                return true;
            }
            if BUILTIN_EXCLUDED_DIRS.contains(&s) {
                return true;
            }
        }
    }
    let rel = path.strip_prefix(root).unwrap_or(path);
    ignore.is_match(rel)
}

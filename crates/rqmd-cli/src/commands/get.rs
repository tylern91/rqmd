use anyhow::{Context, Result};
use std::path::Path;

use rqmd_core::db;

use crate::{format, store};

/// Parse a path spec: "collection/path.md", "#docid", or "rqmd://collection/path.md".
struct PathSpec {
    collection: String,
    path: String,
}

impl PathSpec {
    fn parse(s: &str) -> Option<Self> {
        let s = s.trim_start_matches("rqmd://");
        if let Some(rest) = s.strip_prefix('#') {
            // docid — handled separately
            return Some(Self {
                collection: String::new(),
                path: format!("#{rest}"),
            });
        }
        let (col, path) = s.split_once('/')?;
        Some(Self {
            collection: col.to_string(),
            path: path.to_string(),
        })
    }

    fn is_docid(&self) -> bool {
        self.path.starts_with('#')
    }

    fn docid_hex(&self) -> &str {
        self.path.trim_start_matches('#')
    }
}

pub fn run_get(
    index_dir: &Path,
    path_arg: &str,
    max_lines: Option<usize>,
    no_line_numbers: bool,
    fmt: &str,
) -> Result<()> {
    let s = store::open_store_no_backend(index_dir)?;

    let spec =
        PathSpec::parse(path_arg).with_context(|| format!("cannot parse path: {path_arg}"))?;

    let (title, body, file) = if spec.is_docid() {
        // Look up by hash prefix
        let docid = spec.docid_hex();
        let doc = db::get_document_by_docid_prefix(&s.db, docid)?
            .with_context(|| format!("no document found with docid #{docid}"))?;
        let body = db::get_content(&s.db, &doc.hash)?.unwrap_or_default();
        let file = format!("rrrqmd://{}/{}", doc.collection, doc.path);
        (doc.title, body, file)
    } else {
        let doc = db::get_document_by_filepath(&s.db, &spec.collection, &spec.path)?
            .with_context(|| format!("not found: {path_arg}"))?;
        let body = db::get_content(&s.db, &doc.hash)?.unwrap_or_default();
        let file = format!("rrrqmd://{}/{}", doc.collection, doc.path);
        (doc.title, body, file)
    };

    format::print_document(&file, &title, &body, fmt, max_lines, !no_line_numbers);
    Ok(())
}

pub fn run_multi_get(
    index_dir: &Path,
    pattern: &str,
    collection: Option<&str>,
    max_lines: Option<usize>,
    fmt: &str,
) -> Result<()> {
    let s = store::open_store_no_backend(index_dir)?;

    // Support comma-separated list or glob-style "*" pattern
    let patterns: Vec<&str> = pattern.split(',').map(str::trim).collect();

    let docs = db::list_documents(&s.db, collection)?;
    let mut printed = 0usize;

    for doc in &docs {
        let filepath = format!("{}/{}", doc.collection, doc.path);
        let matched = patterns.iter().any(|p| {
            if p.contains('*') {
                glob_match(p, &filepath)
            } else {
                filepath.contains(p) || doc.path.contains(p)
            }
        });
        if !matched {
            continue;
        }
        let body = db::get_content(&s.db, &doc.hash)?.unwrap_or_default();
        let file = format!("rrqmd://{filepath}");
        if printed > 0 && fmt == "cli" {
            println!("\n{}", "─".repeat(60));
        }
        format::print_document(&file, &doc.title, &body, fmt, max_lines, false);
        printed += 1;
    }

    if printed == 0 {
        eprintln!("No documents matched: {pattern}");
    }
    Ok(())
}

pub fn run_ls(index_dir: &Path, path: Option<&str>) -> Result<()> {
    let s = store::open_store_no_backend(index_dir)?;

    let (filter_collection, filter_prefix) = match path {
        None => (None, None),
        Some(p) => {
            let p = p.trim_start_matches("rqmd://");
            match p.split_once('/') {
                Some((col, prefix)) => (Some(col.to_string()), Some(prefix.to_string())),
                None => (Some(p.to_string()), None),
            }
        }
    };

    if let Some(ref col) = filter_collection {
        // List files in this collection (with optional prefix filter)
        let docs = db::list_documents(&s.db, Some(col))?;
        if docs.is_empty() {
            println!("(no documents in collection '{col}')");
            return Ok(());
        }
        for doc in &docs {
            if let Some(ref prefix) = filter_prefix {
                if !doc.path.starts_with(prefix.as_str()) {
                    continue;
                }
            }
            println!("rrrqmd://{}/{}", doc.collection, doc.path);
        }
    } else {
        // List all collections
        let cols = db::list_collections(&s.db)?;
        if cols.is_empty() {
            println!("No collections. Run `qmd collection add <path>` to add one.");
            return Ok(());
        }
        for col in &cols {
            let count = db::list_documents(&s.db, Some(&col.name))?.len();
            let default_marker = if col.include_by_default {
                ""
            } else {
                " (excluded)"
            };
            println!(
                "{:30}  {} docs  {}{}",
                col.name, count, col.path, default_marker
            );
        }
    }

    Ok(())
}

fn glob_match(pattern: &str, target: &str) -> bool {
    // Simple glob: only supports * as wildcard (any chars, including /)
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
        } else {
            if let Some(pos) = rest.find(part) {
                rest = &rest[pos + part.len()..];
            } else {
                return false;
            }
        }
    }
    true
}

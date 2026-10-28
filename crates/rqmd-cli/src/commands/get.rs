use anyhow::{Context, Result};
use std::path::Path;

use rqmd_core::{db, resolve};

use crate::format::Format;
use crate::{format, store};

/// Parse a path spec: "collection/path.md", "#docid", or "rqmd://collection/path.md".
/// Shared with `commands::similar`, which resolves the same single-ref argument shape.
pub(crate) struct PathSpec {
    pub(crate) collection: String,
    pub(crate) path: String,
}

impl PathSpec {
    pub(crate) fn parse(s: &str) -> Option<Self> {
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

    pub(crate) fn is_docid(&self) -> bool {
        self.path.starts_with('#')
    }

    pub(crate) fn docid_hex(&self) -> &str {
        self.path.trim_start_matches('#')
    }
}

pub fn run_get(
    index_dir: &Path,
    path_arg: &str,
    max_lines: Option<usize>,
    no_line_numbers: bool,
    fmt: Format,
) -> Result<()> {
    let s = store::open_existing_store_no_backend(index_dir, true)?;

    let spec =
        PathSpec::parse(path_arg).with_context(|| format!("cannot parse path: {path_arg}"))?;

    let (title, body, file, collection, rel_path) = if spec.is_docid() {
        // Look up by hash prefix
        let docid = spec.docid_hex();
        let doc = db::get_document_by_docid_prefix(&s.db, docid)?
            .with_context(|| format!("no document found with docid #{docid}"))?;
        let body = db::get_document_raw(&s.db, doc.id)?.unwrap_or_default();
        let file = format!("rqmd://{}/{}", doc.collection, doc.path);
        (doc.title, body, file, doc.collection, doc.path)
    } else {
        let doc = db::get_active_document_by_filepath(&s.db, &spec.collection, &spec.path)?
            .with_context(|| format!("not found: {path_arg}"))?;
        let body = db::get_document_raw(&s.db, doc.id)?.unwrap_or_default();
        let file = format!("rqmd://{}/{}", doc.collection, doc.path);
        (doc.title, body, file, doc.collection, doc.path)
    };

    let roots = store::collection_roots(&s, fmt)?;
    let abs_path = roots
        .get(&collection)
        .map(|root| format::resolve_absolute_path(root, &rel_path));
    format::print_document(
        &file,
        &title,
        &body,
        fmt,
        max_lines,
        !no_line_numbers,
        abs_path.as_deref(),
    )
}

pub fn run_multi_get(
    index_dir: &Path,
    pattern: &str,
    collections: Option<&[String]>,
    max_lines: Option<usize>,
    fmt: Format,
) -> Result<()> {
    let s = store::open_existing_store_no_backend(index_dir, true)?;

    // Support comma-separated list, "#docid" entries, and glob-style "*" patterns.
    let report = resolve::resolve_multi_get_report(&s.db, collections, pattern)?;
    let docs = &report.documents;
    let roots = store::collection_roots(&s, fmt)?;
    let mut printed = 0usize;

    for doc in docs {
        let body = db::get_document_raw(&s.db, doc.id)?.unwrap_or_default();
        let file = format!("rqmd://{}/{}", doc.collection, doc.path);
        if printed > 0 && fmt == Format::Cli {
            println!("\n{}", "─".repeat(60));
        }
        let abs_path = roots
            .get(&doc.collection)
            .map(|root| format::resolve_absolute_path(root, &doc.path));
        format::print_document(
            &file,
            &doc.title,
            &body,
            fmt,
            max_lines,
            false,
            abs_path.as_deref(),
        )?;
        printed += 1;
    }

    for line in multi_get_report_lines(&report) {
        eprintln!("{line}");
    }
    if printed == 0 {
        eprintln!("No documents matched: {pattern}");
    }
    Ok(())
}

/// The stderr lines for a multi-get report: one per unmatched entry, then one
/// per path entry that matched several documents. Control characters in an
/// entry become `?` so a pasted escape sequence cannot drive the terminal.
fn multi_get_report_lines(report: &resolve::MultiGetReport) -> Vec<String> {
    let clean = |entry: &str| -> String {
        entry
            .chars()
            .map(|c| if c.is_control() { '?' } else { c })
            .collect()
    };
    let unmatched = report
        .unmatched
        .iter()
        .map(|e| format!("rqmd multi-get: no document matched \"{}\"", clean(e)));
    let ambiguous = report.ambiguous.iter().map(|a| {
        format!(
            "rqmd multi-get: \"{}\" matched {} documents; all are returned",
            clean(&a.entry),
            a.matches
        )
    });
    unmatched.chain(ambiguous).collect()
}

pub fn run_ls(index_dir: &Path, path: Option<&str>) -> Result<()> {
    if store::report_if_no_index(index_dir) {
        return Ok(());
    }
    let s = store::open_store_no_backend(index_dir, true)?;

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
            if let Some(ref prefix) = filter_prefix
                && !doc.path.starts_with(prefix.as_str())
            {
                continue;
            }
            println!("rqmd://{}/{}", doc.collection, doc.path);
        }
    } else {
        // List all collections
        let cols = db::list_collections(&s.db)?;
        if cols.is_empty() {
            println!("No collections. Run `rqmd collection add <path>` to add one.");
            return Ok(());
        }
        for col in &cols {
            let (count, _) = db::collection_doc_stats(&s.db, &col.name)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multi_get_report_lines_name_each_entry_and_strip_control_characters() {
        let report = resolve::MultiGetReport {
            documents: Vec::new(),
            unmatched: vec!["missing.md".into(), "evil\x1b[2J.md".into()],
            ambiguous: vec![resolve::AmbiguousEntry {
                entry: "README.md".into(),
                matches: 3,
            }],
        };
        let lines = multi_get_report_lines(&report);
        assert_eq!(
            lines,
            [
                "rqmd multi-get: no document matched \"missing.md\"",
                "rqmd multi-get: no document matched \"evil?[2J.md\"",
                "rqmd multi-get: \"README.md\" matched 3 documents; all are returned",
            ]
        );
        assert!(lines.iter().all(|l| !l.chars().any(char::is_control)));
    }
}

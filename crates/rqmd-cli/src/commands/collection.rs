use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

use rqmd_core::{db, Collection};

use crate::{document, exclusions, format as fmt, store, CollectionCommand};

pub fn run(index_dir: &Path, cmd: CollectionCommand) -> Result<()> {
    match cmd {
        CollectionCommand::Add {
            path,
            name,
            mask,
            ignore,
            hidden,
        } => add(
            index_dir,
            &path,
            name.as_deref(),
            mask.as_deref(),
            ignore,
            hidden,
        ),
        CollectionCommand::List => list(index_dir),
        CollectionCommand::Remove { name } => remove(index_dir, &name),
        CollectionCommand::Rename { old, new } => rename(index_dir, &old, &new),
        CollectionCommand::Show { name } => show(index_dir, &name),
        CollectionCommand::UpdateCmd { name, cmd } => update_cmd(index_dir, &name, cmd.as_deref()),
        CollectionCommand::Include { name } => set_include(index_dir, &name, true),
        CollectionCommand::Exclude { name } => set_include(index_dir, &name, false),
    }
}

/// Guard against `collection add <file>` silently producing a broken document.
/// Walking a single file with `WalkDir` yields that file with an empty
/// relative path (`strip_prefix` of itself is empty), which upstream code
/// would happily persist as a document with a malformed synthetic URI.
/// Rejecting a non-directory here up front gives a clear error instead.
fn ensure_is_dir(path: &Path) -> Result<()> {
    if !path.is_dir() {
        bail!(
            "'{}' is not a directory — `collection add` indexes a directory tree, not a single file",
            path.display()
        );
    }
    Ok(())
}

fn add(
    index_dir: &Path,
    dir: &str,
    name: Option<&str>,
    mask: Option<&str>,
    ignore: Vec<String>,
    hidden: bool,
) -> Result<()> {
    let abs_dir = PathBuf::from(dir)
        .canonicalize()
        .with_context(|| format!("cannot resolve path: {dir}"))?;
    ensure_is_dir(&abs_dir)?;

    let collection_name = name
        .unwrap_or_else(|| {
            abs_dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("default")
        })
        .to_string();

    let pattern = mask.unwrap_or("**/*.md").to_string();

    eprintln!(
        "Adding collection '{}' → {}",
        collection_name,
        abs_dir.display()
    );

    let mut s = store::open_store_no_backend(index_dir, false)?;
    let is_tty = fmt::atty_stderr();

    let ignore_set = exclusions::build_ignore_set(&ignore);
    let include_set = exclusions::build_include_set(&pattern)
        .with_context(|| format!("collection '{collection_name}': invalid mask"))?;

    // Register the collection (persists pattern/ignore/hidden for future `rqmd update` runs)
    let col = Collection {
        name: collection_name.clone(),
        path: abs_dir.to_string_lossy().to_string(),
        pattern: pattern.clone(),
        ignore,
        include_by_default: true,
        update_command: None,
        allow_hidden: hidden,
    };
    db::upsert_collection(&s.db, &col)?;

    let candidates = document::collect_candidates(&abs_dir, &include_set, &ignore_set, hidden);
    let mut count = 0usize;
    let mut errors = 0usize;
    let mut skips = document::SkipCounts::default();

    for (i, path) in candidates.iter().enumerate() {
        if is_tty {
            let rel_preview = path
                .strip_prefix(&abs_dir)
                .unwrap_or(path)
                .to_string_lossy()
                .to_string();
            let line = format!("  Indexing {} ({})", rel_preview, i + 1);
            let w = fmt::term_width().unwrap_or(80).saturating_sub(1);
            eprint!("\r\x1b[2K{}", fmt::fit_to_width(&line, w));
        }

        let doc = match document::prepare(path, &abs_dir) {
            Ok(doc) => doc,
            Err(reason) => {
                skips.record(reason);
                continue;
            }
        };

        match s.index_document_fts_only_with_raw(
            &collection_name,
            &doc.rel_path,
            &doc.title,
            &doc.indexed_text,
            &doc.raw,
        ) {
            Ok(_) => count += 1,
            Err(e) => {
                eprintln!("\n  WARN: skipping {}: {e:#}", doc.rel_path);
                errors += 1;
            }
        }
    }

    s.flush()?;
    // Clear the progress line before printing the summary.
    if is_tty {
        eprint!("\r{}\r", " ".repeat(fmt::term_width().unwrap_or(80)));
    }

    let mut summary = format!("  Indexed {count} document(s)");
    if skips.total() > 0 {
        summary.push_str(&format!(
            " · skipped {} ({})",
            skips.total(),
            skips.describe()
        ));
    }
    if errors > 0 {
        summary.push_str(&format!(" · {errors} error(s)"));
    }
    summary.push('.');
    println!("{summary}");

    if count == 0 {
        eprintln!(
            "  WARN: 0 documents indexed into new collection '{collection_name}'. Likely causes: \
             the mask '{pattern}' matched nothing, every matched file was unreadable, or files live \
             under a dot-directory that was excluded (try --hidden)."
        );
    }

    eprintln!("Collection '{}' ready.", collection_name);
    Ok(())
}

fn list(index_dir: &Path) -> Result<()> {
    let s = store::open_store_no_backend(index_dir, true)?;
    let cols = db::list_collections(&s.db)?;
    if cols.is_empty() {
        println!("No collections. Run `rqmd collection add <path> --name <name>` to add one.");
        return Ok(());
    }
    println!("{:<30}  {:<8}  {:<12}  PATH", "NAME", "DOCS", "INCLUDED");
    println!("{}", "─".repeat(80));
    for col in &cols {
        let count = db::list_documents(&s.db, Some(&col.name))?.len();
        let included = if col.include_by_default { "yes" } else { "no" };
        println!(
            "{:<30}  {:<8}  {:<12}  {}",
            col.name, count, included, col.path
        );
    }
    Ok(())
}

fn remove(index_dir: &Path, name: &str) -> Result<()> {
    // Write access (not the usual read_only=true for a metadata-only op): the sweep
    // below deletes rows and Tantivy entries, and flush() needs a non-view HNSW handle.
    let mut s = store::open_store_no_backend(index_dir, false)?;
    // Verify it exists first
    let cols = db::list_collections(&s.db)?;
    if !cols.iter().any(|c| c.name == name) {
        bail!("collection '{name}' not found");
    }

    // Purge everything this collection owns — documents, orphaned content/vectors,
    // and the store_collections row — then sweep the matching Tantivy entries so
    // removed documents stop being searchable everywhere, not just via SQLite.
    let filepaths = db::purge_collection(&s.db, name).context("purge collection")?;
    for filepath in &filepaths {
        if let Err(e) = s.remove_from_fts(filepath) {
            eprintln!("  WARN: failed to remove stale FTS entry for {filepath}: {e:#}");
        }
    }
    s.flush()?;

    println!(
        "Collection '{name}' removed ({} document(s) purged).",
        filepaths.len()
    );
    Ok(())
}

fn rename(index_dir: &Path, old: &str, new: &str) -> Result<()> {
    let s = store::open_store_no_backend(index_dir, true)?;
    let cols = db::list_collections(&s.db)?;
    if !cols.iter().any(|c| c.name == old) {
        bail!("collection '{old}' not found");
    }
    if cols.iter().any(|c| c.name == new) {
        bail!("collection '{new}' already exists");
    }
    db::rename_collection(&s.db, old, new)?;
    println!("Renamed '{old}' → '{new}'.");
    Ok(())
}

fn show(index_dir: &Path, name: &str) -> Result<()> {
    let s = store::open_store_no_backend(index_dir, true)?;
    let cols = db::list_collections(&s.db)?;
    let col = cols
        .iter()
        .find(|c| c.name == name)
        .with_context(|| format!("collection '{name}' not found"))?;
    let count = db::list_documents(&s.db, Some(name))?.len();

    println!("Collection: {}", col.name);
    println!("  Path:     {}", col.path);
    println!("  Pattern:  {}", col.pattern);
    println!("  Docs:     {count}");
    println!("  Included: {}", col.include_by_default);
    println!("  Hidden:   {}", col.allow_hidden);
    if let Some(ref cmd) = col.update_command {
        println!("  Hook:     {cmd}");
    }
    Ok(())
}

fn update_cmd(index_dir: &Path, name: &str, cmd: Option<&str>) -> Result<()> {
    let s = store::open_store_no_backend(index_dir, true)?;
    db::set_collection_update_cmd(&s.db, name, cmd)?;
    match cmd {
        Some(c) => println!("Set update command for '{name}': {c}"),
        None => println!("Cleared update command for '{name}'."),
    }
    Ok(())
}

fn set_include(index_dir: &Path, name: &str, include: bool) -> Result<()> {
    let s = store::open_store_no_backend(index_dir, true)?;
    db::set_collection_include(&s.db, name, include)?;
    let verb = if include {
        "included in"
    } else {
        "excluded from"
    };
    println!("Collection '{name}' {verb} default queries.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_is_dir_rejects_a_plain_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("note.md");
        std::fs::write(&file, "body").unwrap();
        assert!(ensure_is_dir(&file).is_err());
    }

    #[test]
    fn ensure_is_dir_accepts_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert!(ensure_is_dir(dir.path()).is_ok());
    }
}

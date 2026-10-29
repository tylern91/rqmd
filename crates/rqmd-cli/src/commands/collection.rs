use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

use rqmd_core::{Collection, IndexLock, db};

use crate::{CollectionCommand, document, exclusions, format as fmt, store};

pub fn run(index_dir: &Path, cmd: CollectionCommand) -> Result<()> {
    match cmd {
        CollectionCommand::Add {
            path,
            name,
            mask,
            ignore,
            hidden,
            respect_gitignore,
            max_file_bytes,
        } => add(
            index_dir,
            &path,
            name.as_deref(),
            mask.as_deref(),
            ignore,
            WalkFlags {
                hidden,
                respect_gitignore,
                max_file_bytes,
            },
        ),
        CollectionCommand::List => list(index_dir),
        CollectionCommand::Remove { name } => remove(index_dir, &name),
        CollectionCommand::Rename { old, new } => rename(index_dir, &old, &new),
        CollectionCommand::Show { name } => show(index_dir, &name),
        CollectionCommand::UpdateCmd { name, cmd } => update_cmd(index_dir, &name, cmd.as_deref()),
        CollectionCommand::Include { name } => set_include(index_dir, &name, true),
        CollectionCommand::Exclude { name } => set_include(index_dir, &name, false),
        CollectionCommand::Set {
            name,
            embed_ignore,
            clear_embed_ignore,
            embed_max_bytes,
            clear_embed_max_bytes,
        } => set(
            index_dir,
            &name,
            EmbedRuleChange {
                ignore: if clear_embed_ignore {
                    Some(Vec::new())
                } else if embed_ignore.is_empty() {
                    None
                } else {
                    Some(embed_ignore)
                },
                max_bytes: if clear_embed_max_bytes {
                    Some(None)
                } else {
                    embed_max_bytes.map(Some)
                },
            },
        ),
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

/// The `collection add` flags that shape how the tree is walked.
struct WalkFlags {
    hidden: bool,
    respect_gitignore: bool,
    max_file_bytes: Option<u64>,
}

fn add(
    index_dir: &Path,
    dir: &str,
    name: Option<&str>,
    mask: Option<&str>,
    ignore: Vec<String>,
    walk: WalkFlags,
) -> Result<()> {
    let WalkFlags {
        hidden,
        respect_gitignore,
        max_file_bytes,
    } = walk;
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

    validate_collection_name(&collection_name)?;

    let pattern = mask.unwrap_or("**/*.md").to_string();
    exclusions::validate_ignore_patterns(&ignore)
        .with_context(|| format!("collection '{collection_name}'"))?;

    eprintln!(
        "Adding collection '{}' → {}",
        collection_name,
        abs_dir.display()
    );

    let mut s = store::open_store_no_backend(index_dir, false)?;
    // Re-adding a name keeps what `add` has no flag for: the update hook, the
    // include-by-default setting and the embed-exclusion rules (`collection
    // set`). Walk settings come from this invocation.
    let existing = db::list_collections(&s.db)?
        .into_iter()
        .find(|c| c.name == collection_name);
    if existing.is_some() {
        eprintln!(
            "Updating existing collection '{collection_name}' (keeping its update hook and include setting)"
        );
    }
    let mut progress = fmt::Progress::new(fmt::ProgressMode::detect());

    let ignore_set = exclusions::build_ignore_set(&ignore);
    let include_set = exclusions::build_include_set(&pattern)
        .with_context(|| format!("collection '{collection_name}': invalid mask"))?;

    // Register the collection (persists pattern/ignore/hidden for future `rqmd update` runs)
    let col = Collection {
        name: collection_name.clone(),
        path: abs_dir.to_string_lossy().to_string(),
        pattern: pattern.clone(),
        ignore,
        include_by_default: existing.as_ref().is_none_or(|c| c.include_by_default),
        update_command: existing.as_ref().and_then(|c| c.update_command.clone()),
        allow_hidden: hidden,
        respect_gitignore,
        max_file_bytes,
        embed_ignore: existing
            .as_ref()
            .map(|c| c.embed_ignore.clone())
            .unwrap_or_default(),
        embed_max_bytes: existing.as_ref().and_then(|c| c.embed_max_bytes),
    };
    db::upsert_collection(&s.db, &col)?;

    let document::Candidates {
        files: candidates,
        too_large,
        excluded_dirs,
    } = document::collect_candidates(
        &abs_dir,
        &include_set,
        &ignore_set,
        document::WalkOptions::for_collection(&col),
    );
    let mut count = 0usize;
    let mut errors = 0usize;
    let mut skips = document::SkipCounts {
        too_large,
        ..Default::default()
    };

    for (i, path) in candidates.iter().enumerate() {
        if progress.is_active() {
            let rel_preview = path
                .strip_prefix(&abs_dir)
                .unwrap_or(path)
                .to_string_lossy()
                .to_string();
            let line = format!("  Indexing {} ({})", rel_preview, i + 1);
            progress.show(&line, i + 1, candidates.len());
        }

        let stat = document::stat_file(path);
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
            Ok(_) => {
                count += 1;
                db::set_document_stat(
                    &s.db,
                    &collection_name,
                    &doc.rel_path,
                    stat.and_then(document::trusted_stat),
                )?;
            }
            Err(e) => {
                eprintln!("\n  WARN: skipping {}: {e:#}", doc.rel_path);
                errors += 1;
            }
        }
    }

    s.flush()?;
    // Clear the progress line before printing the summary.
    progress.clear();

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
    if let Some(clause) = document::excluded_dirs_clause(&excluded_dirs) {
        summary.push_str(&format!(" · {clause}"));
    }
    summary.push('.');
    println!("{summary}");

    if count == 0 {
        eprintln!(
            "  WARN: 0 documents indexed into new collection '{collection_name}'. Likely causes: \
             the mask '{pattern}' matched nothing, every matched file was unreadable, files live \
             under a dot-directory that was excluded (try --hidden), or files live under a folder \
             the built-in rule leaves out ({}).",
            exclusions::builtin_excluded_dirs().join(", ")
        );
    }

    eprintln!("Collection '{collection_name}' ready.");
    Ok(())
}

fn list(index_dir: &Path) -> Result<()> {
    if store::report_if_no_index(index_dir) {
        return Ok(());
    }
    let s = store::open_store_no_backend(index_dir, true)?;
    let cols = db::list_collections(&s.db)?;
    if cols.is_empty() {
        println!("No collections. Run `rqmd collection add <path> --name <name>` to add one.");
        return Ok(());
    }
    println!("{:<30}  {:<8}  {:<12}  PATH", "NAME", "DOCS", "INCLUDED");
    println!("{}", "─".repeat(80));
    for col in &cols {
        let (count, _) = db::collection_doc_stats(&s.db, &col.name)?;
        let included = if col.include_by_default { "yes" } else { "no" };
        println!(
            "{:<30}  {:<8}  {:<12}  {}",
            col.name, count, included, col.path
        );
    }
    Ok(())
}

fn remove(index_dir: &Path, name: &str) -> Result<()> {
    // Shares IndexLock with embed/update: purging a collection mutates
    // `documents`/`content_vectors` the same way a concurrent embed's vid
    // allocation does, and the two must not race.
    let _lock = IndexLock::acquire(index_dir)?;

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
    // purge_collection deletes the document rows before we can capture which vids
    // it orphaned, so this evicts them from HNSW after the fact by re-querying —
    // the leak this closes: a purge previously left live vectors that
    // `count_orphaned_vectors` couldn't see, unlike `run_update`'s equivalent sweep.
    // `reclaim_orphaned_vectors` only flushes when it finds something to evict, so
    // an explicit flush still follows to commit the FTS removals above either way.
    s.reclaim_orphaned_vectors()?;
    s.flush()?;

    println!(
        "Collection '{name}' removed ({} document(s) purged).",
        filepaths.len()
    );
    Ok(())
}

/// A collection name appears as `<collection>/<path>` in `rqmd get` and in
/// `rqmd://` URIs, so it must be non-empty and free of `/`.
fn validate_collection_name(name: &str) -> Result<()> {
    if name.trim().is_empty() {
        bail!("invalid collection name '{name}': must not be empty");
    }
    if name.contains('/') {
        bail!("invalid collection name '{name}': must not contain '/'");
    }
    Ok(())
}

fn rename(index_dir: &Path, old: &str, new: &str) -> Result<()> {
    validate_collection_name(new)?;
    // Shares IndexLock with embed/update/remove: the rename rewrites the
    // collection's full-text entries, which a concurrent writer must not touch.
    let _lock = IndexLock::acquire(index_dir)?;

    let mut s = store::open_store_no_backend(index_dir, false)?;
    let cols = db::list_collections(&s.db)?;
    if !cols.iter().any(|c| c.name == old) {
        bail!("collection '{old}' not found");
    }
    if cols.iter().any(|c| c.name == new) {
        bail!("collection '{new}' already exists");
    }
    let summary = s.rename_collection(old, new)?;
    println!(
        "Renamed '{old}' → '{new}' ({} document(s) re-indexed).",
        summary.documents
    );
    if summary.stale_removed > 0 {
        println!(
            "Removed {} stale full-text entr{} left by an earlier rename.",
            summary.stale_removed,
            if summary.stale_removed == 1 {
                "y"
            } else {
                "ies"
            }
        );
    }
    Ok(())
}

fn show(index_dir: &Path, name: &str) -> Result<()> {
    let s = store::open_store_no_backend(index_dir, true)?;
    let cols = db::list_collections(&s.db)?;
    let col = cols
        .iter()
        .find(|c| c.name == name)
        .with_context(|| format!("collection '{name}' not found"))?;
    let (count, _) = db::collection_doc_stats(&s.db, name)?;

    println!("Collection: {}", col.name);
    println!("  Path:     {}", col.path);
    println!("  Pattern:  {}", col.pattern);
    println!("  Docs:     {count}");
    println!("  Included: {}", col.include_by_default);
    println!("  Hidden:   {}", col.allow_hidden);
    if col.ignore.is_empty() {
        println!("  Ignore:   (none)");
    } else {
        println!("  Ignore:   {}", col.ignore.join(", "));
    }
    println!("  Gitignore: {}", col.respect_gitignore);
    println!(
        "  Max file: {} bytes",
        col.max_file_bytes
            .unwrap_or(document::DEFAULT_MAX_FILE_BYTES)
    );
    // Shown only when set, so a collection without embed rules prints exactly
    // what it printed before they existed.
    if !col.embed_ignore.is_empty() {
        println!("  Embed ignore: {}", col.embed_ignore.join(", "));
    }
    if let Some(cap) = col.embed_max_bytes {
        println!("  Embed max:    {cap} bytes (indexed text)");
    }
    if let Some(ref cmd) = col.update_command {
        println!("  Hook:     {cmd}");
    }
    Ok(())
}

/// What `collection set` changes. `None` leaves a setting as stored.
struct EmbedRuleChange {
    ignore: Option<Vec<String>>,
    max_bytes: Option<Option<u64>>,
}

/// `collection set`: change the embed-exclusion rules of an existing
/// collection. Takes effect at the next `rqmd embed`; nothing is re-embedded
/// and no vector is deleted here (`rqmd embed --cleanup` drops the vectors
/// of documents that became excluded).
fn set(index_dir: &Path, name: &str, change: EmbedRuleChange) -> Result<()> {
    if change.ignore.is_none() && change.max_bytes.is_none() {
        bail!(
            "nothing to set: pass --embed-ignore, --clear-embed-ignore, --embed-max-bytes or --clear-embed-max-bytes"
        );
    }
    if let Some(ref patterns) = change.ignore {
        exclusions::validate_ignore_patterns(patterns)
            .with_context(|| format!("collection '{name}'"))?;
    }
    if !store::index_exists(index_dir) {
        bail!("collection '{name}' not found");
    }
    let s = store::open_store_no_backend(index_dir, true)?;
    let col = db::list_collections(&s.db)?
        .into_iter()
        .find(|c| c.name == name)
        .with_context(|| format!("collection '{name}' not found"))?;
    let ignore = change.ignore.unwrap_or(col.embed_ignore);
    let max_bytes = change.max_bytes.unwrap_or(col.embed_max_bytes);
    db::set_collection_embed_rules(&s.db, name, &ignore, max_bytes)?;

    let ignore_text = if ignore.is_empty() {
        "(none)".to_string()
    } else {
        ignore.join(", ")
    };
    let cap_text = max_bytes.map_or("(none)".to_string(), |n| format!("{n} bytes"));
    println!("Collection '{name}': embed ignore {ignore_text}; embed max {cap_text}.");
    println!(
        "Applies from the next `rqmd embed`. Vectors already stored for newly excluded \
         documents are kept until `rqmd embed --cleanup`."
    );
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

    fn seed_collection(index_dir: &Path, name: &str, path: &str) {
        let s = store::open_store_no_backend(index_dir, false).unwrap();
        db::upsert_collection(
            &s.db,
            &Collection {
                name: name.into(),
                path: path.into(),
                pattern: "**/*.md".into(),
                ignore: vec![],
                include_by_default: true,
                update_command: None,
                allow_hidden: false,
                respect_gitignore: false,
                max_file_bytes: None,
                embed_ignore: vec![],
                embed_max_bytes: None,
            },
        )
        .unwrap();
    }

    /// Regression for the confirmed leak: `remove` purged a collection's rows
    /// and flushed, but never evicted the purged hash's vectors from HNSW —
    /// `count_orphaned_vectors` can't see a vector still live in HNSW, so the
    /// leak was invisible to `doctor` too. A hash shared with a surviving
    /// collection must not be evicted.
    #[test]
    fn remove_evicts_orphaned_vectors_but_keeps_hash_shared_with_another_collection() {
        let index_dir = tempfile::tempdir().unwrap();
        let now = "2026-01-01T00:00:00Z";

        seed_collection(index_dir.path(), "keep", "/keep");
        seed_collection(index_dir.path(), "gone", "/gone");

        let s = store::open_store_no_backend(index_dir.path(), false).unwrap();
        db::upsert_content(&s.db, "shared_hash", "shared body", now).unwrap();
        db::upsert_content(&s.db, "unique_hash", "unique body", now).unwrap();
        db::upsert_document(&s.db, "keep", "shared.md", "S", "shared_hash", now).unwrap();
        db::upsert_document(&s.db, "gone", "shared.md", "S", "shared_hash", now).unwrap();
        db::upsert_document(&s.db, "gone", "unique.md", "U", "unique_hash", now).unwrap();
        db::upsert_vector_meta(&s.db, "shared_hash", 0, 0, "m", "fp", 1, 900, now).unwrap();
        db::upsert_vector_meta(&s.db, "unique_hash", 0, 0, "m", "fp", 1, 901, now).unwrap();
        drop(s);

        remove(index_dir.path(), "gone").unwrap();

        let s = store::open_store_no_backend(index_dir.path(), true).unwrap();
        assert!(
            db::hash_has_any_vector(&s.db, "shared_hash"),
            "shared_hash's vector must survive — 'keep' still references it"
        );
        assert!(
            !db::hash_has_any_vector(&s.db, "unique_hash"),
            "unique_hash's vector must be reclaimed once 'gone' is purged"
        );
        assert_eq!(db::count_orphaned_vectors(&s.db).unwrap(), 0);
    }
}

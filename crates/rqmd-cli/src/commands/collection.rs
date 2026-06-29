use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

use rqmd_core::{db, Collection};

use crate::{store, CollectionCommand};

pub fn run(index_dir: &Path, cmd: CollectionCommand) -> Result<()> {
    match cmd {
        CollectionCommand::Add { path, name, mask } => {
            add(index_dir, &path, name.as_deref(), mask.as_deref())
        }
        CollectionCommand::List => list(index_dir),
        CollectionCommand::Remove { name } => remove(index_dir, &name),
        CollectionCommand::Rename { old, new } => rename(index_dir, &old, &new),
        CollectionCommand::Show { name } => show(index_dir, &name),
        CollectionCommand::UpdateCmd { name, cmd } => update_cmd(index_dir, &name, cmd.as_deref()),
        CollectionCommand::Include { name } => set_include(index_dir, &name, true),
        CollectionCommand::Exclude { name } => set_include(index_dir, &name, false),
    }
}

fn add(index_dir: &Path, dir: &str, name: Option<&str>, mask: Option<&str>) -> Result<()> {
    let abs_dir = PathBuf::from(dir)
        .canonicalize()
        .with_context(|| format!("cannot resolve path: {dir}"))?;

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

    let mut s = store::open_store_no_backend(index_dir)?;

    // Register the collection
    let col = Collection {
        name: collection_name.clone(),
        path: abs_dir.to_string_lossy().to_string(),
        pattern: pattern.clone(),
        ignore: vec![],
        include_by_default: true,
        update_command: None,
    };
    db::upsert_collection(&s.db, &col)?;

    // Walk directory and index matching files
    let ext = mask_to_extension(&pattern);
    let mut count = 0usize;
    let mut errors = 0usize;

    for entry in WalkDir::new(&abs_dir)
        .follow_links(true)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if let Some(ref ext_filter) = ext {
            if path.extension().and_then(|e| e.to_str()) != Some(ext_filter.as_str()) {
                continue;
            }
        }

        let rel_path = path
            .strip_prefix(&abs_dir)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();

        let body = match std::fs::read_to_string(path) {
            Ok(b) => b,
            Err(_) => continue, // skip non-UTF8 files silently
        };

        let title = body
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or(&rel_path)
            .trim_start_matches('#')
            .trim()
            .to_string();

        print!("\r  Indexing {} ({}) ...", rel_path, count + 1);
        match s.index_document_fts_only(&collection_name, &rel_path, &title, &body) {
            Ok(_) => count += 1,
            Err(e) => {
                eprintln!("\n  WARN: skipping {rel_path}: {e:#}");
                errors += 1;
            }
        }
    }

    s.flush()?;
    println!(
        "\r  Indexed {count} document(s){}.",
        if errors > 0 {
            format!(", {errors} error(s)")
        } else {
            String::new()
        }
    );
    eprintln!("Collection '{}' ready.", collection_name);
    Ok(())
}

fn list(index_dir: &Path) -> Result<()> {
    let s = store::open_store_no_backend(index_dir)?;
    let cols = db::list_collections(&s.db)?;
    if cols.is_empty() {
        println!("No collections. Run `qmd collection add <path> --name <name>` to add one.");
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
    let s = store::open_store_no_backend(index_dir)?;
    // Verify it exists first
    let cols = db::list_collections(&s.db)?;
    if !cols.iter().any(|c| c.name == name) {
        bail!("collection '{name}' not found");
    }
    db::delete_collection(&s.db, name)?;
    println!("Collection '{name}' removed.");
    Ok(())
}

fn rename(index_dir: &Path, old: &str, new: &str) -> Result<()> {
    let s = store::open_store_no_backend(index_dir)?;
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
    let s = store::open_store_no_backend(index_dir)?;
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
    if let Some(ref cmd) = col.update_command {
        println!("  Hook:     {cmd}");
    }
    Ok(())
}

fn update_cmd(index_dir: &Path, name: &str, cmd: Option<&str>) -> Result<()> {
    let s = store::open_store_no_backend(index_dir)?;
    db::set_collection_update_cmd(&s.db, name, cmd)?;
    match cmd {
        Some(c) => println!("Set update command for '{name}': {c}"),
        None => println!("Cleared update command for '{name}'."),
    }
    Ok(())
}

fn set_include(index_dir: &Path, name: &str, include: bool) -> Result<()> {
    let s = store::open_store_no_backend(index_dir)?;
    db::set_collection_include(&s.db, name, include)?;
    let verb = if include {
        "included in"
    } else {
        "excluded from"
    };
    println!("Collection '{name}' {verb} default queries.");
    Ok(())
}

/// Extract extension from mask pattern like "**/*.md" → Some("md")
fn mask_to_extension(mask: &str) -> Option<String> {
    let base = mask.rsplit('/').next().unwrap_or(mask);
    if let Some(ext) = base.rsplit('.').next() {
        if ext != base && !ext.is_empty() {
            return Some(ext.to_string());
        }
    }
    None
}

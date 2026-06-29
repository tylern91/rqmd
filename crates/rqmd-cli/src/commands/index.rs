use anyhow::{Context, Result};
use std::path::Path;
use walkdir::WalkDir;

use rqmd_core::db;

use crate::store;

pub fn run_status(index_dir: &Path) -> Result<()> {
    let s = store::open_store_no_backend(index_dir)?;

    let db_path = index_dir.join("index.sqlite");
    let db_size = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);
    let tantivy_size: u64 = dir_size(&index_dir.join("tantivy"));
    let hnsw_size = std::fs::metadata(index_dir.join("hnsw.usearch"))
        .map(|m| m.len())
        .unwrap_or(0);

    let total_docs: i64 =
        s.db.query_row("SELECT COUNT(*) FROM documents WHERE active=1", [], |r| {
            r.get(0)
        })
        .unwrap_or(0);
    let total_vecs: i64 =
        s.db.query_row("SELECT COUNT(*) FROM content_vectors", [], |r| r.get(0))
            .unwrap_or(0);

    println!("QMD Status (Rust engine)\n");
    println!("  Index:    {}", index_dir.display());
    println!("  SQLite:   {}", fmt_bytes(db_size));
    println!("  Tantivy:  {}", fmt_bytes(tantivy_size));
    println!("  HNSW:     {}", fmt_bytes(hnsw_size));
    println!("  Docs:     {total_docs}");
    println!("  Vectors:  {total_vecs}");
    println!();

    let cols = db::list_collections(&s.db)?;
    if cols.is_empty() {
        println!("  No collections.");
    } else {
        println!("  {:<30}  {:<8}  INCLUDED", "COLLECTION", "DOCS");
        println!("  {}", "─".repeat(60));
        for col in &cols {
            let count = db::list_documents(&s.db, Some(&col.name))?.len();
            let incl = if col.include_by_default { "yes" } else { "no" };
            println!("  {:<30}  {:<8}  {incl}", col.name, count);
        }
    }
    Ok(())
}

pub fn run_embed(index_dir: &Path, collection: Option<&str>) -> Result<()> {
    let cols = {
        let s = store::open_store_no_backend(index_dir)?;
        match collection {
            Some(c) => vec![db::list_collections(&s.db)?
                .into_iter()
                .find(|col| col.name == c)
                .with_context(|| format!("collection '{c}' not found"))?],
            None => db::list_collections(&s.db)?,
        }
    };

    if cols.is_empty() {
        println!("No collections to embed.");
        return Ok(());
    }

    eprintln!("Loading inference backend...");
    let mut s = store::open_store_with_backend(index_dir)?;

    for col in &cols {
        eprintln!("Embedding collection '{}'...", col.name);
        let docs = db::list_documents(&s.db, Some(&col.name))?;
        let mut count = 0usize;
        for doc in &docs {
            let body = db::get_content(&s.db, &doc.hash)?.unwrap_or_default();
            if body.is_empty() {
                continue;
            }
            s.index_document(&doc.collection, &doc.path, &doc.title, &body)?;
            count += 1;
        }
        eprintln!("  Embedded {count} doc(s) in '{}'.", col.name);
    }

    s.flush()?;
    Ok(())
}

pub fn run_update(index_dir: &Path, collection: Option<&str>) -> Result<()> {
    // Re-walk each collection's directory and re-index changed files.
    let cols = {
        let s = store::open_store_no_backend(index_dir)?;
        match collection {
            Some(c) => vec![db::list_collections(&s.db)?
                .into_iter()
                .find(|col| col.name == c)
                .with_context(|| format!("collection '{c}' not found"))?],
            None => db::list_collections(&s.db)?,
        }
    };

    if cols.is_empty() {
        println!("No collections to update.");
        return Ok(());
    }

    let mut s = store::open_store_with_backend(index_dir)?;

    for col in &cols {
        eprintln!("Updating collection '{}'...", col.name);
        let dir = Path::new(&col.path);
        if !dir.exists() {
            eprintln!("  WARN: directory not found: {}", dir.display());
            continue;
        }

        let ext = col
            .pattern
            .rsplit('/')
            .next()
            .and_then(|base| base.rsplit('.').next())
            .filter(|e| *e != "*")
            .map(|e| e.to_string());

        let mut count = 0usize;
        for entry in WalkDir::new(dir)
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
            let rel = path
                .strip_prefix(dir)
                .unwrap_or(path)
                .to_string_lossy()
                .to_string();
            let body = match std::fs::read_to_string(path) {
                Ok(b) => b,
                Err(_) => continue,
            };
            let title = body
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or(&rel)
                .trim_start_matches('#')
                .trim()
                .to_string();
            if let Err(e) = s.index_document(&col.name, &rel, &title, &body) {
                eprintln!("  WARN: {rel}: {e:#}");
            } else {
                count += 1;
            }
        }
        s.flush()?;
        eprintln!("  Updated {count} doc(s) in '{}'.", col.name);
    }
    Ok(())
}

pub fn run_init() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let qmd_dir = cwd.join(".rqmd");

    if qmd_dir.exists() {
        println!("Local index already exists at {}", qmd_dir.display());
        return Ok(());
    }

    std::fs::create_dir_all(&qmd_dir)?;
    // Touch the SQLite db to create it
    let _ = store::open_store_no_backend(&qmd_dir)?;
    println!("Initialized local index at {}", qmd_dir.display());
    println!("Run `qmd collection add <path> --name <name>` to add a collection.");
    Ok(())
}

pub fn run_doctor(index_dir: &Path) -> Result<()> {
    println!("QMD Doctor (Rust engine)\n");

    let db_path = index_dir.join("index.sqlite");
    println!("  Index dir:     {}", index_dir.display());
    println!(
        "  SQLite exists: {}",
        if db_path.exists() {
            "yes"
        } else {
            "NO — run any qmd command to create"
        }
    );
    println!("  Tantivy dir:   {}", index_dir.join("tantivy").display());
    println!(
        "  HNSW file:     {}",
        index_dir.join("hnsw.usearch").display()
    );
    println!();

    // Check models cache
    let model_cache = dirs::cache_dir()
        .unwrap_or_default()
        .join("huggingface/hub");
    println!("  Model cache:   {}", model_cache.display());

    let embed_model = model_cache.join("models--ggml-org--embeddinggemma-300M-GGUF");
    println!(
        "  Embed model:   {}",
        if embed_model.exists() {
            "cached ✓"
        } else {
            "not cached (downloads on first embed/query)"
        }
    );

    let rerank_model = model_cache.join("models--ggml-org--Qwen3-Reranker-0.6B-Q8_0-GGUF");
    println!(
        "  Rerank model:  {}",
        if rerank_model.exists() {
            "cached ✓"
        } else {
            "not cached"
        }
    );

    // Check GPU
    #[cfg(target_os = "macos")]
    println!("  GPU backend:   Metal (Apple Silicon detected)");
    #[cfg(not(target_os = "macos"))]
    println!("  GPU backend:   check llama.cpp build flags");

    if db_path.exists() {
        let s = store::open_store_no_backend(index_dir)?;
        let cols = db::list_collections(&s.db)?;
        println!("\n  Collections:   {}", cols.len());
        for col in &cols {
            let count = db::list_documents(&s.db, Some(&col.name))?.len();
            println!("    {} — {count} docs at {}", col.name, col.path);
        }
    }
    Ok(())
}

fn fmt_bytes(b: u64) -> String {
    if b < 1024 {
        format!("{b} B")
    } else if b < 1024 * 1024 {
        format!("{:.1} KB", b as f64 / 1024.0)
    } else if b < 1024 * 1024 * 1024 {
        format!("{:.1} MB", b as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} GB", b as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

fn dir_size(dir: &Path) -> u64 {
    if !dir.exists() {
        return 0;
    }
    WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
}

use anyhow::Result;
use std::path::Path;

use rqmd_core::db;

use crate::{store, ContextCommand};

const CTX_PREFIX: &str = "context:";

pub fn run(index_dir: &Path, cmd: ContextCommand) -> Result<()> {
    match cmd {
        ContextCommand::Add { path, text } => add(index_dir, path.as_deref(), &text),
        ContextCommand::List => list(index_dir),
        ContextCommand::Rm { path } => rm(index_dir, &path),
        ContextCommand::Check => check(index_dir),
    }
}

fn context_key(path: &str) -> String {
    format!("{CTX_PREFIX}{path}")
}

fn add(index_dir: &Path, path: Option<&str>, text: &str) -> Result<()> {
    let s = store::open_store_no_backend(index_dir)?;
    let key_path = path.unwrap_or("/");
    let key = context_key(key_path);
    db::set_config(&s.db, &key, text)?;
    println!("Context set for '{key_path}'.");
    Ok(())
}

fn list(index_dir: &Path) -> Result<()> {
    let s = store::open_store_no_backend(index_dir)?;
    // Read all config keys that start with context:
    let mut stmt =
        s.db.prepare("SELECT key, value FROM store_config WHERE key LIKE ?1 ORDER BY key")?;
    let prefix = format!("{CTX_PREFIX}%");
    let rows: Vec<(String, String)> = stmt
        .query_map([&prefix], |row| Ok((row.get(0)?, row.get(1)?)))?
        .filter_map(|r| r.ok())
        .collect();

    if rows.is_empty() {
        println!("No contexts set. Run `qmd context add [path] \"description\"` to add one.");
        return Ok(());
    }
    for (key, value) in &rows {
        let path = key.trim_start_matches(CTX_PREFIX);
        println!("{path}");
        for line in value.lines().take(3) {
            println!("  {line}");
        }
        println!();
    }
    Ok(())
}

fn rm(index_dir: &Path, path: &str) -> Result<()> {
    let s = store::open_store_no_backend(index_dir)?;
    let key = context_key(path);
    s.db.execute("DELETE FROM store_config WHERE key=?1", [&key])?;
    println!("Removed context for '{path}'.");
    Ok(())
}

fn check(index_dir: &Path) -> Result<()> {
    let s = store::open_store_no_backend(index_dir)?;
    let cols = db::list_collections(&s.db)?;
    let mut missing = 0usize;
    for col in &cols {
        let key = db::collection_context_key(&col.name);
        if db::get_config(&s.db, &key)?.is_none() {
            println!("MISSING context for collection '{}'", col.name);
            println!(
                "  Run: qmd context add rqmd://{}/ \"<description>\"",
                col.name
            );
            missing += 1;
        }
    }
    if missing == 0 {
        println!("All collections have context set.");
    }
    Ok(())
}

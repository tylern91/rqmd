use anyhow::{bail, Result};
use std::path::Path;

use rqmd_core::{db, Store};

use crate::{store, ContextCommand};

const CTX_PREFIX: &str = "context:";

pub fn run(index_dir: &Path, cmd: ContextCommand) -> Result<()> {
    match cmd {
        ContextCommand::Add { args } => {
            let (path, text) = split_add_args(args);
            add(index_dir, path.as_deref(), &text)
        }
        ContextCommand::List => list(index_dir),
        ContextCommand::Rm { path } => rm(index_dir, &path),
        ContextCommand::Check => check(index_dir),
    }
}

/// Split the bounded (1 or 2 element, enforced by clap's `num_args = 1..=2`)
/// `context add` positional args into `(path, text)`. One arg is `<text>`
/// with an implicit root path; two are `<path> <text>`.
fn split_add_args(mut args: Vec<String>) -> (Option<String>, String) {
    if args.len() == 2 {
        let text = args.remove(1);
        (Some(args.remove(0)), text)
    } else {
        (None, args.remove(0))
    }
}

fn context_key(path: &str) -> String {
    format!("{CTX_PREFIX}{path}")
}

/// Resolve a user-supplied `context add`/`rm` path to the canonical
/// `store_config` key every reader (`db::get_context_for_path`,
/// `db::get_context_for_collection`, `context check`) looks up.
///
/// Accepts the root (`/`), an already-qualified `rqmd://...` path, or a bare
/// collection name (resolved against `db::list_collections`). A bare name
/// that doesn't match a known collection is rejected rather than silently
/// written to a key nothing reads.
///
/// Returns `(display_path, key)`.
fn resolve_context_target(store: &Store, path: &str) -> Result<(String, String)> {
    if path == "/" || path.starts_with("rqmd://") {
        return Ok((path.to_string(), context_key(path)));
    }
    let cols = db::list_collections(&store.db)?;
    if cols.iter().any(|c| c.name == path) {
        let qualified = format!("rqmd://{path}/");
        Ok((qualified, db::collection_context_key(path)))
    } else {
        bail!(
            "'{path}' is not a known collection and not a qualified rqmd:// path.\n\
             Add the collection first, or use the qualified form: \
             rqmd context add rqmd://{path}/ \"...\""
        )
    }
}

fn add(index_dir: &Path, path: Option<&str>, text: &str) -> Result<()> {
    let s = store::open_store_no_backend(index_dir, false)?;
    let (display_path, key) = resolve_context_target(&s, path.unwrap_or("/"))?;
    db::set_config(&s.db, &key, text)?;
    println!("Context set for '{display_path}'.");
    Ok(())
}

fn list(index_dir: &Path) -> Result<()> {
    let s = store::open_store_no_backend(index_dir, true)?;
    let rows = db::list_config_by_prefix(&s.db, CTX_PREFIX)?;

    if rows.is_empty() {
        println!("No contexts set. Run `rqmd context add [path] \"description\"` to add one.");
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
    let s = store::open_store_no_backend(index_dir, false)?;
    let (display_path, key) = resolve_context_target(&s, path)?;
    db::delete_config(&s.db, &key)?;
    println!("Removed context for '{display_path}'.");
    Ok(())
}

fn check(index_dir: &Path) -> Result<()> {
    let s = store::open_store_no_backend(index_dir, true)?;
    let cols = db::list_collections(&s.db)?;
    let mut missing = 0usize;
    for col in &cols {
        let key = db::collection_context_key(&col.name);
        if db::get_config(&s.db, &key)?.is_none() {
            println!("MISSING context for collection '{}'", col.name);
            println!(
                "  Run: rqmd context add rqmd://{}/ \"<description>\"",
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

#[cfg(test)]
mod tests {
    use super::*;
    use rqmd_core::types::Collection;

    fn test_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = store::open_store_no_backend(dir.path(), false).expect("open store");
        (dir, s)
    }

    fn add_collection(s: &Store, name: &str) {
        db::upsert_collection(
            &s.db,
            &Collection {
                name: name.to_string(),
                path: format!("/tmp/{name}"),
                pattern: "**/*.md".to_string(),
                ignore: vec![],
                include_by_default: true,
                update_command: None,
                allow_hidden: false,
            },
        )
        .expect("upsert collection");
    }

    #[test]
    fn resolve_root_path_is_unqualified() {
        let (_dir, s) = test_store();
        let (display, key) = resolve_context_target(&s, "/").unwrap();
        assert_eq!(display, "/");
        assert_eq!(key, "context:/");
    }

    #[test]
    fn resolve_qualified_rqmd_path_passes_through() {
        let (_dir, s) = test_store();
        let (display, key) = resolve_context_target(&s, "rqmd://notes/").unwrap();
        assert_eq!(display, "rqmd://notes/");
        assert_eq!(key, "context:rqmd://notes/");
    }

    #[test]
    fn resolve_bare_known_collection_name_qualifies() {
        let (_dir, s) = test_store();
        add_collection(&s, "notes");
        let (display, key) = resolve_context_target(&s, "notes").unwrap();
        assert_eq!(display, "rqmd://notes/");
        assert_eq!(key, db::collection_context_key("notes"));
    }

    #[test]
    fn resolve_bare_unknown_name_is_rejected() {
        let (_dir, s) = test_store();
        let err = resolve_context_target(&s, "nope").unwrap_err();
        assert!(err.to_string().contains("not a known collection"));
    }

    #[test]
    fn add_then_check_reports_collection_covered() {
        let (_dir, s) = test_store();
        add_collection(&s, "notes");
        let (_, key) = resolve_context_target(&s, "notes").unwrap();
        db::set_config(&s.db, &key, "some context").unwrap();
        assert_eq!(
            db::get_config(&s.db, &db::collection_context_key("notes")).unwrap(),
            Some("some context".to_string())
        );
    }
}

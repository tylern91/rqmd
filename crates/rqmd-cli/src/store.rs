use anyhow::{Context, Result};
use rqmd_core::{db, store as core_store, Store, StoreConfig};
use rqmd_llm::{create_backend, no_backend, BackendKind};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::format::Format;

/// Resolve the index directory:
///   1. `--index-dir` flag / `RQMD_INDEX_DIR` env
///   2. `.rqmd/` in the current directory (project-local)
///   3. `~/.cache/rqmd/` (global default)
pub fn resolve_index_dir(override_path: Option<&str>) -> Result<PathBuf> {
    if let Some(p) = override_path {
        return Ok(PathBuf::from(p));
    }

    // Project-local .rqmd/ takes precedence over global
    let local = PathBuf::from(".rqmd");
    if local.join("index.sqlite").exists() {
        return Ok(local);
    }

    // Global default
    let home = dirs::cache_dir()
        .or_else(dirs::home_dir)
        .context("cannot determine home directory")?;
    Ok(home.join("rqmd"))
}

/// `read_only` opens the HNSW index as a memory-mapped view instead of
/// loading it fully into RAM. Pass `true` for query-only callers (search,
/// get, status); `false` for callers that index (embed, update, collection
/// add) — a read-only store rejects `add`/`add_with_vid`/`save`.
pub fn store_config(index_dir: &Path, read_only: bool) -> StoreConfig {
    std::fs::create_dir_all(index_dir).ok();
    StoreConfig {
        db_path: index_dir.join("index.sqlite"),
        tantivy_dir: index_dir.join("tantivy"),
        hnsw_path: index_dir.join("hnsw.usearch"),
        read_only,
    }
}

/// Open a store without the inference backend (for FTS-only commands).
pub fn open_store_no_backend(index_dir: &Path, read_only: bool) -> Result<Store> {
    Store::open(store_config(index_dir, read_only), no_backend())
}

/// Open a store with the inference backend selected by `RQMD_INFERENCE_BACKEND`
/// (or the provided override). Downloads models on first run.
pub fn open_store_with_backend(index_dir: &Path, read_only: bool) -> Result<Store> {
    open_store_with_backend_kind(index_dir, &BackendKind::from_env(), read_only)
}

/// Open a store with an explicit backend kind (used when CLI flags override env).
pub fn open_store_with_backend_kind(
    index_dir: &Path,
    kind: &BackendKind,
    read_only: bool,
) -> Result<Store> {
    let backend = create_backend(kind).context("failed to initialize inference backend")?;
    Store::open(store_config(index_dir, read_only), backend)
}

/// The `embed_fingerprint` the currently *configured* backend (per
/// `RQMD_INFERENCE_BACKEND`) + chunking constants would produce. Compare
/// against `db::fingerprint_breakdown` rows to detect stale vectors — shared
/// by `doctor`, `embed`, `query`, and `vsearch` so there is exactly one
/// staleness detection path.
///
/// Derived from `BackendKind::default_embed_model_name()`, not a hardcoded
/// `LlamaCppConfig::default()` — otherwise this permanently disagrees with the
/// real fingerprint whenever a non-default backend (e.g. ORT) is active.
pub fn expected_fingerprint() -> String {
    let name = BackendKind::from_env().default_embed_model_name();
    core_store::expected_embed_fingerprint(&name)
}

/// Warn once if any content_vectors row was produced by a different model or
/// chunking config than the one active now. A single stale fingerprint (no
/// mixing) is exactly what upgrading past a chunking/model change looks like
/// before the next `embed --rebuild` — checking `breakdown.len() > 1` alone
/// would miss it.
pub fn warn_if_fingerprint_stale(s: &Store) {
    let expected = expected_fingerprint();
    let breakdown = db::fingerprint_breakdown(&s.db).unwrap_or_default();
    if breakdown.iter().any(|(fp, _)| fp != &expected) {
        eprintln!(
            "\x1b[33mrqmd: warning: embeddings are stale (model or chunking config changed \
             since last embed) — run `rqmd embed --rebuild` to refresh\x1b[0m"
        );
    }
}

/// Collection name → root filesystem path, needed to resolve a document's
/// real absolute path for `--format files`. Only worth a DB round-trip when
/// the chosen format actually needs it.
pub fn collection_roots(s: &Store, format: Format) -> Result<HashMap<String, String>> {
    if format != Format::Files {
        return Ok(HashMap::new());
    }
    Ok(db::list_collections(&s.db)?
        .into_iter()
        .map(|c| (c.name, c.path))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rqmd_core::Collection;
    use tempfile::TempDir;

    #[test]
    fn resolve_index_dir_prefers_override_over_any_default() {
        // The override branch must return immediately — it must never touch
        // the current directory or the environment, both of which are
        // process-global state shared with every other test in the binary.
        let resolved = resolve_index_dir(Some("/tmp/some/explicit/dir")).unwrap();
        assert_eq!(resolved, PathBuf::from("/tmp/some/explicit/dir"));
    }

    #[test]
    fn store_config_joins_paths_under_index_dir_and_threads_read_only() {
        let tmp = TempDir::new().unwrap();
        let index_dir = tmp.path().join("idx");

        let cfg = store_config(&index_dir, true);

        assert_eq!(cfg.db_path, index_dir.join("index.sqlite"));
        assert_eq!(cfg.tantivy_dir, index_dir.join("tantivy"));
        assert_eq!(cfg.hnsw_path, index_dir.join("hnsw.usearch"));
        assert!(cfg.read_only);
        // Callers open the Store immediately afterward and expect the
        // directory to already exist.
        assert!(index_dir.is_dir());
    }

    #[test]
    fn store_config_threads_read_only_false() {
        let tmp = TempDir::new().unwrap();
        assert!(!store_config(tmp.path(), false).read_only);
    }

    #[test]
    fn collection_roots_short_circuits_for_non_files_formats() {
        let tmp = TempDir::new().unwrap();
        let store = open_store_no_backend(tmp.path(), false).unwrap();
        // Every format other than `Files` must skip the db round-trip
        // entirely — asserting emptiness here on a store with zero
        // collections wouldn't distinguish "short-circuited" from "queried
        // and found nothing", so the meaningful case is 4b below.
        let roots = collection_roots(&store, Format::Json).unwrap();
        assert!(roots.is_empty());
    }

    #[test]
    fn collection_roots_maps_name_to_path_for_files_format() {
        let tmp = TempDir::new().unwrap();
        let store = open_store_no_backend(tmp.path(), false).unwrap();
        db::upsert_collection(
            &store.db,
            &Collection {
                name: "notes".to_string(),
                path: "/repo/notes".to_string(),
                pattern: "**/*.md".to_string(),
                ignore: vec![],
                include_by_default: true,
                update_command: None,
                allow_hidden: false,
            },
        )
        .unwrap();

        let roots = collection_roots(&store, Format::Files).unwrap();

        assert_eq!(roots.get("notes").map(String::as_str), Some("/repo/notes"));
    }
}

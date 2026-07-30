use anyhow::{Context, Result};
use rqmd_core::{Store, StoreConfig};
use rqmd_llm::{create_backend, no_backend, BackendKind};
use std::path::{Path, PathBuf};

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

use anyhow::Result;
use std::path::Path;

use crate::format::Format;
use crate::{format, store};

#[allow(clippy::too_many_arguments)]
pub fn run_query(
    index_dir: &Path,
    query: &str,
    intent: Option<&str>,
    collection: Option<&str>,
    num: usize,
    fmt: Format,
    no_rerank: bool,
    full: bool,
    no_expand: bool,
) -> Result<()> {
    let mut s = store::open_store_with_backend(index_dir, true)?;
    store::warn_if_fingerprint_stale(&s);
    let results = s.hybrid_query(query, intent, num, collection, no_rerank, no_expand)?;
    let roots = store::collection_roots(&s, fmt)?;
    format::print_results(&results, fmt, full, query, &roots);
    Ok(())
}

pub fn run_search(
    index_dir: &Path,
    query: &str,
    collection: Option<&str>,
    num: usize,
    fmt: Format,
    full: bool,
) -> Result<()> {
    let s = store::open_store_no_backend(index_dir, true)?;
    let results = s.search_fts(query, num, collection)?;
    let roots = store::collection_roots(&s, fmt)?;
    format::print_results(&results, fmt, full, query, &roots);
    Ok(())
}

pub fn run_vsearch(
    index_dir: &Path,
    query: &str,
    collection: Option<&str>,
    num: usize,
    fmt: Format,
    full: bool,
) -> Result<()> {
    let mut s = store::open_store_with_backend(index_dir, true)?;
    store::warn_if_fingerprint_stale(&s);
    let results = s.search_vec(query, num, collection)?;
    let roots = store::collection_roots(&s, fmt)?;
    format::print_results(&results, fmt, full, query, &roots);
    Ok(())
}

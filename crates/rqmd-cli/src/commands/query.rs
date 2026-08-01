use anyhow::Result;
use std::path::Path;

use crate::format::Format;
use crate::{format, store};

#[allow(clippy::too_many_arguments)]
pub fn run_query(
    index_dir: &Path,
    query: &str,
    intent: Option<&str>,
    collections: Option<&[String]>,
    num: usize,
    fmt: Format,
    no_rerank: bool,
    full: bool,
    no_expand: bool,
) -> Result<()> {
    let mut s = store::open_store_with_backend(index_dir, true)?;
    store::warn_if_fingerprint_stale(&s);
    let results = s.hybrid_query_multi(query, intent, num, collections, no_rerank, no_expand)?;
    let roots = store::collection_roots(&s, fmt)?;
    format::print_results(&results, fmt, full, query, &roots);
    Ok(())
}

pub fn run_search(
    index_dir: &Path,
    query: &str,
    collections: Option<&[String]>,
    num: usize,
    fmt: Format,
    full: bool,
) -> Result<()> {
    let s = store::open_store_no_backend(index_dir, true)?;
    let results = s.search_fts_multi(query, num, collections)?;
    let roots = store::collection_roots(&s, fmt)?;
    format::print_results(&results, fmt, full, query, &roots);
    Ok(())
}

pub fn run_vsearch(
    index_dir: &Path,
    query: &str,
    collections: Option<&[String]>,
    num: usize,
    fmt: Format,
    full: bool,
) -> Result<()> {
    let mut s = store::open_store_with_backend(index_dir, true)?;
    store::warn_if_fingerprint_stale(&s);
    let results = s.search_vec_multi(query, num, collections)?;
    let roots = store::collection_roots(&s, fmt)?;
    format::print_results(&results, fmt, full, query, &roots);
    Ok(())
}

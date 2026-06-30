use anyhow::Result;
use std::path::Path;

use crate::{format, store};

#[allow(clippy::too_many_arguments)]
pub fn run_query(
    index_dir: &Path,
    query: &str,
    intent: Option<&str>,
    collection: Option<&str>,
    num: usize,
    fmt: &str,
    no_rerank: bool,
    full: bool,
) -> Result<()> {
    let mut s = store::open_store_with_backend(index_dir)?;
    let results = s.hybrid_query(query, intent, num, collection, no_rerank)?;
    format::print_results(&results, fmt, full, query);
    Ok(())
}

pub fn run_search(
    index_dir: &Path,
    query: &str,
    collection: Option<&str>,
    num: usize,
    fmt: &str,
    full: bool,
) -> Result<()> {
    let s = store::open_store_no_backend(index_dir)?;
    let results = s.search_fts(query, num, collection)?;
    format::print_results(&results, fmt, full, query);
    Ok(())
}

pub fn run_vsearch(
    index_dir: &Path,
    query: &str,
    collection: Option<&str>,
    num: usize,
    fmt: &str,
    full: bool,
) -> Result<()> {
    let mut s = store::open_store_with_backend(index_dir)?;
    let results = s.search_vec(query, num, collection)?;
    format::print_results(&results, fmt, full, query);
    Ok(())
}

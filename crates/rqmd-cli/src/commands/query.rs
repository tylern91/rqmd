use anyhow::Result;
use rqmd_core::{SearchResult, Store};
use std::path::Path;

use crate::format::Format;
use crate::{format, store};

/// Open a store, run one `search` step against it, and print the results —
/// the shape shared by `run_query`, `run_search`, and `run_vsearch`. Only the
/// store-opening mode (backend needed or not, fingerprint staleness worth
/// warning about) and the search step itself differ between them.
fn run_search_command(
    index_dir: &Path,
    query: &str,
    fmt: Format,
    full: bool,
    with_backend: bool,
    search: impl FnOnce(&mut Store) -> Result<Vec<SearchResult>>,
) -> Result<()> {
    let mut s = if with_backend {
        let s = store::open_store_with_backend(index_dir, true)?;
        store::warn_if_fingerprint_stale(&s);
        s
    } else {
        store::open_store_no_backend(index_dir, true)?
    };
    let results = search(&mut s)?;
    let roots = store::collection_roots(&s, fmt)?;
    format::print_results(&results, fmt, full, query, &roots);
    Ok(())
}

/// Options for `run_query` beyond the store location and the query text
/// itself — bundled so the function stays under clippy's argument-count
/// lint instead of carrying all nine as positional parameters.
pub struct QueryOptions<'a> {
    pub intent: Option<&'a str>,
    pub collections: Option<&'a [String]>,
    pub num: usize,
    pub fmt: Format,
    pub no_rerank: bool,
    pub full: bool,
    pub no_expand: bool,
}

pub fn run_query(index_dir: &Path, query: &str, opts: QueryOptions) -> Result<()> {
    run_search_command(index_dir, query, opts.fmt, opts.full, true, |s| {
        s.hybrid_query_multi(
            query,
            opts.intent,
            opts.num,
            opts.collections,
            opts.no_rerank,
            opts.no_expand,
        )
    })
}

pub fn run_search(
    index_dir: &Path,
    query: &str,
    collections: Option<&[String]>,
    num: usize,
    fmt: Format,
    full: bool,
) -> Result<()> {
    run_search_command(index_dir, query, fmt, full, false, |s| {
        s.search_fts_multi(query, num, collections)
    })
}

pub fn run_vsearch(
    index_dir: &Path,
    query: &str,
    collections: Option<&[String]>,
    num: usize,
    fmt: Format,
    full: bool,
) -> Result<()> {
    run_search_command(index_dir, query, fmt, full, true, |s| {
        s.search_vec_multi(query, num, collections)
    })
}

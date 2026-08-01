use anyhow::{Context, Result};
use std::path::Path;

use rqmd_core::db;

use super::get::PathSpec;
use crate::format::Format;
use crate::{format, store};

/// `rqmd similar <path|#docid>` — find documents most similar to an
/// already-indexed one. Uses `open_store_no_backend` since every chunk vector
/// needed already lives in the HNSW index; no model load is required.
pub fn run_similar(index_dir: &Path, ref_arg: &str, num: usize, fmt: Format) -> Result<()> {
    let s = store::open_store_no_backend(index_dir, true)?;

    let spec = PathSpec::parse(ref_arg).with_context(|| format!("cannot parse path: {ref_arg}"))?;

    let doc = if spec.is_docid() {
        let docid = spec.docid_hex();
        db::get_document_by_docid_prefix(&s.db, docid)?
            .with_context(|| format!("no document found with docid #{docid}"))?
    } else {
        db::get_document_by_filepath(&s.db, &spec.collection, &spec.path)?
            .with_context(|| format!("not found: {ref_arg}"))?
    };

    let results = s.similar_to_hash(&doc.hash, &doc.collection, &doc.path, num)?;
    let roots = store::collection_roots(&s, fmt)?;
    format::print_results(&results, fmt, false, "", &roots);
    Ok(())
}

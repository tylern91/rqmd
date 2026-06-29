//! Phase 1 parity gate — hybrid_query eval on eval-deep-research corpus.
//!
//! Usage:
//!   cargo run --example eval -p qmd-core
//!
//! Downloads embeddinggemma-300M + qwen3-reranker-0.6B on first run (~500 MB combined).
//! Expected top-1 accuracy: ≥40% on these "hard" queries (no keyword overlap).
//! The TS baseline with query expansion reaches ~70%; Phase 1 without expansion is lower.

use anyhow::Result;
use rqmd_core::{Store, StoreConfig};
use rqmd_llm::{LlamaCppBackend, LlamaCppConfig};
use serde::Deserialize;
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct EvalCase {
    query: String,
    expected_doc: String,
    difficulty: String,
    intent: String,
    notes: String,
}

fn eval_docs_dir() -> PathBuf {
    // qmd-core/examples/ → qmd-core/ → rust/ → qmd/ → test/eval-docs/
    let manifest = env!("CARGO_MANIFEST_DIR");
    PathBuf::from(manifest)
        .join("../../test/eval-docs")
        .canonicalize()
        .expect("eval-docs directory not found — run from the qmd workspace root")
}

fn eval_jsonl_path() -> PathBuf {
    let manifest = env!("CARGO_MANIFEST_DIR");
    PathBuf::from(manifest)
        .join("../../test/eval-deep-research.jsonl")
        .canonicalize()
        .expect("eval-deep-research.jsonl not found")
}

fn load_eval_cases(path: &Path) -> Result<Vec<EvalCase>> {
    let content = fs::read_to_string(path)?;
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| Ok(serde_json::from_str(line)?))
        .collect()
}

fn result_matches(result_path: &str, expected_doc: &str) -> bool {
    // result_path is like "eval/distributed-systems-overview.md"
    // expected_doc is like "distributed-systems"
    result_path.contains(expected_doc)
}

fn main() -> Result<()> {
    let docs_dir = eval_docs_dir();
    let jsonl_path = eval_jsonl_path();

    println!("=== qmd-core Phase 1 Eval ===");
    println!("Docs: {}", docs_dir.display());
    println!("Queries: {}", jsonl_path.display());
    println!();

    // ── Set up a temporary store directory ───────────────────────────────────
    let store_dir = std::env::temp_dir().join("qmd-eval-store");
    if store_dir.exists() {
        fs::remove_dir_all(&store_dir)?;
    }
    fs::create_dir_all(&store_dir)?;

    let config = StoreConfig {
        db_path: store_dir.join("index.sqlite"),
        tantivy_dir: store_dir.join("tantivy"),
        hnsw_path: store_dir.join("hnsw.usearch"),
    };

    // ── Initialize backend (downloads models on first run) ───────────────────
    println!("Loading inference backend (models download on first run)...");
    let backend = LlamaCppBackend::new(LlamaCppConfig::default())?;
    println!("Backend ready.\n");

    let mut store = Store::open(config, Box::new(backend))?;

    // ── Index all eval docs ───────────────────────────────────────────────────
    let mut indexed = 0usize;
    for entry in fs::read_dir(&docs_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let filename = path.file_name().unwrap().to_string_lossy().to_string();
        let body = fs::read_to_string(&path)?;
        // Title: first line stripped of leading `# `
        let title = body
            .lines()
            .next()
            .unwrap_or(&filename)
            .trim_start_matches('#')
            .trim()
            .to_string();

        print!("  Indexing {} ... ", filename);
        store.index_document("eval", &filename, &title, &body)?;
        println!("done");
        indexed += 1;
    }
    store.flush()?;
    println!("\nIndexed {indexed} documents.\n");

    // ── Load eval cases ───────────────────────────────────────────────────────
    let cases = load_eval_cases(&jsonl_path)?;
    println!("Running {} queries...\n", cases.len());

    let mut hits = 0usize;
    let mut fts_only_hits = 0usize;
    let mut vec_only_hits = 0usize;

    for (i, case) in cases.iter().enumerate() {
        // Full hybrid
        let results = store.hybrid_query(&case.query, 5, None, false)?;
        let top1 = results.first().map(|r| r.path.as_str()).unwrap_or("");
        let matched = result_matches(top1, &case.expected_doc);

        // BM25 only
        let fts_results = store.search_fts(&case.query, 5, None)?;
        let fts_top1 = fts_results.first().map(|r| r.path.as_str()).unwrap_or("");
        let fts_matched = result_matches(fts_top1, &case.expected_doc);

        // Vector only
        let vec_results = store.search_vec(&case.query, 5, None)?;
        let vec_top1 = vec_results.first().map(|r| r.path.as_str()).unwrap_or("");
        let vec_matched = result_matches(vec_top1, &case.expected_doc);

        if matched {
            hits += 1;
        }
        if fts_matched {
            fts_only_hits += 1;
        }
        if vec_matched {
            vec_only_hits += 1;
        }

        let indicator = if matched { "✓" } else { "✗" };
        println!(
            "[{i:02}] {indicator} hybrid={} fts={} vec={}",
            if matched { "HIT" } else { "miss" },
            if fts_matched { "hit" } else { "miss" },
            if vec_matched { "hit" } else { "miss" },
        );
        if !matched {
            println!("      query:    {}", case.query);
            println!("      expected: {}", case.expected_doc);
            println!("      got:      {top1}");
        }
    }

    let n = cases.len();
    println!("\n{}", "=".repeat(40));
    println!("Results:");
    println!(
        "  Hybrid (BM25+vec+rerank): {hits}/{n} ({:.0}%)",
        100.0 * hits as f64 / n as f64
    );
    println!(
        "  BM25 only:               {fts_only_hits}/{n} ({:.0}%)",
        100.0 * fts_only_hits as f64 / n as f64
    );
    println!(
        "  Vector only:             {vec_only_hits}/{n} ({:.0}%)",
        100.0 * vec_only_hits as f64 / n as f64
    );

    if hits * 100 / n >= 40 {
        println!("\nPASS — Phase 1 parity gate met (≥40% top-1)");
    } else {
        println!("\nFAIL — below Phase 1 gate ({hits}/{n})");
        std::process::exit(1);
    }

    Ok(())
}

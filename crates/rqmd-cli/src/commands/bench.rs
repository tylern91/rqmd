use anyhow::Result;
use rqmd_core::Store;
use rqmd_llm::{create_backend, BackendKind, InferenceBackend};
use std::path::Path;
use std::time::Instant;

use crate::store::store_config;

// Sample corpus for embedding throughput benchmark — diverse topics and lengths.
const BENCH_TEXTS: &[&str] = &[
    "Rust's ownership model eliminates entire classes of memory bugs at compile time.",
    "Machine learning inference on Apple Silicon benefits from the unified memory architecture.",
    "Reciprocal Rank Fusion combines multiple ranked lists without score normalization.",
    "SQLite FTS5 supports BM25 ranking with configurable per-column weights.",
    "The CoreML execution provider routes operations to the Apple Neural Engine.",
    "ONNX Runtime supports dynamic shapes for variable-length sequence inputs.",
    "Tokenizers from HuggingFace provide fast WordPiece and BPE tokenization.",
    "Cross-encoder rerankers score query-document pairs jointly for high-quality ranking.",
    "Vector similarity search over HNSW graphs provides sub-linear query time.",
    "Mean pooling over the last hidden state produces sentence-level embeddings.",
];

// Diverse queries for query-latency measurement — independent of the eval-docs fixture corpus.
const BENCH_QUERIES: &[&str] = &[
    "API versioning strategies",
    "consistency vs availability tradeoffs",
    "how to prevent models from memorizing training data",
    "what is a service mesh and when to use it",
    "remote work productivity and async communication",
    "Rust ownership and borrowing explained",
    "machine learning inference optimization on GPU",
    "reciprocal rank fusion for hybrid search",
    "vector similarity search HNSW index",
    "BM25 ranking with per-field boost weights",
    "observability metrics traces logs",
    "database sharding strategies horizontal scaling",
];

/// Run the embedding throughput benchmark followed by an optional query-latency phase.
///
/// The LlamaBackend singleton is initialised once and threaded through both phases so
/// the vec/hybrid query-latency pass doesn't try to re-initialise it (which would error).
pub fn run_bench(index_dir: &Path, n_rounds: usize) -> Result<()> {
    // ── Phase 1: Embedding throughput ─────────────────────────────────────────────
    let kind = BackendKind::from_env();
    eprintln!("Backend: {kind:?}");
    eprintln!(
        "Batch size: {} texts × {n_rounds} rounds",
        BENCH_TEXTS.len()
    );

    // Create the backend once — the Llama global singleton is initialised here.
    let mut backend = create_backend(&kind)?;

    eprintln!("Warming up...");
    backend.embed_batch(BENCH_TEXTS)?;

    eprintln!("Benchmarking embed throughput...");
    let t0 = Instant::now();
    for _ in 0..n_rounds {
        backend.embed_batch(BENCH_TEXTS)?;
    }
    let elapsed = t0.elapsed();

    let total_texts = BENCH_TEXTS.len() * n_rounds;
    let texts_per_sec = total_texts as f64 / elapsed.as_secs_f64();
    let ms_per_text = elapsed.as_secs_f64() * 1000.0 / total_texts as f64;

    println!("─────────────────────────────────────────");
    println!("  Backend:        {kind:?}");
    println!("  Total texts:    {total_texts}");
    println!("  Wall time:      {:.2}s", elapsed.as_secs_f64());
    println!("  Throughput:     {texts_per_sec:.1} texts/sec");
    println!("  Latency/text:   {ms_per_text:.2} ms");
    println!("─────────────────────────────────────────");
    println!("Tip: compare backends with RRQMD_INFERENCE_BACKEND=ort RRQMD_ORT_EP=coreml rqmd bench");

    // ── Phase 2: Query latency (requires a real index) ───────────────────────────
    if index_dir.join("index.sqlite").exists() {
        // Pass the already-initialised backend so the store reuses it without
        // attempting a second LlamaBackend::new() call.
        run_query_latency_bench(index_dir, n_rounds, backend)?;
    } else {
        eprintln!(
            "\n(Skipping query-latency: no index at {})",
            index_dir.display()
        );
        eprintln!("  Run `rqmd embed` first, then re-run bench --index-dir <path>");
    }

    Ok(())
}

/// Compute the p-th percentile (0–100) of a pre-sorted slice of microsecond timings.
fn percentile(sorted: &[u128], p: f64) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 * p / 100.0).ceil() as usize).saturating_sub(1);
    sorted[idx.min(sorted.len() - 1)]
}

fn run_query_latency_bench(
    index_dir: &Path,
    n_rounds: usize,
    backend: Box<dyn InferenceBackend>,
) -> Result<()> {
    let n_q = BENCH_QUERIES.len();
    eprintln!("\n── Query-latency benchmark ({n_q} queries × {n_rounds} rounds) ──");
    eprintln!("Loading index (one store for all modes)...");

    // Open ONE store with the already-initialised backend.  All three modes
    // (BM25, vec, hybrid) run on this single instance — no second HNSW load.
    // search_fts takes &self so it works fine on a mut store.
    let mut store = Store::open(store_config(index_dir), backend)?;

    // ── BM25 ─────────────────────────────────────────────────────────────────
    eprintln!("Mode: BM25...");
    for q in BENCH_QUERIES {
        store.search_fts(q, 10, None)?;
    }
    let mut bm25_timings: Vec<u128> = Vec::with_capacity(n_q * n_rounds);
    for _ in 0..n_rounds {
        for q in BENCH_QUERIES {
            let t = Instant::now();
            store.search_fts(q, 10, None)?;
            bm25_timings.push(t.elapsed().as_micros());
        }
    }
    bm25_timings.sort_unstable();
    println!(
        "  BM25:             p50 = {:>6}µs   p99 = {:>6}µs   (n={})",
        percentile(&bm25_timings, 50.0),
        percentile(&bm25_timings, 99.0),
        bm25_timings.len(),
    );

    // ── Vec ──────────────────────────────────────────────────────────────────
    eprintln!("Mode: Vec...");
    for q in &BENCH_QUERIES[..3] {
        store.search_vec(q, 10, None)?;
    }
    let mut vec_timings: Vec<u128> = Vec::with_capacity(n_q * n_rounds);
    for _ in 0..n_rounds {
        for q in BENCH_QUERIES {
            let t = Instant::now();
            store.search_vec(q, 10, None)?;
            vec_timings.push(t.elapsed().as_micros());
        }
    }
    vec_timings.sort_unstable();
    println!(
        "  Vec:              p50 = {:>6}µs   p99 = {:>6}µs   (n={})",
        percentile(&vec_timings, 50.0),
        percentile(&vec_timings, 99.0),
        vec_timings.len(),
    );

    // ── Hybrid (no rerank) ───────────────────────────────────────────────────
    eprintln!("Mode: Hybrid (no rerank)...");
    let mut hybrid_nr_timings: Vec<u128> = Vec::with_capacity(n_q * n_rounds);
    for _ in 0..n_rounds {
        for q in BENCH_QUERIES {
            let t = Instant::now();
            store.hybrid_query(q, None, 10, None, true)?;
            hybrid_nr_timings.push(t.elapsed().as_micros());
        }
    }
    hybrid_nr_timings.sort_unstable();
    println!(
        "  Hybrid (no-rr):   p50 = {:>6}µs   p99 = {:>6}µs   (n={})",
        percentile(&hybrid_nr_timings, 50.0),
        percentile(&hybrid_nr_timings, 99.0),
        hybrid_nr_timings.len(),
    );

    // ── Hybrid (with rerank) ─────────────────────────────────────────────────
    eprintln!("Mode: Hybrid (with rerank)...");
    let mut hybrid_rr_timings: Vec<u128> = Vec::with_capacity(n_q * n_rounds);
    for _ in 0..n_rounds {
        for q in BENCH_QUERIES {
            let t = Instant::now();
            store.hybrid_query(q, None, 10, None, false)?;
            hybrid_rr_timings.push(t.elapsed().as_micros());
        }
    }
    hybrid_rr_timings.sort_unstable();
    println!(
        "  Hybrid (rerank):  p50 = {:>6}µs   p99 = {:>6}µs   (n={})",
        percentile(&hybrid_rr_timings, 50.0),
        percentile(&hybrid_rr_timings, 99.0),
        hybrid_rr_timings.len(),
    );

    println!("─────────────────────────────────────────");
    println!("Note: timings are warm in-process (no model-load overhead).");

    Ok(())
}

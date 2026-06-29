use anyhow::Result;
use rqmd_llm::{create_backend, BackendKind};
use std::path::Path;
use std::time::Instant;

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

/// Run an embedding benchmark: warm-up then measure n_rounds of embed_batch.
pub fn run_bench(_index_dir: &Path, n_rounds: usize) -> Result<()> {
    let kind = BackendKind::from_env();
    eprintln!("Backend: {kind:?}");
    eprintln!(
        "Batch size: {} texts × {n_rounds} rounds",
        BENCH_TEXTS.len()
    );

    let mut backend = create_backend(&kind)?;

    // Warm-up: one round to compile CoreML graphs / load GPU kernels
    eprintln!("Warming up...");
    backend.embed_batch(BENCH_TEXTS)?;

    // Timed benchmark
    eprintln!("Benchmarking...");
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
    println!("Tip: compare backends with RQMD_INFERENCE_BACKEND=ort RQMD_ORT_EP=coreml qmd bench");

    Ok(())
}

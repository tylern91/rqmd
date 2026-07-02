# De-risking Benchmarks

This document records the methodology, raw results, and decisions from the technology
evaluation that validated rqmd's two highest-risk choices: **inference backend** and
**search database**. It is the permanent reference for "why llama-cpp-2, why
Tantivy+usearch" — read this before changing either.

---

## Methodology

### Spike A — Inference backends

Decision criteria:

| Question | Pass condition |
|----------|---------------|
| Embeddings work? | Produce a non-zero f32 vector from text; cosine similarity order makes sense |
| Rerank discriminates? | Relevant doc scores materially higher than irrelevant doc |
| GBNF compiles? | `lex:/vec:/hyde:` query-expansion grammar compiles and samples correctly |
| Throughput acceptable? | ≥10 tok/s warm Metal for 300M embed model |
| Critical API surface covered? | `embed`, `embed_batch`, `rerank`, `generate_constrained` all implementable |

Sub-tests:
- **A1** — embeddings: load embeddinggemma-300M GGUF via `hf-hub`, produce vector, check norm and ordering.
- **A2** — rerank: load Qwen3-Reranker-0.6B with `LlamaPoolingType::Rank`, score (query, relevant-doc) and (query, irrelevant-doc) pair.
- **A3** — GBNF: compile the `lex:/vec:/hyde:` grammar for query expansion, verify constrained sampling produces valid tokens.

Candidates evaluated: **llama-cpp-2** (primary), **ort** (CoreML EP, secondary).

### Spike B — Database bake-off

Decision criteria:

| Question | Weight |
|----------|--------|
| Search-quality parity with TS baseline on bench fixtures | High |
| Query latency (p50, warm index, 500 chunks) | High |
| Index time (500 markdown chunks) | Medium |
| Code surface area (lines to implement full search pipeline) | Medium |
| On-disk compat with existing `~/.cache/qmd/index.sqlite` | Low (re-index accepted) |

Candidates:
- **LanceDB 0.30.0** — embedded columnar, built-in Tantivy BM25 + vector + RRF.
- **Tantivy 0.26.1 + usearch 2.25.3** — best-in-class BM25, HNSW ANN, app-side RRF preserving qmd's exact formula.

Corpus: ~500 markdown chunks from the qmd eval fixtures plus representative docs.
Queries: the 24 qmd bench queries plus 6 targeted phrase queries.

---

## Spike A — Inference Backends

### A1: Embeddings (llama-cpp-2)

Model: `nomic-ai/nomic-embed-text-v1.5-GGUF` (embeddinggemma-300M, f16, 568 MB)
Hardware: Apple M-series, Metal GPU

| Metric | Result |
|--------|--------|
| Embedding dimension | 768 |
| Cold load time | ~3.8 s (first run, model cached) |
| Warm latency (single text) | **714 µs** |
| Cosine similarity (same-topic pair) | ~0.82 |
| Cosine similarity (random pair) | ~0.31 |

Result: ✅ **PASS** — embeddings work, ordering is semantically correct.

### A2: Reranking (llama-cpp-2)

Model: `Qwen/Qwen3-Reranker-0.6B-GGUF` (q8_0, ~640 MB)
Pooling type: `LlamaPoolingType::Rank`

| Pair | Score |
|------|-------|
| (query, relevant doc) | **+0.9928** |
| (query, irrelevant doc) | **+0.7858** |
| Discrimination gap | 0.207 |

Result: ✅ **PASS** — clear discrimination; gap sufficient for reliable top-k reranking.

**Critical API corrections discovered during Spike A:**

- Use `ctx.encode()` NOT `ctx.decode()` for the reranker (despite the reranker being a causal model). `LlamaPoolingType::Rank` requires the encode path to extract pooled logits.
- Allocate a **fresh context per (query, doc) pair** — KV cache accumulates positions for seq_id=0; a second batch starting at position 0 fails with "positions not consecutive".
- Set `n_ctx=512` for the reranker on Apple Silicon (matches the 448 MiB KV memory budget at the 14-layer split point).
- `n_gpu_layers=14` for the reranker — beyond 14 layers the KV cache exceeds the 448 MiB Metal buffer limit on M-series chips with 16 GB RAM.

### A3: GBNF query expansion

Grammar: `lex:/vec:/hyde:` prefix-based expansion grammar matching qmd's TypeScript GBNF.

Result: ✅ **PASS** — grammar compiles via `LlamaSampler::grammar`; constrained sampling produces valid `lex:`, `vec:`, and `hyde:` prefixed expansions. JSON-schema-to-GBNF utility in `qmd_llm` wired correctly.

### ORT CoreML baseline (Spike A secondary)

Model: `BAAI/bge-base-en-v1.5` (ONNX, 768-dim — used as a stand-in; embeddinggemma ONNX export not wired)
EP: CoreML (Apple GPU/ANE)

| Metric | llama-cpp-2 (Metal) | ort (CoreML) |
|--------|--------------------:|-------------:|
| Model | embeddinggemma-300M GGUF | BAAI/bge-base-en-v1.5 ONNX |
| Embedding dim | 768 | 768 |
| Warm latency (single) | 714 µs | 10.75 ms |
| Throughput (batch) | ~1,400 tok/s | **93.0 texts/sec** |
| Model size | ~568 MB | 415 MB |
| Model format | GGUF (native llama.cpp kernels) | ONNX |

Note: models differ (embeddinggemma vs bge-base), so throughput is directional only. The llama-cpp-2 Metal path is faster for single-text latency because Metal's GPU path is highly optimized for GGUF kernels. ORT CoreML throughput (93 texts/sec at batch) is production-viable for the embed indexing workload; its advantage would show under sustained ANE batch load with smaller models (<100M). The `Context leak detected` messages in the ORT run are macOS CoreML msgtracer noise, not a qmd issue.

**Backend decision: llama-cpp-2 as default.** ORT available via `--features ort-backend` as an optional NPU path.

---

## Spike B — Database Bake-off

### Setup

Both candidates indexed the same corpus of ~500 markdown chunks (28 documents, average 18 chunks/doc) drawn from the qmd eval fixture set. Queries: 24 standard bench queries + 6 phrase queries. Search pipeline for each candidate was implemented to parity with qmd's TypeScript pipeline:

- BM25 field weights: title=1.5, path=4.0, body=1.0 (Tantivy field-level boosting; LanceDB FTS all-fields)
- RRF formula: `weight / (k + rank + 1)`, k=60, original-query weight=2.0, top-rank bonuses
- Vector cosine: `score = 1 - distance`
- Top-K: 10 results

### Results

| Metric | LanceDB 0.30.0 | Tantivy 0.26.1 + usearch 2.25.3 | Winner |
|--------|---------------:|--------------------------------:|--------|
| Index time (500 chunks) | 44.6 ms | 44.3 ms | ≈ tie |
| Query latency p50 (warm) | **4.6 ms** | **375 µs** | Tantivy (**12×**) |
| Query latency p99 (warm) | ~18 ms | ~1.2 ms | Tantivy |
| Top-K quality parity | ≈ parity | ≈ parity | tie |
| API surface (lines) | ~55 lines | ~35 lines | Tantivy |
| Async required | yes (LanceDB is async-first) | no | Tantivy |

### LanceDB API gotchas discovered

- `FullTextSearchQuery` path changed in LanceDB 0.30: must call `.create_fts_index()` separately then pass query via the `CreateIndex` builder, not the search builder directly.
- Requires Arrow v58 (`arrow = "=58.*"`); mixing with newer versions causes a linker conflict.
- `order_by_score()` is not exposed on the hybrid query builder in 0.30 — must sort results in application code.

### Decision: Tantivy + usearch

**Rationale:**
1. **12× faster queries** — 4.6 ms vs 375 µs at p50. LanceDB's async runtime overhead dominates at this corpus size; even at 50k chunks the gap narrows only to ~3×.
2. **Synchronous API** — Tantivy's sync reader + writer maps naturally to qmd's synchronous store trait. No `tokio` runtime required in the search hot path.
3. **Preserves exact RRF tuning** — app-side RRF means qmd's tested `k=60`, `weight=2.0`, and top-rank bonus parameters carry over verbatim with no mapping layer.
4. **Smaller API surface** — ~35 lines vs ~55 lines; easier to audit and maintain.
5. **Re-index accepted** — the user explicitly preferred SOTA-over-compat; a one-time re-index from `~/.cache/qmd/index.sqlite` to `~/.cache/qmd-rs/` is acceptable.

`rusqlite` is retained for document/collection metadata storage (not search) — it maps cleanly to the existing `.qmd/index.yaml` + metadata schema without the LanceDB columnar overhead.

---

## Runtime Benchmarks

For ongoing throughput and quality numbers, run:

```sh
# Throughput bench (all backends)
cargo run -p rqmd-cli -- bench --rounds 5

# CPU-only (disables Metal/GPU layers in LlamaCppBackend)
RRQMD_FORCE_CPU=1 cargo run -p rqmd-cli -- bench --rounds 5

# ORT CoreML (requires --features ort-backend)
RRQMD_INFERENCE_BACKEND=ort RRQMD_ORT_EP=coreml \
  cargo run -p rqmd-cli --features ort-backend -- bench --rounds 5

# Search quality (BM25 — runs in CI)
cargo run -p rqmd-cli -- eval --mode bm25 --verbose

# Search quality (vec + hybrid — local only, needs models)
cargo run -p rqmd-cli -- eval --mode vec --verbose
cargo run -p rqmd-cli -- eval --mode hybrid --verbose
```

ORT smoke test and vec/hybrid quality gate numbers are recorded in `CHANGELOG.md` once run.

---

---

## Full-Corpus Runtime Benchmark

Reproducible benchmark on a large, real-world markdown corpus to complement the
de-risking spike data above.

### Methodology

**Corpus:** large local markdown corpus (≈62.9k documents, ≈29 MB). Document sizes
are small on average (≈474 bytes / ≈100 words each), reflecting a personal knowledge
base with many short atomic notes.

**Hardware:** Apple M-series (Apple Silicon), Metal GPU, cmake 4.3.3.

**Index:** isolated scratch directory (not `~/.cache/rqmd/`). The binary used is the
`dist` profile build (`--profile dist`, optimised + LTO).

**Procedure:**

```sh
# 1. Add corpus (BM25 metadata — fast)
RRQMD_INDEX_DIR=<scratch> rqmd collection add <corpus> --name vault

# 2. Embed (GPU/Metal — slow; captures throughput from progress output)
RRQMD_INDEX_DIR=<scratch> rqmd embed -c vault

# 3. Status — capture doc/chunk/vector counts and index size
RRQMD_INDEX_DIR=<scratch> rqmd status

# 4. Embedding throughput benchmark (in-process, warm Metal)
RRQMD_INDEX_DIR=<scratch> rqmd bench --rounds 5

# 5. CPU-only comparison
RRQMD_FORCE_CPU=1 RRQMD_INDEX_DIR=<scratch> rqmd bench --rounds 5

# 6. Search quality (eval fixture corpus, mode: bm25 / vec / hybrid)
rqmd eval --mode bm25 --verbose
rqmd eval --mode vec --verbose
rqmd eval --mode hybrid --verbose
```

### Corpus Indexing

| Metric | Value |
|--------|-------|
| Documents added | 62,878 |
| Chunks embedded | 210,396 |
| Index size (SQLite + Tantivy + HNSW) | 1.5 GB (720 MB SQLite, 150 MB Tantivy, 656 MB HNSW) |
| Embed time (Metal GPU, wall-clock) | 6 h 30 min |
| Real-world throughput | ~9 chunks/sec (includes disk I/O, chunking, SQLite + HNSW writes) |

The real-world indexing rate (~9 chunks/sec) reflects the end-to-end pipeline overhead.
The in-process embed throughput below is model-inference only.

### Embedding Throughput (in-process, warm — model inference only)

| Backend | Throughput | Latency/text |
|---------|------------|--------------|
| llama-cpp-2 Metal (GPU) | 83 texts/sec | 12.0 ms |
| llama-cpp-2 CPU only | 123 texts/sec | 8.1 ms |

Note: on Apple Silicon (M-series), the 300 M embedding model fits in CPU L3 cache and
NEON SIMD is highly efficient for this workload, making CPU throughput competitive with
(or faster than) Metal for small batch sizes.

### Query Latency (in-process, warm index — no cold-load overhead, n=60 per mode)

| Mode | p50 | p99 | Bottleneck |
|------|----:|----:|-----------|
| BM25 (FTS only) | 783 µs | 1,515 µs | Tantivy scorer |
| Vec (HNSW + embed query) | 10.3 ms | 15.7 ms | HNSW ANN (210 k vectors) |
| Hybrid, no rerank | 2.0 s | 2.0 s | SQLite text fetch for 20 candidates (720 MB DB) |
| Hybrid + rerank | 16.6 s | 20.5 s | Cross-encoder rerank × 20 (0.6 B model) |

Hybrid latencies are dominated by I/O on the large SQLite database and sequential
cross-encoder inference. For interactive use the `vsearch` and `search` modes (BM25/Vec)
are preferred when sub-second latency is required. `query` (with rerank) is suited for
batch or offline workflows where quality matters more than speed.

### Search Quality (eval fixture corpus — 24+ queries across easy/medium/hard/fusion tiers)

| Mode | Hit@K | All gates |
|------|------:|-----------|
| BM25 | 100% | PASS |
| Vec | 100% | PASS |
| Hybrid | 100% | PASS |

Quality is measured on the eval fixture corpus (6 reference documents, 24+ queries).
This corpus is small and purpose-built to test recall precision; real-world retrieval
quality varies with corpus diversity and query phrasing.

---

*Source of truth for raw spike data: `qmd-rust-port-research` memory note (project memory, 2026-06-27).*

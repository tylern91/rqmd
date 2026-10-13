# Performance and quality adoption pass (2026-10-13)

[← Research notes](README.md)

A survey of indexing, search and embedding ideas, the changes that came out of it, and a measured comparison of the current embedding model with EmbeddingGemma-2. Claims are labelled **Fact** (measured or read in this repository), **Inference** or **Open question**.

## What shipped

| Change | Version | Effect | Label |
|--------|---------|--------|-------|
| Skip files whose size and mtime are unchanged (`rqmd update`; `--full` forces a re-hash) | 0.24.0 | An update of 20,000 unchanged files took 0.87 s instead of 2.8 s | Fact |
| MCP `offset` for `query` and `search` | 0.25.0 | Pages are disjoint slices of one ranking; huge offsets are clamped | Fact |
| One embedding context per batch of chunks | 0.25.1 | `rqmd bench` embedding throughput rose from about 80 to about 134 texts/s | Fact |
| Filler words dropped from plain keyword queries of four or more terms | 0.25.2 | Natural-language keyword queries rank on their content words | Fact |
| Opt-in `.gitignore` handling and a per-file size limit | 0.26.0 | Collections can skip ignored and oversized files | Fact |
| Frontmatter `tags` and `aliases` in their own weighted full-text field | 0.27.0 | A tag match outranks a single body mention; one-time full-text rebuild | Fact |

## Ideas filed as issues instead of built

Each is larger than a bounded fix and needs a design decision first.

| Issue | Idea |
|-------|------|
| #120 | `rqmd watch`: keep the index fresh as files change |
| #121 | Persist query-expansion and rerank results across runs |
| #122 | Optional index refresh before an MCP search, and daemon idle exit |
| #123 | More than one result per file |
| #124 | Optional recency boost |
| #125 | Return fused results when `--timeout` expires before rerank |
| #126 | Configurable chunk size and overlap, and a minimum-chunk merge pass |
| #127 | Syntax-aware chunking for more languages |
| #128 | Progress output for the embedding and rerank stages |
| #129 | Late-interaction reranking as an optional stage |
| #131 | Reuse vectors for unchanged chunks when a document is edited |

**Inference.** Incremental-indexing systems such as CocoIndex key their work on content fingerprints, memoize per-chunk results and offer a live-update mode. That is the shape of #120, #121 and #131.

## EmbeddingGemma-2 against the current model

**Question.** Should `google/embeddinggemma-2` replace `embeddinggemma-300M-Q8_0`?

**Method (Fact).** A harness outside the repository served each model with `llama-server --embedding --pooling mean -c 2048 -ub 2048` from a llama.cpp master build, applied rqmd's exact query and passage prefixes, and scored by exact cosine similarity so index approximation does not affect the comparison. Metal, Apple M4, 32 GB. Sets are subsamples with a fixed seed.

### Quality (nDCG@10 × 100)

| Set | Queries | Docs | Current (Q8_0) | v2 Q8_0 | v2 BF16 | v2 Q8_0 minus current |
|-----|---------|------|----------------|---------|---------|-----------------------|
| scifact | 300 | 5,183 | 76.60 | 84.43 | 84.46 | +7.83 |
| nfcorpus | 323 | 3,633 | 39.07 | 35.24 | 35.32 | −3.83 |
| cosqa (code) | 500 | 20,604 | 39.58 | 45.05 | 44.20 | +5.47 |
| miracl-ja | 200 | 10,000 | 80.67 | 81.81 | 81.81 | +1.14 |
| miracl-zh | 200 | 10,000 | 76.19 | 72.43 | 72.42 | −3.76 |

- `rqmd eval` Hit@3 is 100% on all three models, so the built-in set does not discriminate between them.
- Truncating v2 to 512 dimensions cost about 0.1 to 0.3 points, 256 about 1 to 2, and 128 about 5 to 7.

### Speed (scifact documents, batches of 16)

| Model | Device | Docs/s | Tokens/s | Query p50 / p95 (ms) | Peak RSS (MB) | File (MB) |
|-------|--------|--------|----------|----------------------|---------------|-----------|
| current Q8_0 | Metal | 29.3 | 9,111 | 8.7 / 10.5 | 2,009 | 318 |
| v2 Q8_0 | Metal | 19.4 | 6,030 | 13.0 / 15.7 | 2,037 | 295 |
| v2 BF16 | Metal | 19.6 | 6,099 | 14.0 / 16.1 | 2,270 | 532 |
| current Q8_0 | CPU | 8.3 | 2,591 | 11.2 / 15.8 | 1,997 | 318 |
| v2 Q8_0 | CPU | 6.1 | 1,892 | 28.9 / 37.0 | 2,320 | 295 |
| v2 BF16 | CPU | 4.4 | 1,355 | 362 / 486 | 2,182 | 532 |

v2 Q8_0 runs at 0.66× the current model's throughput on Metal and 0.73× on CPU. The 362 ms CPU query latency for BF16 was not investigated.

### Decision

Do not switch. The rule set before measuring was: an average gain of at least 1.0 point over the English and CJK sets, no set worse by more than 1.0, `rqmd eval` Hit@3 held, and throughput at least 0.6×. Throughput and Hit@3 pass. The average is +0.35, and nfcorpus (−3.83) and miracl-zh (−3.76) each fail the per-set limit. v2 is clearly better on scifact and on code retrieval, and Q8_0 and BF16 score the same, so Q8_0 is the variant to test next time.

### Blocker for a later retry

**Fact.** The model's GGUF architecture is `gemma-embedding2`. Homebrew llama.cpp 0.6.0 and `llama-cpp-2` 0.1.159 both fail with "unknown model architecture"; only a llama.cpp master build loads it. rqmd cannot run it until a `llama-cpp-2` release includes that architecture and has gone through the 30-day soak the project requires for new releases.

**Open questions.**

- Whether real rqmd collections (markdown notes, mixed code) behave more like scifact or like nfcorpus is unmeasured; a larger eval built from real notes would settle it.
- The CPU BF16 query latency outlier above.

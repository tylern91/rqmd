# rqmd Changelog

## [0.1.1] — 2026-06-29

### Fixed

- `collection add`: stop loading the inference backend (embed + rerank GGUF
  models) during BM25 indexing. Switched to `open_store_no_backend` +
  `index_document_fts_only` so model loading is deferred to `rqmd embed`.
- `rqmd embed`: clear stale `content_vectors` rows before re-embedding a
  collection. Prevents UNIQUE constraint violation on `vid` when a prior
  interrupted embed left the DB ahead of the HNSW index.
- CLI result display: fix hardcoded `rrrqmd://` URI scheme typo in
  `print_cli`; path labels now use the canonical `rqmd://` URI from
  `SearchResult.file`.

## [0.1.0] — Initial release

rqmd is a Rust port of [tobi/qmd](https://github.com/tobi/qmd), the original
TypeScript hybrid-search CLI. This is the first public release of the Rust
implementation.

### Added

- **rqmd-core** — core library crate: SQLite schema (rusqlite), Tantivy BM25
  full-text index, usearch HNSW vector index, Reciprocal Rank Fusion (RRF),
  sliding-window chunker, and the hybrid BM25+vector+RRF+cross-encoder pipeline.
- **rqmd-cli** — binary crate producing the `rqmd` command with subcommands:
  `query`, `search`, `vsearch`, `get`, `multi-get`, `ls`, `collection`, `context`,
  `init`, `status`, `embed`, `update`, `doctor`, `bench`, `eval`, `mcp`.
- **rqmd-llm** — inference backend abstraction. Default: `LlamaCppBackend` via
  `llama-cpp-2` (GGUF, Metal on macOS / CPU on Linux). Optional `ort-backend`
  feature: OrtBackend via ONNX Runtime (CoreML/CUDA/DirectML).
- **rqmd-mcp** — MCP server exposing `query`, `search`, `get`, `multi_get`, and
  `status` tools. Stdio and Streamable HTTP transports.
- **Workspace profiles**: `dev` (fast incremental), `release` (LTO thin), `dist`
  (LTO fat, symbols stripped, panic=abort) for release binaries.
- **CI**: `rust.yml` — macOS arm64 (default + ort-backend) + Linux x64; clippy
  `-D warnings`, fmt check, unit tests, BM25 quality eval. Dist binary artifact
  on push to `main`.
- **Nix flake**: reproducible dev shell with Rust stable + cmake/C++ for
  `llama-cpp-2` build dependencies.

### Notes

- Query expansion / HyDE (`generate_constrained`) is wired in the API but the
  generate model is not yet loaded — a deferred future phase. `query` uses
  BM25 + vector + RRF + rerank only.
- HF models are pinned by repository name (not digest). Model pinning by digest
  will be added in a future release.
- The SQLite schema is intentionally compatible with the original TypeScript `qmd`
  index format. Indexes created by `rqmd` use RFC-3339 UTC timestamps in
  `created_at`/`modified_at`/`embedded_at`.

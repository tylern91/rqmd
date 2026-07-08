# rqmd Changelog

## [Unreleased]

---

## [0.4.0] - 2026-07-08
### Added
- `rqmd doctor` now warns when the index contains chunks embedded under more than
  one embedding fingerprint (stale vectors left behind by a model or chunking
  change), listing per-fingerprint chunk counts and recommending `rqmd embed --rebuild`.
- Test coverage for special-character paths (`#`, `&`, spaces, `[]`, `()`) round-tripping
  through index → search → get, and dotted-version (e.g. `2026.4.10`) BM25 tokenization.

### Changed
- **Breaking (MCP):** the `query`, `search`, and `multi_get` MCP tools now take
  `collections: [...]` (an array) instead of `collection` (a single string) — matches
  qmd 2.6.3's multi-collection filter. Existing MCP client configs passing a bare
  string must switch to an array; omitting the field still searches all collections.
- SQLite `busy_timeout` raised from 5s to 30s so a long embed batch no longer wedges
  a concurrent MCP/CLI reader.

### CI
- `security.yml`'s Trivy checkout now pins `fetch-depth: 1` and
  `persist-credentials: false`, matching the other workflow jobs' explicit settings.

---

## [0.3.1] - 2026-07-05

### Fixed
- Release binaries and the Homebrew tap now publish correctly; v0.3.0 shipped
  without assets due to a GitHub immutable-release conflict in the upload pipeline.

---

## [0.3.0] - 2026-07-05

### Added
- Homebrew tap (`brew tap tylern91/rqmd && brew install rqmd`) — downloads a prebuilt binary; no Rust toolchain or cmake required
- `cargo install --git https://github.com/tylern91/rqmd --locked rqmd-cli` one-liner install documented in README
- Prebuilt release binaries (macOS arm64, Linux x86_64) attached to every GitHub Release as `rqmd-<version>-<platform>.tar.gz` with `.sha256` sidecar files

### Changed
- README Installation section now leads with Homebrew and `cargo install --git`, followed by prebuilt binary download, then the existing from-source path

### CI
- `release.yml`: new `upload-assets` matrix job builds and attaches platform binaries after each release tag; optional `HOMEBREW_TAP_TOKEN` secret triggers automatic formula sync to `tylern91/homebrew-rqmd`
- `scripts/update-homebrew-formula.sh`: new script fills sha256 values into `packaging/homebrew/rqmd.rb` and optionally pushes to the tap repo

---

## [0.2.3] - 2026-07-05

### Added
- Security scanning CI: new `.github/workflows/security.yml` runs Trivy `fs` scan on every
  PR and push to `main`. CRITICAL + HIGH findings are uploaded to the GitHub Security tab
  (code-scanning alerts, SARIF). A second blocking step hard-fails the PR check on any
  CRITICAL vulnerability with a known fix (`ignore-unfixed: true`). HIGH findings are
  recorded but non-blocking.

### Changed
- Binary assets are now tracked with Git LFS. A `.gitattributes` file declares LFS
  patterns for images (`*.png`, `*.jpg`, `*.gif`, `*.webp`, `*.pdf`), ML model files
  (`*.gguf`, `*.onnx`, `*.bin`), and archives (`*.tar.gz`, `*.zip`). The existing
  `assets/qmd-architecture.png` has been converted to a pointer. New binaries committed
  to the repo will land in LFS automatically.

---

## [0.2.2] - 2026-07-05

### Fixed
- `rqmd query` (and `search`/`vsearch`) no longer panics with `assertion failed: self.is_char_boundary` when a result snippet contains multi-byte UTF-8 characters near the truncation boundary (#chunking)

---

## [0.2.1] - 2026-07-04
### Fixed

- `rqmd context check`: falsely reported all collections as MISSING even when
  contexts were correctly set via `rqmd context add`. Root cause was an
  `rrqmd://` (double-r) URI-scheme typo in the lookup key inside `check()`,
  while `context add` stores the canonical single-r `rqmd://` key. Fixed by
  extracting `collection_context_key()` in `db.rs` as the shared key-builder
  used by both `check()` and `get_context_for_collection`, eliminating the
  duplicated literal that allowed the drift. Regression test added.

---

## [0.2.0] - 2026-07-03
### Added

- `rqmd status`: Models section now shows the exact downloaded `.gguf` filename
  alongside the HuggingFace repo URL (e.g. `└─ embeddinggemma-300M-Q8_0.gguf`
  under the repo link). Previously only the repo URL was shown, leaving the actual
  quantization variant opaque to the user.

- `rqmd bench`: new in-process query-latency phase. When `--index-dir` points at a
  real index (`index.sqlite` present), the bench opens the store once (amortising
  model load), warms up each mode, then reports **p50/p99 in µs** across all query–
  round combinations for: BM25, vector, hybrid-no-rerank, and hybrid-with-rerank.
  Previously `bench` timed only embedding throughput on a hardcoded 10-text array
  and ignored `index_dir` entirely. Results are now printed per-mode as each
  completes (no batching at the end).

- `BENCHMARK.md`: new Full-Corpus Runtime Benchmark section. Runs on a large local
  markdown corpus (≈62.9k documents, 210k vectors, 1.5 GB index) on Apple M-series.
  Records end-to-end indexing rate, in-process embed throughput (Metal GPU and CPU),
  query latency p50/p99 per mode (BM25 / Vec / Hybrid), and search quality Hit@K.
  All numbers are aggregate only — no corpus paths or document content.

- `scripts/install.sh`: content-aware install that replaces `cargo install --path`.
  Uses `cargo build` fingerprinting (content-based, not version-based) then atomically
  copies the fresh binary to `~/.cargo/bin/rqmd`. Supports `RQMD_PROFILE` env var and
  passes extra args through (e.g. `./scripts/install.sh --features ort-backend`).
- File exclusion on `collection add` and `rqmd update`: new `--ignore <PATTERN>` flag
  accepts gitignore-style glob patterns (powered by `globset`). Built-in exclusions
  always apply: hidden paths (`.`-prefixed), `node_modules`, `vendor`, `dist`, `build`,
  `target`, `.cache`. Patterns are stored in the collection record and re-applied on
  every subsequent `update` run for that collection.
- `rqmd mcp --daemon`: self-respawns as a background HTTP process (implies `--http`)
  and exits, leaving the server running detached. Existing `--http`/`--port` flags
  are unchanged.
- GPU feature flags in `rqmd-llm` and `rqmd-cli`: `metal` (default on, no behaviour
  change for existing macOS builds), `cuda`, and `vulkan`. CPU-only builds:
  `--no-default-features`. Previously `metal` was hardcoded in the `llama-cpp-2` dep.

### Fixed

- cmake 4.x is now supported for building `llama-cpp-sys-2` on macOS. The previous
  belief that cmake 4.x would break the llama.cpp CMake build was Python-specific
  (the Python `cmake` pip package had an incompatibility); the Rust `llama-cpp-2`
  crate builds cleanly with cmake 4.x. The CI `pip install "cmake<4"` pin has been
  removed from `rust.yml` (both `build-macos` and `dist-binary` jobs). The README
  troubleshooting block and `flake.nix` / `nix.yml` comments have been updated
  accordingly.

- Environment variable names corrected throughout documentation. All `rqmd` env vars
  use the `RRQMD_` prefix (double-R), matching what the code actually reads. The
  docs previously showed `RQMD_*` (single-R), which silently had no effect. Affected:
  `README.md`, `BENCHMARK.md`, `scripts/crosscheck.sh`. Correct names:
  `RRQMD_INDEX_DIR`, `RRQMD_INFERENCE_BACKEND`, `RRQMD_ORT_EP`, `RRQMD_FORCE_CPU`,
  `RRQMD_CI`, `RRQMD_VERBOSE`.

- `rqmd update`: unchanged documents no longer re-added to the Tantivy FTS index.
  Previously `index_document_fts_only` always called `fts.add_document` even when the
  content hash was identical, causing duplicate Tantivy segments that inflated scores
  and grew the on-disk index on every `update` run.
- File exclusion: non-UTF-8 path components now correctly exclude the path (fail-closed)
  instead of silently passing all exclusion checks via `unwrap_or("")`.

### Changed

- `BENCHMARK.md`: removed "Phase 0" internal-phase framing; fixed stale `QMD_*` env
  vars to `RQMD_*`; removed stale "Phase 6" internal reference. All tables and
  performance comparison data preserved.
- `README.md`: six new sections — *Excluding files*, *Models*, *MCP server*, *Where
  data lives*, *Differences from qmd*, *Migrating from qmd*. QMD inspiration credit
  added to tagline and Acknowledgements. Install docs now reference `scripts/install.sh`.
- All four `Cargo.toml` files: added `publish = false`, `repository`, `keywords`,
  `categories` metadata. `rqmd` package name is taken on crates.io by a separate
  project (`stn/rqmd`); `publish = false` guards against accidental publish.
- Stale `qmd-cli` / `target/dist/qmd` / `QMD_INDEX_DIR` references fixed in
  `.cargo/config.toml`, `flake.nix`, and `scripts/crosscheck.sh`.

---

## [0.1.6] - 2026-06-30
### Added

- Phase 4: HyDE / query expansion — generation model (Qwen3-1.7B Q8_0) downloaded
  eagerly alongside embed/rerank; free-form constrained generation with ChatML prompt;
  `lex:`/`vec:`/`hyde:` expansion results fused via RRF (expansion weight 1.0,
  original weight 2.0); non-fatal fallback (warn + original results) on any error.
- Typed-line query parser (`rqmd-core::query::parse_query`): routes `lex:`/`vec:`/`hyde:`/`intent:`
  typed-doc mode directly to their respective search methods; plain lines run expansion.
- `--intent <STRING>` flag on `rqmd query` and `intent` field in MCP `QueryInput`;
  intent steers the expansion prompt, reranker cross-encoder query, and snippet term
  selection.

### Fixed

- Generation model was never downloaded or used: `generate_constrained` was a stub that
  `bail!()`ed on all backends and the expansion step was skipped.
- Generation model repo name was wrong (`ggml-org/Qwen3-1.7B-Q8_0-GGUF` does not exist;
  correct: `ggml-org/Qwen3-1.7B-GGUF`) and filename casing was wrong (`qwen3-1.7b-q8_0.gguf`
  → `Qwen3-1.7B-Q8_0.gguf`).
- GBNF grammar sampling caused uncatchable process aborts (`GGML_ASSERT(!stacks.empty())`
  via C FFI when a multi-byte token drove the grammar into a dead state); replaced with
  free-form generation (temp/top_k/top_p/dist sampler chain) + lenient line parsing.

---

## [0.1.5] - 2026-06-30

### Fixed

- `doctor`: fix model-cache check always reporting "not cached" on macOS. Root
  cause: the check used `dirs::cache_dir()` (→ `~/Library/Caches/huggingface/hub`)
  while hf-hub stores models in `~/.cache/huggingface/hub`. Replaced the manual
  path rebuild with a `rqmd_llm::model_cache_report()` helper that delegates to
  `hf_hub::Cache::from_env()`, so the path matches the actual downloader and
  `HF_HOME` overrides are honoured.
- `doctor`: add Generation model (`Qwen3-1.7B`) to the model-cache report (it was
  missing; it downloads on first HyDE query expansion, so "not cached" is accurate
  until first use).

## [0.1.4] - 2026-06-30

### Fixed

- `update`: replace hard-coded 60-column space-pad clear with `\r\x1b[2K` so the
  progress line is fully erased before each collection's `Indexed:` summary,
  regardless of terminal width or path length.
- `status`, `embed`, `update`, `doctor`: fix phantom `Pending: N need embedding`
  that `rqmd embed` never cleared. Root cause: the "needs embedding" COUNT query
  was body-blind — it counted empty-body documents (hash = SHA-256 of `""`) as
  pending, but the embed loop skips empty bodies. Centralized the query into
  `db::count_docs_needing_embed` with a `JOIN content … AND length(c.doc) > 0`
  filter so the count matches what embed will actually process.

## [0.1.3] - 2026-06-29

### Fixed

- `update`: show real file total in progress (`Indexing: N/total`) by pre-collecting
  matching paths before the index loop; previously showed a literal `?`.
- `update`, `embed`, `collection add`: fix `term_width()` on Apple Silicon — `ioctl`
  must be declared variadic (`...`) to match the arm64 AAPCS64 calling convention;
  the non-variadic declaration put the `Winsize*` argument in the wrong register,
  causing `term_width()` to always return `None` and the width-clamp to never engage.
  Progress lines now overwrite in place instead of spawning a new line per update.
- `update`, `embed`, `collection add`: harden progress rendering by emitting
  `\r\x1b[2K` (erase-line) before each update and using `unwrap_or(80)` as fallback
  width so a width-detection miss can no longer cause line wrap.
- `cli`: bump `rqmd-cli` crate version so `cargo install --path` without `--force`
  correctly detects and installs new builds.

---

## [0.1.2] - 2026-06-29
### Added

- `embed`: display bytes/s throughput in progress bar (matches qmd's `formatBytes/s` metric).
  Progress line now shows: `bar% input · N chunks · D/T docs · X.X MB/s · ETA T`

### Fixed

- `embed`, `update`, `collection add`: clamp progress lines to terminal width via
  `term_width()` / `fit_to_width()` helpers in `format.rs`; prevents multiline smear
  when paths or stats exceed the terminal width. Progress is suppressed when not a TTY.
- `update`: fix advisory message branding — was `'qmd embed'`, now `'rqmd embed'`.
- `embed`: fix `UNIQUE constraint failed: content_vectors.vid` crash — reconcile
  HNSW `next_vid` with `MAX(content_vectors.vid)` in SQLite on startup; add in-run
  hash dedup to stop duplicate-hash drift; add `--rebuild` flag and divergence advisory.
- `embed`: guard embed/rerank token overflow with truncation to context window
  (`EMBED_CONTEXT_SIZE - 4` tokens); prevents `GGML_ASSERT n_ubatch >= n_tokens` abort.
- `fts`: normalize Tantivy BM25 score to `[0,1)` using `s/(1+s)` squash (mirrors
  qmd) so `rqmd search` never displays scores above 100%.
- `llm`: suppress llama.cpp INFO/WARN noise; send logs to tracing subscriber instead
  of stderr; add `add_sequence(false)` for Mean-pooling encoders.
- `embed`: make embed resumable across interrupts; fix `update` UNIQUE constraint;
  fix char-boundary panic on multi-byte UTF-8 (em dash, CJK) in chunker.
- `status`: rewrite `rqmd status` to match qmd's layout — single `Size:` line,
  per-collection multi-line blocks, `Updated`/`AST Chunking`/`Examples`/`Models`/`Tips`
  sections; correct `rqmd` branding throughout.

---

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

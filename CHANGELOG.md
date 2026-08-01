# rqmd Changelog

## [Unreleased]

---

## [0.8.1] - 2026-08-01
### Documentation
- `README.md` and `docs/SYNTAX.md` brought up to date with the v0.7.0 CLI
  surface: `similar`, `mcp status`/`stop`, `--rebuild`, `--mask`/`--hidden`,
  `--host`, `--no-expand`, and the real four-counter `update` summary;
  `--format` expanded to all six values; the rerank candidate-pool cap
  explained; the "hidden files always skipped" claim corrected; a Quick
  Start example fixed (missing trailing slash on `context add`); an MCP
  `--host` security warning and tool-parameters reference added; three new
  sections (Score interpretation, How it works + smart chunking, Model
  configuration); `docs/SYNTAX.md` rebranded from `qmd` to `rqmd` and
  corrected on tokenization (exact-token match, not prefix; OR-combined
  multi-term queries).

---

## [0.8.0] - 2026-08-01
### Fixed
- `-c`/`--collection` on `query`, `search`, `vsearch`, and `multi-get` was
  declared as a single-valued flag even though it was documented as
  repeatable and OR-matched across collections. Passing `-c a -c b`
  silently kept only the last value (`b`) and dropped `a` — no error, no
  warning, just a narrower result set than requested. The flag is now
  genuinely repeatable, wired through the existing multi-collection
  plumbing already used by the MCP server.

---

## [0.7.0] - 2026-08-01
### Added
- `rqmd similar <path|#docid>` finds documents most similar to an
  already-indexed one, reusing the existing HNSW index directly — no
  model load required.

### Changed
- `--format` is now a validated enum instead of a bare string: an
  unsupported value (e.g. a typo, or a format a given command doesn't
  support like `get --format md`) now fails fast instead of silently
  falling back to human-readable CLI rendering.
- `--format files` now emits real, absolute filesystem paths (collection
  root joined with the relative path), one per line, instead of the old
  synthetic `#docid,score,file` shape — the old output wasn't a real
  path and couldn't be piped to `xargs`/`cat`. Scripts parsing the old
  `files` shape need to switch to plain path handling.

### Fixed
- Fixed stale single-R `RQMD_*` env var names in `rqmd-cli/src/store.rs`
  doc comments and a stale `vectors.usearch` path in the README; the var
  actually read is `RRQMD_INDEX_DIR`/`RRQMD_INFERENCE_BACKEND` and the
  on-disk file is `hnsw.usearch`.

---

## [0.6.5] - 2026-08-01
### Fixed
- `mcp --daemon` on a port that's already occupied used to print "started"
  and exit 0 even though the child died immediately, because the child's
  stderr went to `/dev/null` and the parent never checked whether the
  server actually came up. The daemon now writes a pidfile, and the parent
  pre-checks the port, waits for the daemon's `/health` endpoint after
  spawning, and on failure surfaces the real log tail and exits non-zero
  instead of reporting false success.
- Added `mcp stop`/`mcp status`, backed by the same pidfile. Identity is
  confirmed by cross-checking the recorded pid against the daemon's own
  `/health` response (never a bare pid match, since pids get recycled),
  so `stop`/`status` never signal an unrelated process and a stale
  pidfile never blocks a fresh start.
- The daemon now shuts down gracefully on SIGTERM/ctrl-c instead of
  needing to be killed as an orphan, logs to a real file instead of
  `/dev/null`, and `--daemon` now implies `--http` instead of conflicting
  with it. `--host`/`RRQMD_MCP_HOST` is supported for non-loopback binds,
  with a loud warning naming what's exposed (unauthenticated full-text
  and semantic search, plus arbitrary file content via `get`).
- Docs: removed the `SYNTAX.md` MCP `searches` array and REST `/query`
  endpoint, neither of which exist — the `query`/`search` MCP tools take
  the same `query` string as the CLI, served over the MCP protocol
  itself (stdio or `/mcp`), with `/health` as the only bespoke REST
  endpoint. Also removed the dead `RRQMD_CI`/`QMD_CI` README rows —
  nothing in the codebase reads that variable.

---

## [0.6.4] - 2026-08-01
### Fixed
- `embed_fingerprint` (used to detect a stale vector index) was derived
  from hardcoded literals that happened to match the chunking constants
  at one point in time, so a chunking change was permanently
  undetectable. It's now derived from the actual chunking constants.
- Embedding now applies the model's documented query/passage prompt
  asymmetry instead of embedding queries and document chunks
  identically; HyDE's hypothetical-document text is embedded on the
  passage side, since that's the subspace it needs to land in to work.
- The llama.cpp backend now L2-normalizes its embedding output, matching
  the documented contract and the ONNX Runtime backend's existing
  behavior (previously the two backends silently disagreed). Cosine
  similarity is scale-invariant, so result ordering is unaffected.
- `doctor`, `embed` (without `--rebuild`), `query`, and `vsearch` now
  share one stale-fingerprint check that warns instead of silently
  degrading or auto-rebuilding, and correctly flags a single uniformly
  stale index — not just a mix of fingerprints.
- Fixed stale single-R `RQMD_*` env var names in `rqmd-llm` doc
  comments; the vars actually read are `RRQMD_INFERENCE_BACKEND`,
  `RRQMD_ORT_EP`, and `RRQMD_FORCE_CPU`.

**Note:** every fix above shifts the embedding fingerprint, so upgrading
triggers one re-embed (`rqmd embed --rebuild`) rather than three
separate ones across future releases.

---

## [0.6.3] - 2026-08-01
### Fixed
- `update` no longer reports a hardcoded "0 removed": it now diffs the
  indexed set against the real walked file list per collection and
  soft-deletes (`active = 0`) documents whose file was deleted or renamed
  away, sweeping the matching Tantivy entries so they stop being
  searchable immediately. Previously a rename permanently doubled that
  document in search results, and a deleted file kept being returned with
  a path that no longer existed.
- `collection remove` no longer leaves every document, its content,
  vectors, and search-index entries fully searchable after the collection
  is supposedly gone — it now purges everything it owns (content/vectors
  no longer referenced by any other collection are dropped too, since
  content is deduplicated globally by hash).
- Re-indexing an existing path could silently feed a stale/unrelated
  document id into the search index (SQLite doesn't advance
  `last_insert_rowid()` on an upsert's `ON CONFLICT DO UPDATE` arm); fixed
  by reading the id back with `RETURNING id`.

---

## [0.6.2] - 2026-08-01
### Fixed
- Collection scoping (`-c <collection>`) no longer returns false-empty when
  the target collection is a small minority of a larger corpus: BM25 now
  pushes a must-clause down onto the indexed `filepath` field to narrow the
  candidate pool before the existing exact-prefix post-filter, and vector
  search widens its candidate count until enough in-scope hits are found.
- `vsearch`/hybrid search no longer return the same document once per chunk:
  both vector search and RRF fusion now dedupe to the best-scoring chunk per
  document. This also removes RRF's systematic bias toward long documents,
  which previously accumulated a rank-based score once per chunk.
  `-n`/`--limit` is no longer silently capped at 20 — the internal rerank
  candidate pool now scales with the requested limit
  (`clamp(limit*2, RERANK_CANDIDATE_LIMIT, 100)`), warning when it's capped.
- A query containing FTS special syntax (e.g. a colon read as a field
  specifier) no longer degrades to a silent empty result — parsing is now
  lenient, with a warning logged on partial parse failures.
- `collection exclude` (`include_by_default = 0`) is no longer a no-op:
  default-scope queries now resolve the included-collection set once per
  query and skip scoping entirely when every collection is included.

---

## [0.6.1] - 2026-08-01
### Fixed
- `collection add`/`index update` no longer report success while indexing
  zero files: a collection root under a dot-directory (e.g. a dotfiles
  checkout) is now resolved relative to the collection root before the
  exclusion scan, instead of matching dot-components in the absolute path.
  Dot-directories nested inside the tree are still excluded by default; pass
  `--hidden` to include them.
- Frontmatter is now parsed: every note used to be titled `"---"` because the
  YAML fence was indexed verbatim. Title now resolves from frontmatter
  `title:` → first `#` heading → filename stem, the fence is stripped from
  indexed/searched text, and `tags:`/`aliases:` values are folded in as extra
  search terms. Content hashing runs over the stripped text, so a
  metadata-only frontmatter edit no longer forces a full re-embed.
- Multi-glob collection masks (`**/*.{md,mdx,txt}`, comma-separated patterns)
  used to match zero or only some files, and `collection add` vs.
  `index update` could silently disagree on membership. Both now share one
  glob-set builder that errors loudly on a malformed pattern instead of
  matching nothing.
- Unreadable files were previously skipped with no count; the indexing
  summary now reports skips by reason (not UTF-8, permission denied, I/O
  error). `collection add <file>` on a non-directory path now fails instead
  of persisting a document with an empty relative path.
- README: corrected the documented index-storage path — `dirs::cache_dir()`
  resolves to `~/Library/Caches/rqmd/` on macOS, not `~/.cache/rqmd/`.

### Removed
- `example-index.yml`, a stale artifact demonstrating the now-fixed
  multi-extension glob bug.

---

## [0.6.0] - 2026-07-31
### Added
- Each GGUF model (embed, rerank, generate) now loads lazily on first use
  instead of all three loading unconditionally in `LlamaCppBackend::new`.
  `status` and `doctor` remain load-free.
- Idle model eviction: `RRQMD_MODEL_IDLE_TTL` (default 300s, `0` disables)
  controls how long a loaded model may sit unused before a periodic sweep
  releases it. Without this, query expansion being on by default meant the
  ~2 GB generate model would load within a few queries and never be freed,
  ratcheting a long-lived daemon's RSS upward (measured at 2.97 GB on a
  6-day-old daemon that should idle around 750 MB).

### Fixed
- Corrected two places that claimed `rerank_n_ctx` defaults to 512 (module
  doc in `rqmd-llm`, `BENCHMARK.md`) — the shipped default is 2048; 512 is a
  documented tuning option for the 448 MiB KV budget on Apple Silicon, not
  the default.

---

## [0.5.2] - 2026-07-30
### Changed
- The HNSW vector index is now memory-mapped (`VectorIndex::view`) instead of
  fully read into RAM (`VectorIndex::load`) for every read-only code path —
  `rqmd-mcp`, `query`, `search`, `get`, `multi_get`, `status`, `ls`, `context`,
  and the query-latency benchmark. This moves the on-disk index's private RSS
  cost (~685 MB uncompressed F32) into shared, evictable page cache. Indexing
  paths (`embed`, `update`, `collection add`, `init`) still open the store
  read-write and load the index fully, since usearch has no documented
  mutate-after-view behavior. A `Store` opened read-only now returns a clean
  error instead of attempting a write if a mutating code path is reached.

---

## [0.5.1] - 2026-07-26
### Fixed
- Query-time context resolution only ever considered the collection-root
  context (`context:rqmd://<collection>/`) and the legacy global context
  (`context:/`) — `get_context_for_collection` never received a document's
  path, so any per-subdirectory context added via `rqmd context add
  "rqmd://<collection>/<subpath>/" ...` was stored and listed but structurally
  unreachable at query time. Added `get_context_for_path(conn, collection,
  rel_path)`, which walks a matched document's path from its deepest ancestor
  directory up to the collection root, returning the first configured context
  found (falling back to the root/legacy behavior if no ancestor matches).
  Wired into all three query-result-building call sites (vector search,
  hybrid+rerank, and BM25) so results now carry the nearest curated context
  for their area instead of only ever the collection-wide one. Ancestor
  lookups match a document's stored path verbatim (no case-folding or
  slugification), so context keys must be added using the same casing as
  `rqmd ls`/`documents.path`.

---

## [0.5.0] - 2026-07-13
### Fixed
- `multi-get` (CLI and MCP) matched plain path fragments with an unanchored substring
  check, so a pattern like `README.md` could silently also return `OLD-README.md` —
  worst case for a caller that expects one document and gets the wrong one with no
  error. Matching is now anchored at a `/` path-segment boundary; an explicit glob
  (e.g. `docs/*.md`) is required for fragment/prefix matching, matching the MCP
  tool's documented behavior.
- `#docid` prefix lookups (`get_document_by_docid_prefix`) had no deterministic
  tie-break on a hash-prefix collision (`LIMIT 1` with no `ORDER BY`), so the
  resolved document could vary run to run. Now ordered by `(collection, path)`
  before `LIMIT 1`.

### Added
- `--no-expand` flag (`RRQMD_NO_EXPAND` env) on `rqmd query`, and an `expand: false`
  input on the MCP `query` tool, to skip the LLM query-expansion/HyDE round-trip.
  BM25 + vector retrieval and RRF fusion still run — this is pure hybrid retrieval
  without the extra generation call, for corpora or callers that don't need it.

### Changed
- `multi-get`'s plain-pattern resolution now pushes down to SQL
  (`find_documents_by_needles`) instead of loading every document in the index and
  filtering in Rust — avoids a full-table scan for the common explicit-path case.
- CLI and MCP `multi-get` now share one resolution implementation
  (`rqmd_core::resolve`), removing a duplicated `glob_match`/matching function that
  had drifted between the two crates.

### CI
- `rust.yml`'s `upload-artifact` step bumped from `@v4` to `@v7`.

---

## [0.4.2] - 2026-07-13
### Fixed
- The per-collection pre-update hook (`rqmd collection update-cmd`, shown as
  `Hook:` in `collection show`) was persisted, loaded, and displayed but never
  executed — `rqmd update` walked and reindexed the collection directory
  without ever running it. `run_update` now spawns the stored command via
  `sh -c` with the collection's directory as CWD before walking it, warning
  (not failing) on a non-zero exit or spawn error.

---

## [0.4.1] - 2026-07-08
### Fixed
- `rqmd --version` and the MCP server's `server_info` were pinned at `0.2.0` since
  the v0.3.0 release — every crate `Cargo.toml` still hardcoded the old literal, and
  the release pipeline built binaries from the tagged tree without ever touching it.
  Version is now a single `[workspace.package]` value inherited by every crate
  (`version.workspace = true`), so one line bumps all of them.
- Added `scripts/check-version-sync.sh`, wired into CI, which fails the build if the
  workspace version and the CHANGELOG's top release heading disagree — the exact
  drift that caused this bug.
- Added a release-time assertion (`upload-assets` job) that the built binary's
  `--version` output matches the tag being released, as a final safety net.

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

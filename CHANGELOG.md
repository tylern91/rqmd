# rqmd Changelog

## [Unreleased]

---

## [0.12.1] - 2026-08-25
### Fixed
- CI: `rustsec/audit-check` was pinned to `v2.0.0`, whose `action.yml` still
  declares the deprecated `node20` runtime; no tag past `v2.0.0` carries the
  upstream Node 24 fix, so `security.yml` now SHA-pins `rustsec/audit-check` to
  the `main` commit that ships it.
- CI: the release workflow's "Import GPG key" step failed opaquely
  (`gpg: no valid OpenPGP data found`) whenever `GPG_PRIVATE_KEY` was stored
  base64-wrapped instead of raw ASCII-armored — silently blocking every
  release since v0.11.0. The step now auto-detects and unwraps a
  base64-wrapped key, and fails with a non-leaking shape diagnostic
  (byte lengths only) instead of a bare gpg error when the secret is neither.
- CI: `release.yml` declared a top-level `workflows: write` permission, which
  is not a valid `GITHUB_TOKEN` permission scope — GitHub rejected the entire
  workflow file at startup, failing every release in 0 s with zero jobs run
  (this is what actually blocked v0.12.1). The release job now mints a
  short-lived GitHub App installation token and uses it for `actions/checkout`
  and the tag push instead; the App's Workflows permission is what lets the
  push succeed against a commit whose tree touches `.github/workflows/*`,
  which is what blocked the v0.11.1/v0.12.0 tag pushes.
- CI: `release.yml` now also accepts a `workflow_dispatch` trigger
  (`label`/`pr_number`/`dry_run` inputs) as a break-glass path for cutting a
  release without a qualifying PR merge, still routed through the same
  App-token identity rather than a local machine.
- CI: `security.yml`'s `cargo-audit` job now grants `checks: write` — without
  it, `rustsec/audit-check` found 0 vulnerabilities but still failed the job
  because it couldn't publish its result as a check run.

---

## [0.12.0] - 2026-08-25
### Breaking
- `rust-version = "1.88"` is now declared in `[workspace.package]`, inherited
  by all four crates. This is the true floor of the resolved dependency graph
  (`time` 0.3.51, `darling` 0.23, `cookie_store` 0.22), not a new
  requirement — but it is now enforced: a toolchain below 1.88 fails to
  compile with a clear resolver error instead of a confusing one. Anyone
  building rqmd on an older Rust must upgrade to 1.88+.

### Changed
- Migrated all four crates to Rust edition 2024 (needs only 1.85, below the
  1.88 floor, so it costs nothing in compatibility). `unsafe extern "C"`
  blocks and the `std::env::set_var`/`remove_var` call sites edition 2024
  requires as `unsafe` now carry safety comments proving soundness (no
  concurrent env access, or `#[serial]`-guarded tests).
- CI: `rust.yml`'s `build-linux` job now runs a `["stable", "1.88"]`
  toolchain matrix. The pinned `1.88` leg is `continue-on-error` so it can't
  block `dist-binary`, but a real MSRV regression still shows red in PR
  checks.
- Docs: `docs/INSTALL.md` no longer claims a false "≥1.78" floor (was wrong
  by 10 releases); `CONTRIBUTING.md` gained an MSRV policy section;
  `flake.nix` now notes the drift risk of its unpinned
  `nixpkgs-unstable` rustc/cargo.

---

## [0.11.1] - 2026-08-25
### Fixed
- MCP daemon (`rqmd mcp --daemon`) served permanently stale search results
  after any subsequent `rqmd index`/`update`: it opened both the FTS
  (Tantivy `ReloadPolicy::Manual`) and vector (`usearch` mmap `view()`)
  indexes read-only and never reloaded them. `Store` now stats Tantivy's
  `meta.json` (rewritten atomically on every commit — the SQLite main
  file's mtime does not change under WAL-mode writes) and the HNSW file,
  and reloads both when either has advanced.
- `FtsIndex::search_fts_multi` panicked on `limit: 0` (`TopDocs::with_limit`
  asserts `limit != 0`), which — reached via the MCP `search` tool or the
  CLI's `search -n 0` — unwound through the daemon's `Mutex<Store>` guard,
  poisoning it and wedging every subsequent tool call until restart. Zero
  results is now returned directly instead of reaching Tantivy.
- `db::find_documents_by_needles` (`multi_get`'s literal-fragment path)
  built its `LIKE` clauses without escaping `%`/`_`, so a needle containing
  either widened into an unintended wildcard match (e.g. a bare `%`
  matched every document, since every row's `collection/path` contains a
  `/`). Both `LIKE` arms now escape the needle and add `ESCAPE '\'`.
- `resolve_multi_get` classified only `*` as a glob metacharacter, so
  `globset`-valid patterns using `?` or `[a-z]` fell through to the
  literal-needle branch and silently matched the wrong documents (or none).
  Classification now checks for any of `*`, `?`, `[`, `{`.
- An unmatched `#docid`, needle, or glob entry in a `multi_get` pattern was
  silently dropped from the result set with no signal, indistinguishable
  from "the document has no content." Each now emits a `tracing::warn!`.
- CI had no RUSTSEC advisory scan of `Cargo.lock` — the only scanner
  (Trivy) ignores unfixed advisories and only blocks on `CRITICAL`, so a
  patchable `HIGH` Rust advisory would merge unnoticed. Added a
  `cargo-audit` job, and bumped two transitive deps it flagged on first
  run: `crossbeam-epoch` 0.9.18 → 0.9.20 (RUSTSEC-2026-0204, invalid
  pointer deref) and `h2` 0.4.15 → 0.4.16 (RUSTSEC-2026-0258, unbounded
  empty DATA frames).
- CI: a release that failed after tagging (e.g. the v1.0.0 bump-label
  escalation bug) silently skipped asset uploads, release-notes
  finalization, and the Homebrew tap sync, with no supported way to finish
  the job — this is what left v0.11.0 published without a Linux asset and
  the tap pinned to v0.10.6. `upload-assets`, `finalize-notes`, and
  `sync-homebrew` are now a separate reusable workflow
  (`publish-assets.yml`) that also accepts `workflow_dispatch`, so a
  half-finished release can be completed for an already-existing tag.
- `scripts/stamp-changelog-refs.sh`: a release whose commit range contained
  only `chore(release):` commits (e.g. a squash-merged release PR) aborted
  the entire release job instead of falling back to the unfiltered commit
  pool; a genuinely empty range now warns and exits 0 instead of failing
  the release.

---

## [0.11.0] - 2026-08-10
### Breaking
- Every environment variable's doubled-R `RRQMD_` prefix is renamed to the
  single-R `RQMD_`, matching the binary, crates, and URI scheme. No
  compatibility shim — set the new names before upgrading. See
  [docs/MIGRATING.md](docs/MIGRATING.md#migrating-from-rqmd--010x) for the
  full old → new table:

  | Old | New |
  |---|---|
  | `RRQMD_INDEX_DIR` | `RQMD_INDEX_DIR` |
  | `RRQMD_INFERENCE_BACKEND` | `RQMD_INFERENCE_BACKEND` |
  | `RRQMD_ORT_EP` | `RQMD_ORT_EP` |
  | `RRQMD_FORCE_CPU` | `RQMD_FORCE_CPU` |
  | `RRQMD_MCP_HOST` | `RQMD_MCP_HOST` |
  | `RRQMD_MCP_PORT` | `RQMD_MCP_PORT` |
  | `RRQMD_MCP_ALLOW_NON_LOOPBACK` | `RQMD_MCP_ALLOW_NON_LOOPBACK` |
  | `RRQMD_MODEL_IDLE_TTL` | `RQMD_MODEL_IDLE_TTL` |
  | `RRQMD_NO_EXPAND` | `RQMD_NO_EXPAND` |
  | `RRQMD_VERBOSE` | `RQMD_VERBOSE` |

### Fixed
- `rqmd mcp`'s five tools (`query`, `search`, `get`, `multi_get`, `status`)
  reported backend and store errors as successful results with an
  error-shaped string body, so an MCP client could not distinguish a failed
  search from a successful one whose top result happened to discuss errors.
  They now return a real MCP error response on failure.
- `rqmd context add <bare-name>` wrote a key
  (`context:<name>`) that no reader ever looks up — only the fully-qualified
  `context:rqmd://<collection>/` form round-trips — so context silently never
  applied to bare-name adds. `add`/`rm` now resolve a bare name against the
  known collection list and normalize it, or reject with the correct form.
- Collection-scoped BM25 search (`search_fts_multi`) applied its
  collection filter *after* the Tantivy top-K cut, so a scoped query could
  return fewer than the requested limit even when enough in-scope matches
  existed further down the ranking. Candidates are now over-fetched before
  the post-filter so the requested limit is honored.

### Changed
- `rqmd multi-get` (and MCP `multi_get`) glob matching moved from a
  hand-rolled `*`-only matcher to `globset`. Cross-`/` matching is preserved
  (`docs/*` still matches `docs/a/b.md`), and `?`, `[...]`, and `{...}` are
  now supported. An invalid glob pattern is now a reported error instead of
  silently matching nothing.
- `db::purge_collection` and `db::deactivate_missing_documents` now issue
  batched/set-based SQL instead of one query per row, removing two N+1
  patterns from `rqmd update`.
- Search result construction (`Store`'s `search_fts`, `search_vec`,
  `hybrid_query`, `hybrid_query_multi`) now shares one `result_from_doc`/
  `first_chunk` path instead of four near-identical copies, and stops
  running the full multi-pass chunker just to take its first chunk.

### Removed
- The dead `RQMD_CI` CI-only environment variable (set in
  `rust.yml`, read by nothing).
- Verified-unreachable code from `rqmd-core::chunking` (an unreachable
  branch in `extract_snippet`, three unused `build_snippet_result`
  parameters, the ignored `intent` parameter, and the dead `BreakPattern.kind`
  field) and dead ANSI-color helpers from `rqmd-cli::format`.

### Internal
- `crates/rqmd-core/tests/integration.rs` (40 tests) now actually runs in
  CI on both the macOS and Linux legs — CI previously compiled it
  (`--all-targets`) but never executed it (`cargo test --workspace --lib`).
- All `cargo build`/`test`/`clippy`/`run` invocations in CI now pass
  `--locked`.
- Added `[workspace.lints.clippy] all = "deny"` so a local `cargo clippy`
  matches CI's strictness instead of being laxer.
- New tests for `rqmd-cli::format`, `rqmd-cli::commands::context`, and
  `rqmd-cli::store`, previously untested.
- `docs/CRATE-API.md`'s example code is now backed by real doctests on
  `Store` and `InferenceBackend` (`cargo test --doc`), so a future signature
  change fails the build instead of letting the docs drift silently.

---

## [0.10.6] - 2026-08-04
### Fixed
- `rqmd-llm`: model downloads could fail with a 401 even on public repos, and
  a cache populated before revision pinning existed forced a network round
  trip on every run — cache lookups now fall back to the legacy
  `snapshots/<revision>/<file>` layout (verifying the hash once and healing
  `refs/<revision>` so `doctor` and later runs see it directly), a rejected
  credentialed request now retries anonymously instead of hard-failing, and
  `HF_TOKEN`/`HUGGING_FACE_HUB_TOKEN` are now honored ahead of the cached
  token file. `HF_HUB_OFFLINE`, previously documented but not implemented by
  the underlying `hf-hub` crate, now actually disables downloads.

---

## [0.10.5] - 2026-08-03
### Fixed
- `highlight_terms` computed match offsets against a lowercased copy of the
  search result but sliced the original string with them — characters whose
  byte length changes under case folding (e.g. Turkish `İ`, Kelvin sign `K`)
  could shift the slice off a char boundary and panic, or silently highlight
  the wrong span.
- Collection-context truncation in `index.rs` byte-sliced at a fixed offset
  using a byte-length guard that looked like a char-count guard, causing the
  same class of panic on multi-byte UTF-8 input.

---

## [0.10.4] - 2026-08-03
### Fixed
- `rqmd mcp` bypassed the backend factory and hardcoded the llama.cpp
  backend directly, so `RRQMD_INFERENCE_BACKEND=ort` worked for the CLI but
  was silently ignored by the MCP server — both now share one construction
  path (`create_backend`).
- The stale-embeddings check compared against a hardcoded llama.cpp default
  fingerprint regardless of which backend actually produced the vectors,
  causing a permanent false "embeddings are stale" warning whenever a
  non-default backend (e.g. ORT) was active.
- Backends that don't support rerank/generate (e.g. ORT) had their errors
  silently discarded via `.ok()`, degrading `query` to BM25+vector-only with
  no user-visible signal. Callers now check `InferenceBackend::capabilities()`
  and log a warning before skipping an unsupported step.
- `OrtBackend::new()` unconditionally spawned a fresh Tokio runtime to
  download its model via hf-hub, which panics ("Cannot start a runtime from
  within a runtime") when called from an already-async context — exactly
  the situation the MCP-backend-selection fix above now creates the first
  time `rqmd mcp --http` actually reaches this backend. Fixed by mirroring
  `LlamaCppBackend::new()`'s existing `Handle::try_current()` +
  `block_in_place` pattern.

---

## [0.10.3] - 2026-08-03
### Fixed
- Several user-facing strings (status/doctor output, error messages, help
  text, query-syntax doc comments) still referred to `qmd`, the project
  this was ported from, instead of `rqmd`. Notably, `init --help` described
  the directory it creates as `.qmd index` when the command actually
  creates `.rqmd/` — a genuine correctness bug in the help text, not just
  cosmetic drift.
### Documentation
- `BENCHMARK.md` corrected to reference the actual crate name (`rqmd_llm`),
  cache path (`~/.cache/rqmd/`), and index file (`.rqmd/index.sqlite`) used
  by this project, and to stop misattributing an ORT/CoreML noise message
  as a "qmd issue" when it's about this project's own code.

---

## [0.10.2] - 2026-08-03
### Fixed
- `update-homebrew-formula.sh` no longer overwrites its own template when
  rendering the formula, which previously destroyed the `RQMD_*` placeholders
  and caused a second run to silently ship stale version/SHA values.
- `build-release-notes.sh` no longer aborts under `set -Eeuo pipefail` when
  `## [Unreleased]` is empty, which is the expected steady state between
  releases.
- `stamp-changelog-refs.sh` no longer crashes with `unbound variable` under
  bash 3.2 (macOS's default `/bin/bash`) when a commit range contains zero
  commits.

---

## [0.10.1] - 2026-08-03
### Fixed
- `rqmd mcp` no longer leaves an orphaned daemon child process running when
  the parent's health-check times out (e.g. while a large model is still
  loading) — the child is now killed and reaped instead of abandoned.
- Pidfile ownership is now single-writer (the daemon process itself, once
  its listener is bound) instead of being written by both the parent and
  child, closing a race that could leave a stale or missing pidfile.

---

## [0.10.0] - 2026-08-03
### Security
- GGUF model downloads (`rqmd-llm`) now pin an explicit revision per model
  repo and verify the downloaded file's SHA-256 against a known-good hash
  before first use, closing a gap where in-transit tampering, a
  compromised mirror, or a corrupted download could silently swap in a
  different model with no verification. Verification runs on a fresh
  download only (trust-on-first-use), not on every cache hit.
- `HF_ENDPOINT` is now validated to require `https://`; a non-HTTPS or
  malformed value is rejected with an error instead of silently allowing
  model downloads to be redirected over an unencrypted or unexpected
  transport. The download path was also switched to a client that
  actually reads `HF_ENDPOINT`/`HF_HOME` (the previous one silently
  ignored both), so this check now has real effect and cache-directory
  reporting stays consistent with where models are actually downloaded.
- Third-party GitHub Actions (`dtolnay/rust-toolchain`, `Swatinem/rust-cache`,
  `cachix/install-nix-action`, `aquasecurity/trivy-action`) and
  GitHub-authored actions (`actions/checkout`, `actions/upload-artifact`,
  `actions/create-github-app-token`, `github/codeql-action/upload-sarif`)
  are now pinned to a full commit SHA (version tag kept as a trailing
  comment) instead of a mutable tag, closing a gap where a compromised or
  re-tagged upstream release could substitute malicious action code —
  most notably in `upload-assets`, which holds `contents: write` and a
  `GH_TOKEN`.
- `release.yml` checkouts that don't push back to the repository
  (`upload-assets`, `finalize-notes`, `sync-homebrew`) now set
  `persist-credentials: false`, matching the other workflows; the
  `release` job's own checkout keeps the default since it performs the
  actual `git push` of the release tag.
- `release.yml` no longer degrades to publishing an unsigned release tag
  when GPG signing isn't configured or fails — it now fails the job with
  an explicit error instead of a warning.
- Escaped `%`/`_` LIKE metacharacters (and the escape character itself)
  in the docid-prefix lookup (`rqmd-core::db::get_document_by_docid_prefix`),
  fixing non-deterministic document resolution for docids containing
  those characters. Not an injection risk (the query was already
  parameterized) — this was a match-semantics correctness bug.

---

## [0.9.0] - 2026-08-03
### Added
- MCP server now enforces an `Origin` allowlist (previously unset, which
  disabled the underlying library's Origin validation entirely), a request
  body size cap, and a hard cap on `multi_get` result counts.
### Changed
- `/health` now sits behind the same Host-header validation as `/mcp` and no
  longer leaks the daemon PID or index path in its response.
- Binding the MCP server to a non-loopback host now requires an explicit
  opt-in instead of a stderr warning plus automatic self-authorization.

---

## [0.8.1] - 2026-08-02
### Fixed
- `release.yml`: every future release body now gets commit/PR provenance
  stamped in — previously `stamp-changelog-refs.sh` was only ever invoked as a
  one-time manual backfill and was never wired into the pipeline itself.

---

## [0.8.0] - 2026-08-01
### Fixed
- `-c`/`--collection` on `query`, `search`, `vsearch`, and `multi-get` was
  declared as a single-valued flag even though it was documented as
  repeatable and OR-matched across collections. Passing `-c a -c b`
  silently kept only the last value (`b`) and dropped `a` — no error, no
  warning, just a narrower result set than requested. The flag is now
  genuinely repeatable, wired through the existing multi-collection
  plumbing already used by the MCP server. ([`989a8a0`](https://github.com/tylern91/rqmd/commit/989a8a0)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#34](https://github.com/tylern91/rqmd/pull/34)

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
  multi-term queries). ([`4cdba13`](https://github.com/tylern91/rqmd/commit/4cdba13)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#35](https://github.com/tylern91/rqmd/pull/35)

---

## [0.7.0] - 2026-08-01
### Added
- `rqmd similar <path|#docid>` finds documents most similar to an
  already-indexed one, reusing the existing HNSW index directly — no
  model load required. ([`d34f58c`](https://github.com/tylern91/rqmd/commit/d34f58c)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#33](https://github.com/tylern91/rqmd/pull/33)

### Changed
- `--format` is now a validated enum instead of a bare string: an
  unsupported value (e.g. a typo, or a format a given command doesn't
  support like `get --format md`) now fails fast instead of silently
  falling back to human-readable CLI rendering. ([`d34f58c`](https://github.com/tylern91/rqmd/commit/d34f58c)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#33](https://github.com/tylern91/rqmd/pull/33)
- `--format files` now emits real, absolute filesystem paths (collection
  root joined with the relative path), one per line, instead of the old
  synthetic `#docid,score,file` shape — the old output wasn't a real
  path and couldn't be piped to `xargs`/`cat`. Scripts parsing the old
  `files` shape need to switch to plain path handling. ([`d34f58c`](https://github.com/tylern91/rqmd/commit/d34f58c)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#33](https://github.com/tylern91/rqmd/pull/33)

### Fixed
- Fixed stale single-R `RQMD_*` env var names in `rqmd-cli/src/store.rs`
  doc comments and a stale `vectors.usearch` path in the README; the var
  actually read is `RRQMD_INDEX_DIR`/`RRQMD_INFERENCE_BACKEND` and the
  on-disk file is `hnsw.usearch`. ([`d34f58c`](https://github.com/tylern91/rqmd/commit/d34f58c)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#33](https://github.com/tylern91/rqmd/pull/33)

---

## [0.6.5] - 2026-08-01
### Fixed
- `mcp --daemon` on a port that's already occupied used to print "started"
  and exit 0 even though the child died immediately, because the child's
  stderr went to `/dev/null` and the parent never checked whether the
  server actually came up. The daemon now writes a pidfile, and the parent
  pre-checks the port, waits for the daemon's `/health` endpoint after
  spawning, and on failure surfaces the real log tail and exits non-zero
  instead of reporting false success. ([`4aa9227`](https://github.com/tylern91/rqmd/commit/4aa9227)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#32](https://github.com/tylern91/rqmd/pull/32)
- Added `mcp stop`/`mcp status`, backed by the same pidfile. Identity is
  confirmed by cross-checking the recorded pid against the daemon's own
  `/health` response (never a bare pid match, since pids get recycled),
  so `stop`/`status` never signal an unrelated process and a stale
  pidfile never blocks a fresh start. ([`4aa9227`](https://github.com/tylern91/rqmd/commit/4aa9227)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#32](https://github.com/tylern91/rqmd/pull/32)
- The daemon now shuts down gracefully on SIGTERM/ctrl-c instead of
  needing to be killed as an orphan, logs to a real file instead of
  `/dev/null`, and `--daemon` now implies `--http` instead of conflicting
  with it. `--host`/`RRQMD_MCP_HOST` is supported for non-loopback binds,
  with a loud warning naming what's exposed (unauthenticated full-text
  and semantic search, plus arbitrary file content via `get`). ([`4aa9227`](https://github.com/tylern91/rqmd/commit/4aa9227)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#32](https://github.com/tylern91/rqmd/pull/32)
- Docs: removed the `SYNTAX.md` MCP `searches` array and REST `/query`
  endpoint, neither of which exist — the `query`/`search` MCP tools take
  the same `query` string as the CLI, served over the MCP protocol
  itself (stdio or `/mcp`), with `/health` as the only bespoke REST
  endpoint. Also removed the dead `RRQMD_CI`/`QMD_CI` README rows —
  nothing in the codebase reads that variable. ([`4aa9227`](https://github.com/tylern91/rqmd/commit/4aa9227)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#32](https://github.com/tylern91/rqmd/pull/32)

---

## [0.6.4] - 2026-08-01
### Fixed
- `embed_fingerprint` (used to detect a stale vector index) was derived
  from hardcoded literals that happened to match the chunking constants
  at one point in time, so a chunking change was permanently
  undetectable. It's now derived from the actual chunking constants. ([`6edfc27`](https://github.com/tylern91/rqmd/commit/6edfc27)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#31](https://github.com/tylern91/rqmd/pull/31)
- Embedding now applies the model's documented query/passage prompt
  asymmetry instead of embedding queries and document chunks
  identically; HyDE's hypothetical-document text is embedded on the
  passage side, since that's the subspace it needs to land in to work. ([`6edfc27`](https://github.com/tylern91/rqmd/commit/6edfc27)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#31](https://github.com/tylern91/rqmd/pull/31)
- The llama.cpp backend now L2-normalizes its embedding output, matching
  the documented contract and the ONNX Runtime backend's existing
  behavior (previously the two backends silently disagreed). Cosine
  similarity is scale-invariant, so result ordering is unaffected. ([`6edfc27`](https://github.com/tylern91/rqmd/commit/6edfc27)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#31](https://github.com/tylern91/rqmd/pull/31)
- `doctor`, `embed` (without `--rebuild`), `query`, and `vsearch` now
  share one stale-fingerprint check that warns instead of silently
  degrading or auto-rebuilding, and correctly flags a single uniformly
  stale index — not just a mix of fingerprints. ([`6edfc27`](https://github.com/tylern91/rqmd/commit/6edfc27)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#31](https://github.com/tylern91/rqmd/pull/31)
- Fixed stale single-R `RQMD_*` env var names in `rqmd-llm` doc
  comments; the vars actually read are `RRQMD_INFERENCE_BACKEND`,
  `RRQMD_ORT_EP`, and `RRQMD_FORCE_CPU`. ([`6edfc27`](https://github.com/tylern91/rqmd/commit/6edfc27)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#31](https://github.com/tylern91/rqmd/pull/31)

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
  a path that no longer existed. ([`8383dfc`](https://github.com/tylern91/rqmd/commit/8383dfc)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#30](https://github.com/tylern91/rqmd/pull/30)
- `collection remove` no longer leaves every document, its content,
  vectors, and search-index entries fully searchable after the collection
  is supposedly gone — it now purges everything it owns (content/vectors
  no longer referenced by any other collection are dropped too, since
  content is deduplicated globally by hash). ([`8383dfc`](https://github.com/tylern91/rqmd/commit/8383dfc)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#30](https://github.com/tylern91/rqmd/pull/30)
- Re-indexing an existing path could silently feed a stale/unrelated
  document id into the search index (SQLite doesn't advance
  `last_insert_rowid()` on an upsert's `ON CONFLICT DO UPDATE` arm); fixed
  by reading the id back with `RETURNING id`. ([`8383dfc`](https://github.com/tylern91/rqmd/commit/8383dfc)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#30](https://github.com/tylern91/rqmd/pull/30)

---

## [0.6.2] - 2026-08-01
### Fixed
- Collection scoping (`-c <collection>`) no longer returns false-empty when
  the target collection is a small minority of a larger corpus: BM25 now
  pushes a must-clause down onto the indexed `filepath` field to narrow the
  candidate pool before the existing exact-prefix post-filter, and vector
  search widens its candidate count until enough in-scope hits are found. ([`5a2e158`](https://github.com/tylern91/rqmd/commit/5a2e158)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#29](https://github.com/tylern91/rqmd/pull/29)
- `vsearch`/hybrid search no longer return the same document once per chunk:
  both vector search and RRF fusion now dedupe to the best-scoring chunk per
  document. This also removes RRF's systematic bias toward long documents,
  which previously accumulated a rank-based score once per chunk.
  `-n`/`--limit` is no longer silently capped at 20 — the internal rerank
  candidate pool now scales with the requested limit
  (`clamp(limit*2, RERANK_CANDIDATE_LIMIT, 100)`), warning when it's capped. ([`5a2e158`](https://github.com/tylern91/rqmd/commit/5a2e158)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#29](https://github.com/tylern91/rqmd/pull/29)
- A query containing FTS special syntax (e.g. a colon read as a field
  specifier) no longer degrades to a silent empty result — parsing is now
  lenient, with a warning logged on partial parse failures. ([`5a2e158`](https://github.com/tylern91/rqmd/commit/5a2e158)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#29](https://github.com/tylern91/rqmd/pull/29)
- `collection exclude` (`include_by_default = 0`) is no longer a no-op:
  default-scope queries now resolve the included-collection set once per
  query and skip scoping entirely when every collection is included. ([`5a2e158`](https://github.com/tylern91/rqmd/commit/5a2e158)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#29](https://github.com/tylern91/rqmd/pull/29)

---

## [0.6.1] - 2026-08-01
### Fixed
- `collection add`/`index update` no longer report success while indexing
  zero files: a collection root under a dot-directory (e.g. a dotfiles
  checkout) is now resolved relative to the collection root before the
  exclusion scan, instead of matching dot-components in the absolute path.
  Dot-directories nested inside the tree are still excluded by default; pass
  `--hidden` to include them. ([`ae75815`](https://github.com/tylern91/rqmd/commit/ae75815)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#28](https://github.com/tylern91/rqmd/pull/28)
- Frontmatter is now parsed: every note used to be titled `"---"` because the
  YAML fence was indexed verbatim. Title now resolves from frontmatter
  `title:` → first `#` heading → filename stem, the fence is stripped from
  indexed/searched text, and `tags:`/`aliases:` values are folded in as extra
  search terms. Content hashing runs over the stripped text, so a
  metadata-only frontmatter edit no longer forces a full re-embed. ([`ae75815`](https://github.com/tylern91/rqmd/commit/ae75815)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#28](https://github.com/tylern91/rqmd/pull/28)
- Multi-glob collection masks (`**/*.{md,mdx,txt}`, comma-separated patterns)
  used to match zero or only some files, and `collection add` vs.
  `index update` could silently disagree on membership. Both now share one
  glob-set builder that errors loudly on a malformed pattern instead of
  matching nothing. ([`ae75815`](https://github.com/tylern91/rqmd/commit/ae75815)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#28](https://github.com/tylern91/rqmd/pull/28)
- Unreadable files were previously skipped with no count; the indexing
  summary now reports skips by reason (not UTF-8, permission denied, I/O
  error). `collection add <file>` on a non-directory path now fails instead
  of persisting a document with an empty relative path. ([`ae75815`](https://github.com/tylern91/rqmd/commit/ae75815)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#28](https://github.com/tylern91/rqmd/pull/28)
- README: corrected the documented index-storage path — `dirs::cache_dir()`
  resolves to `~/Library/Caches/rqmd/` on macOS, not `~/.cache/rqmd/`. ([`ae75815`](https://github.com/tylern91/rqmd/commit/ae75815)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#28](https://github.com/tylern91/rqmd/pull/28)

### Removed
- `example-index.yml`, a stale artifact demonstrating the now-fixed
  multi-extension glob bug. ([`ae75815`](https://github.com/tylern91/rqmd/commit/ae75815)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#28](https://github.com/tylern91/rqmd/pull/28)

---

## [0.6.0] - 2026-07-31
### Added
- Each GGUF model (embed, rerank, generate) now loads lazily on first use
  instead of all three loading unconditionally in `LlamaCppBackend::new`.
  `status` and `doctor` remain load-free. ([`4958377`](https://github.com/tylern91/rqmd/commit/4958377)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#26](https://github.com/tylern91/rqmd/pull/26)
- Idle model eviction: `RRQMD_MODEL_IDLE_TTL` (default 300s, `0` disables)
  controls how long a loaded model may sit unused before a periodic sweep
  releases it. Without this, query expansion being on by default meant the
  ~2 GB generate model would load within a few queries and never be freed,
  ratcheting a long-lived daemon's RSS upward (measured at 2.97 GB on a
  6-day-old daemon that should idle around 750 MB). ([`4958377`](https://github.com/tylern91/rqmd/commit/4958377)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#26](https://github.com/tylern91/rqmd/pull/26)

### Fixed
- Corrected two places that claimed `rerank_n_ctx` defaults to 512 (module
  doc in `rqmd-llm`, `BENCHMARK.md`) — the shipped default is 2048; 512 is a
  documented tuning option for the 448 MiB KV budget on Apple Silicon, not
  the default. ([`4958377`](https://github.com/tylern91/rqmd/commit/4958377)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#26](https://github.com/tylern91/rqmd/pull/26)

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
  error instead of attempting a write if a mutating code path is reached. ([`d34feac`](https://github.com/tylern91/rqmd/commit/d34feac)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#25](https://github.com/tylern91/rqmd/pull/25)

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
  `rqmd ls`/`documents.path`. ([`2894af4`](https://github.com/tylern91/rqmd/commit/2894af4)) by [@tylern91](https://github.com/tylern91) in [#23](https://github.com/tylern91/rqmd/pull/23)

---

## [0.5.0] - 2026-07-13
### Fixed
- `multi-get` (CLI and MCP) matched plain path fragments with an unanchored substring
  check, so a pattern like `README.md` could silently also return `OLD-README.md` —
  worst case for a caller that expects one document and gets the wrong one with no
  error. Matching is now anchored at a `/` path-segment boundary; an explicit glob
  (e.g. `docs/*.md`) is required for fragment/prefix matching, matching the MCP
  tool's documented behavior. ([`6d6bf58`](https://github.com/tylern91/rqmd/commit/6d6bf58)) by [@tylern91](https://github.com/tylern91) in [#22](https://github.com/tylern91/rqmd/pull/22)
- `#docid` prefix lookups (`get_document_by_docid_prefix`) had no deterministic
  tie-break on a hash-prefix collision (`LIMIT 1` with no `ORDER BY`), so the
  resolved document could vary run to run. Now ordered by `(collection, path)`
  before `LIMIT 1`. ([`6d6bf58`](https://github.com/tylern91/rqmd/commit/6d6bf58)) by [@tylern91](https://github.com/tylern91) in [#22](https://github.com/tylern91/rqmd/pull/22)

### Added
- `--no-expand` flag (`RRQMD_NO_EXPAND` env) on `rqmd query`, and an `expand: false`
  input on the MCP `query` tool, to skip the LLM query-expansion/HyDE round-trip.
  BM25 + vector retrieval and RRF fusion still run — this is pure hybrid retrieval
  without the extra generation call, for corpora or callers that don't need it. ([`6d6bf58`](https://github.com/tylern91/rqmd/commit/6d6bf58)) by [@tylern91](https://github.com/tylern91) in [#22](https://github.com/tylern91/rqmd/pull/22)

### Changed
- `multi-get`'s plain-pattern resolution now pushes down to SQL
  (`find_documents_by_needles`) instead of loading every document in the index and
  filtering in Rust — avoids a full-table scan for the common explicit-path case. ([`6d6bf58`](https://github.com/tylern91/rqmd/commit/6d6bf58)) by [@tylern91](https://github.com/tylern91) in [#22](https://github.com/tylern91/rqmd/pull/22)
- CLI and MCP `multi-get` now share one resolution implementation
  (`rqmd_core::resolve`), removing a duplicated `glob_match`/matching function that
  had drifted between the two crates. ([`6d6bf58`](https://github.com/tylern91/rqmd/commit/6d6bf58)) by [@tylern91](https://github.com/tylern91) in [#22](https://github.com/tylern91/rqmd/pull/22)

### CI
- `rust.yml`'s `upload-artifact` step bumped from `@v4` to `@v7`. ([`6d6bf58`](https://github.com/tylern91/rqmd/commit/6d6bf58)) by [@tylern91](https://github.com/tylern91) in [#22](https://github.com/tylern91/rqmd/pull/22)

---

## [0.4.2] - 2026-07-13
### Fixed
- The per-collection pre-update hook (`rqmd collection update-cmd`, shown as
  `Hook:` in `collection show`) was persisted, loaded, and displayed but never
  executed — `rqmd update` walked and reindexed the collection directory
  without ever running it. `run_update` now spawns the stored command via
  `sh -c` with the collection's directory as CWD before walking it, warning
  (not failing) on a non-zero exit or spawn error. ([`c46dfbb`](https://github.com/tylern91/rqmd/commit/c46dfbb)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#21](https://github.com/tylern91/rqmd/pull/21)

---

## [0.4.1] - 2026-07-08
### Fixed
- `rqmd --version` and the MCP server's `server_info` were pinned at `0.2.0` since
  the v0.3.0 release — every crate `Cargo.toml` still hardcoded the old literal, and
  the release pipeline built binaries from the tagged tree without ever touching it.
  Version is now a single `[workspace.package]` value inherited by every crate
  (`version.workspace = true`), so one line bumps all of them. ([`4e65bbf`](https://github.com/tylern91/rqmd/commit/4e65bbf)) by [@tylern91](https://github.com/tylern91) in [#20](https://github.com/tylern91/rqmd/pull/20)
- Added `scripts/check-version-sync.sh`, wired into CI, which fails the build if the
  workspace version and the CHANGELOG's top release heading disagree — the exact
  drift that caused this bug. ([`4e65bbf`](https://github.com/tylern91/rqmd/commit/4e65bbf)) by [@tylern91](https://github.com/tylern91) in [#20](https://github.com/tylern91/rqmd/pull/20)
- Added a release-time assertion (`upload-assets` job) that the built binary's
  `--version` output matches the tag being released, as a final safety net. ([`4e65bbf`](https://github.com/tylern91/rqmd/commit/4e65bbf)) by [@tylern91](https://github.com/tylern91) in [#20](https://github.com/tylern91/rqmd/pull/20)

---

## [0.4.0] - 2026-07-08
### Added
- `rqmd doctor` now warns when the index contains chunks embedded under more than
  one embedding fingerprint (stale vectors left behind by a model or chunking
  change), listing per-fingerprint chunk counts and recommending `rqmd embed --rebuild`. ([`2f383b5`](https://github.com/tylern91/rqmd/commit/2f383b5)) by [@tylern91](https://github.com/tylern91) in [#19](https://github.com/tylern91/rqmd/pull/19)
- Test coverage for special-character paths (`#`, `&`, spaces, `[]`, `()`) round-tripping
  through index → search → get, and dotted-version (e.g. `2026.4.10`) BM25 tokenization. ([`2f383b5`](https://github.com/tylern91/rqmd/commit/2f383b5)) by [@tylern91](https://github.com/tylern91) in [#19](https://github.com/tylern91/rqmd/pull/19)

### Changed
- **Breaking (MCP):** the `query`, `search`, and `multi_get` MCP tools now take
  `collections: [...]` (an array) instead of `collection` (a single string) — matches
  qmd 2.6.3's multi-collection filter. Existing MCP client configs passing a bare
  string must switch to an array; omitting the field still searches all collections. ([`2f383b5`](https://github.com/tylern91/rqmd/commit/2f383b5)) by [@tylern91](https://github.com/tylern91) in [#19](https://github.com/tylern91/rqmd/pull/19)
- SQLite `busy_timeout` raised from 5s to 30s so a long embed batch no longer wedges
  a concurrent MCP/CLI reader. ([`2f383b5`](https://github.com/tylern91/rqmd/commit/2f383b5)) by [@tylern91](https://github.com/tylern91) in [#19](https://github.com/tylern91/rqmd/pull/19)

### CI
- `security.yml`'s Trivy checkout now pins `fetch-depth: 1` and
  `persist-credentials: false`, matching the other workflow jobs' explicit settings. ([`2f383b5`](https://github.com/tylern91/rqmd/commit/2f383b5)) by [@tylern91](https://github.com/tylern91) in [#19](https://github.com/tylern91/rqmd/pull/19)

---

## [0.3.1] - 2026-07-05

### Fixed
- Release binaries and the Homebrew tap now publish correctly; v0.3.0 shipped
  without assets due to a GitHub immutable-release conflict in the upload pipeline. ([`ea99569`](https://github.com/tylern91/rqmd/commit/ea99569)) by [@tylern91](https://github.com/tylern91) in [#17](https://github.com/tylern91/rqmd/pull/17)

---

## [0.3.0] - 2026-07-05

### Added
- Homebrew tap (`brew tap tylern91/rqmd && brew install rqmd`) — downloads a prebuilt binary; no Rust toolchain or cmake required ([`acbee10`](https://github.com/tylern91/rqmd/commit/acbee10)) by [@tylern91](https://github.com/tylern91) in [#16](https://github.com/tylern91/rqmd/pull/16)
- `cargo install --git https://github.com/tylern91/rqmd --locked rqmd-cli` one-liner install documented in README ([`acbee10`](https://github.com/tylern91/rqmd/commit/acbee10)) by [@tylern91](https://github.com/tylern91) in [#16](https://github.com/tylern91/rqmd/pull/16)
- Prebuilt release binaries (macOS arm64, Linux x86_64) attached to every GitHub Release as `rqmd-<version>-<platform>.tar.gz` with `.sha256` sidecar files ([`acbee10`](https://github.com/tylern91/rqmd/commit/acbee10)) by [@tylern91](https://github.com/tylern91) in [#16](https://github.com/tylern91/rqmd/pull/16)

### Changed
- README Installation section now leads with Homebrew and `cargo install --git`, followed by prebuilt binary download, then the existing from-source path ([`acbee10`](https://github.com/tylern91/rqmd/commit/acbee10)) by [@tylern91](https://github.com/tylern91) in [#16](https://github.com/tylern91/rqmd/pull/16)

### CI
- `release.yml`: new `upload-assets` matrix job builds and attaches platform binaries after each release tag; optional `HOMEBREW_TAP_TOKEN` secret triggers automatic formula sync to `tylern91/homebrew-rqmd` ([`acbee10`](https://github.com/tylern91/rqmd/commit/acbee10)) by [@tylern91](https://github.com/tylern91) in [#16](https://github.com/tylern91/rqmd/pull/16)
- `scripts/update-homebrew-formula.sh`: new script fills sha256 values into `packaging/homebrew/rqmd.rb` and optionally pushes to the tap repo ([`acbee10`](https://github.com/tylern91/rqmd/commit/acbee10)) by [@tylern91](https://github.com/tylern91) in [#16](https://github.com/tylern91/rqmd/pull/16)

---

## [0.2.3] - 2026-07-05

### Added
- Security scanning CI: new `.github/workflows/security.yml` runs Trivy `fs` scan on every
  PR and push to `main`. CRITICAL + HIGH findings are uploaded to the GitHub Security tab
  (code-scanning alerts, SARIF). A second blocking step hard-fails the PR check on any
  CRITICAL vulnerability with a known fix (`ignore-unfixed: true`). HIGH findings are
  recorded but non-blocking. ([`a5ce3d4`](https://github.com/tylern91/rqmd/commit/a5ce3d4)) by [@tylern91](https://github.com/tylern91) in [#15](https://github.com/tylern91/rqmd/pull/15)

### Changed
- Binary assets are now tracked with Git LFS. A `.gitattributes` file declares LFS
  patterns for images (`*.png`, `*.jpg`, `*.gif`, `*.webp`, `*.pdf`), ML model files
  (`*.gguf`, `*.onnx`, `*.bin`), and archives (`*.tar.gz`, `*.zip`). The existing
  `assets/qmd-architecture.png` has been converted to a pointer. New binaries committed
  to the repo will land in LFS automatically. ([`a5ce3d4`](https://github.com/tylern91/rqmd/commit/a5ce3d4)) by [@tylern91](https://github.com/tylern91) in [#15](https://github.com/tylern91/rqmd/pull/15)

---

## [0.2.2] - 2026-07-05

### Fixed
- `rqmd query` (and `search`/`vsearch`) no longer panics with `assertion failed: self.is_char_boundary` when a result snippet contains multi-byte UTF-8 characters near the truncation boundary (#chunking) ([`c20009c`](https://github.com/tylern91/rqmd/commit/c20009c)) by [@tylern91](https://github.com/tylern91) in [#13](https://github.com/tylern91/rqmd/pull/13)

---

## [0.2.1] - 2026-07-04
### Fixed

- `rqmd context check`: falsely reported all collections as MISSING even when
  contexts were correctly set via `rqmd context add`. Root cause was an
  `rrqmd://` (double-r) URI-scheme typo in the lookup key inside `check()`,
  while `context add` stores the canonical single-r `rqmd://` key. Fixed by
  extracting `collection_context_key()` in `db.rs` as the shared key-builder
  used by both `check()` and `get_context_for_collection`, eliminating the
  duplicated literal that allowed the drift. Regression test added. ([`7d1979e`](https://github.com/tylern91/rqmd/commit/7d1979e)) by [@tylern91](https://github.com/tylern91) in [#12](https://github.com/tylern91/rqmd/pull/12)

---

## [0.2.0] - 2026-07-03
### Added

- `rqmd status`: Models section now shows the exact downloaded `.gguf` filename
  alongside the HuggingFace repo URL (e.g. `└─ embeddinggemma-300M-Q8_0.gguf`
  under the repo link). Previously only the repo URL was shown, leaving the actual
  quantization variant opaque to the user. ([`e4c23d0`](https://github.com/tylern91/rqmd/commit/e4c23d0)) by [@tylern91](https://github.com/tylern91) in [#9](https://github.com/tylern91/rqmd/pull/9)

- `rqmd bench`: new in-process query-latency phase. When `--index-dir` points at a
  real index (`index.sqlite` present), the bench opens the store once (amortising
  model load), warms up each mode, then reports **p50/p99 in µs** across all query–
  round combinations for: BM25, vector, hybrid-no-rerank, and hybrid-with-rerank.
  Previously `bench` timed only embedding throughput on a hardcoded 10-text array
  and ignored `index_dir` entirely. Results are now printed per-mode as each
  completes (no batching at the end). ([`e4c23d0`](https://github.com/tylern91/rqmd/commit/e4c23d0)) by [@tylern91](https://github.com/tylern91) in [#9](https://github.com/tylern91/rqmd/pull/9)

- `BENCHMARK.md`: new Full-Corpus Runtime Benchmark section. Runs on a large local
  markdown corpus (≈62.9k documents, 210k vectors, 1.5 GB index) on Apple M-series.
  Records end-to-end indexing rate, in-process embed throughput (Metal GPU and CPU),
  query latency p50/p99 per mode (BM25 / Vec / Hybrid), and search quality Hit@K.
  All numbers are aggregate only — no corpus paths or document content. ([`e4c23d0`](https://github.com/tylern91/rqmd/commit/e4c23d0)) by [@tylern91](https://github.com/tylern91) in [#9](https://github.com/tylern91/rqmd/pull/9)

- `scripts/install.sh`: content-aware install that replaces `cargo install --path`.
  Uses `cargo build` fingerprinting (content-based, not version-based) then atomically
  copies the fresh binary to `~/.cargo/bin/rqmd`. Supports `RQMD_PROFILE` env var and
  passes extra args through (e.g. `./scripts/install.sh --features ort-backend`). ([`3416488`](https://github.com/tylern91/rqmd/commit/3416488)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#8](https://github.com/tylern91/rqmd/pull/8)
- File exclusion on `collection add` and `rqmd update`: new `--ignore <PATTERN>` flag
  accepts gitignore-style glob patterns (powered by `globset`). Built-in exclusions
  always apply: hidden paths (`.`-prefixed), `node_modules`, `vendor`, `dist`, `build`,
  `target`, `.cache`. Patterns are stored in the collection record and re-applied on
  every subsequent `update` run for that collection. ([`3416488`](https://github.com/tylern91/rqmd/commit/3416488)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#8](https://github.com/tylern91/rqmd/pull/8)
- `rqmd mcp --daemon`: self-respawns as a background HTTP process (implies `--http`)
  and exits, leaving the server running detached. Existing `--http`/`--port` flags
  are unchanged. ([`3416488`](https://github.com/tylern91/rqmd/commit/3416488)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#8](https://github.com/tylern91/rqmd/pull/8)
- GPU feature flags in `rqmd-llm` and `rqmd-cli`: `metal` (default on, no behaviour
  change for existing macOS builds), `cuda`, and `vulkan`. CPU-only builds:
  `--no-default-features`. Previously `metal` was hardcoded in the `llama-cpp-2` dep. ([`3416488`](https://github.com/tylern91/rqmd/commit/3416488)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#8](https://github.com/tylern91/rqmd/pull/8)

### Fixed

- cmake 4.x is now supported for building `llama-cpp-sys-2` on macOS. The previous
  belief that cmake 4.x would break the llama.cpp CMake build was Python-specific
  (the Python `cmake` pip package had an incompatibility); the Rust `llama-cpp-2`
  crate builds cleanly with cmake 4.x. The CI `pip install "cmake<4"` pin has been
  removed from `rust.yml` (both `build-macos` and `dist-binary` jobs). The README
  troubleshooting block and `flake.nix` / `nix.yml` comments have been updated
  accordingly. ([`e4c23d0`](https://github.com/tylern91/rqmd/commit/e4c23d0)) by [@tylern91](https://github.com/tylern91) in [#9](https://github.com/tylern91/rqmd/pull/9)

- Environment variable names corrected throughout documentation. All `rqmd` env vars
  use the `RRQMD_` prefix (double-R), matching what the code actually reads. The
  docs previously showed `RQMD_*` (single-R), which silently had no effect. Affected:
  `README.md`, `BENCHMARK.md`, `scripts/crosscheck.sh`. Correct names:
  `RRQMD_INDEX_DIR`, `RRQMD_INFERENCE_BACKEND`, `RRQMD_ORT_EP`, `RRQMD_FORCE_CPU`,
  `RRQMD_CI`, `RRQMD_VERBOSE`. ([`e4c23d0`](https://github.com/tylern91/rqmd/commit/e4c23d0)) by [@tylern91](https://github.com/tylern91) in [#9](https://github.com/tylern91/rqmd/pull/9)

- `rqmd update`: unchanged documents no longer re-added to the Tantivy FTS index.
  Previously `index_document_fts_only` always called `fts.add_document` even when the
  content hash was identical, causing duplicate Tantivy segments that inflated scores
  and grew the on-disk index on every `update` run. ([`3416488`](https://github.com/tylern91/rqmd/commit/3416488)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#8](https://github.com/tylern91/rqmd/pull/8)
- File exclusion: non-UTF-8 path components now correctly exclude the path (fail-closed)
  instead of silently passing all exclusion checks via `unwrap_or("")`. ([`3416488`](https://github.com/tylern91/rqmd/commit/3416488)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#8](https://github.com/tylern91/rqmd/pull/8)

### Changed

- `BENCHMARK.md`: removed "Phase 0" internal-phase framing; fixed stale `QMD_*` env
  vars to `RQMD_*`; removed stale "Phase 6" internal reference. All tables and
  performance comparison data preserved. ([`3416488`](https://github.com/tylern91/rqmd/commit/3416488)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#8](https://github.com/tylern91/rqmd/pull/8)
- `README.md`: six new sections — *Excluding files*, *Models*, *MCP server*, *Where
  data lives*, *Differences from qmd*, *Migrating from qmd*. QMD inspiration credit
  added to tagline and Acknowledgements. Install docs now reference `scripts/install.sh`. ([`3416488`](https://github.com/tylern91/rqmd/commit/3416488)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#8](https://github.com/tylern91/rqmd/pull/8)
- All four `Cargo.toml` files: added `publish = false`, `repository`, `keywords`,
  `categories` metadata. `rqmd` package name is taken on crates.io by a separate
  project (`stn/rqmd`); `publish = false` guards against accidental publish. ([`3416488`](https://github.com/tylern91/rqmd/commit/3416488)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#8](https://github.com/tylern91/rqmd/pull/8)
- Stale `qmd-cli` / `target/dist/qmd` / `QMD_INDEX_DIR` references fixed in
  `.cargo/config.toml`, `flake.nix`, and `scripts/crosscheck.sh`. ([`3416488`](https://github.com/tylern91/rqmd/commit/3416488)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#8](https://github.com/tylern91/rqmd/pull/8)

---

## [0.1.6] - 2026-06-30
### Added

- Phase 4: HyDE / query expansion — generation model (Qwen3-1.7B Q8_0) downloaded
  eagerly alongside embed/rerank; free-form constrained generation with ChatML prompt;
  `lex:`/`vec:`/`hyde:` expansion results fused via RRF (expansion weight 1.0,
  original weight 2.0); non-fatal fallback (warn + original results) on any error. ([`9799131`](https://github.com/tylern91/rqmd/commit/9799131)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#7](https://github.com/tylern91/rqmd/pull/7)
- Typed-line query parser (`rqmd-core::query::parse_query`): routes `lex:`/`vec:`/`hyde:`/`intent:`
  typed-doc mode directly to their respective search methods; plain lines run expansion. ([`9799131`](https://github.com/tylern91/rqmd/commit/9799131)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#7](https://github.com/tylern91/rqmd/pull/7)
- `--intent <STRING>` flag on `rqmd query` and `intent` field in MCP `QueryInput`;
  intent steers the expansion prompt, reranker cross-encoder query, and snippet term
  selection. ([`9799131`](https://github.com/tylern91/rqmd/commit/9799131)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#7](https://github.com/tylern91/rqmd/pull/7)

### Fixed

- Generation model was never downloaded or used: `generate_constrained` was a stub that
  `bail!()`ed on all backends and the expansion step was skipped. ([`9799131`](https://github.com/tylern91/rqmd/commit/9799131)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#7](https://github.com/tylern91/rqmd/pull/7)
- Generation model repo name was wrong (`ggml-org/Qwen3-1.7B-Q8_0-GGUF` does not exist;
  correct: `ggml-org/Qwen3-1.7B-GGUF`) and filename casing was wrong (`qwen3-1.7b-q8_0.gguf`
  → `Qwen3-1.7B-Q8_0.gguf`). ([`9799131`](https://github.com/tylern91/rqmd/commit/9799131)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#7](https://github.com/tylern91/rqmd/pull/7)
- GBNF grammar sampling caused uncatchable process aborts (`GGML_ASSERT(!stacks.empty())`
  via C FFI when a multi-byte token drove the grammar into a dead state); replaced with
  free-form generation (temp/top_k/top_p/dist sampler chain) + lenient line parsing. ([`9799131`](https://github.com/tylern91/rqmd/commit/9799131)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#7](https://github.com/tylern91/rqmd/pull/7)

---

## [0.1.5] - 2026-06-30

### Fixed

- `doctor`: fix model-cache check always reporting "not cached" on macOS. Root
  cause: the check used `dirs::cache_dir()` (→ `~/Library/Caches/huggingface/hub`)
  while hf-hub stores models in `~/.cache/huggingface/hub`. Replaced the manual
  path rebuild with a `rqmd_llm::model_cache_report()` helper that delegates to
  `hf_hub::Cache::from_env()`, so the path matches the actual downloader and
  `HF_HOME` overrides are honoured. ([`73231ca`](https://github.com/tylern91/rqmd/commit/73231ca)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#6](https://github.com/tylern91/rqmd/pull/6)
- `doctor`: add Generation model (`Qwen3-1.7B`) to the model-cache report (it was
  missing; it downloads on first HyDE query expansion, so "not cached" is accurate
  until first use). ([`73231ca`](https://github.com/tylern91/rqmd/commit/73231ca)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#6](https://github.com/tylern91/rqmd/pull/6)

## [0.1.4] - 2026-06-30

### Fixed

- `update`: replace hard-coded 60-column space-pad clear with `\r\x1b[2K` so the
  progress line is fully erased before each collection's `Indexed:` summary,
  regardless of terminal width or path length. ([`1fa9c72`](https://github.com/tylern91/rqmd/commit/1fa9c72)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#5](https://github.com/tylern91/rqmd/pull/5)
- `status`, `embed`, `update`, `doctor`: fix phantom `Pending: N need embedding`
  that `rqmd embed` never cleared. Root cause: the "needs embedding" COUNT query
  was body-blind — it counted empty-body documents (hash = SHA-256 of `""`) as
  pending, but the embed loop skips empty bodies. Centralized the query into
  `db::count_docs_needing_embed` with a `JOIN content … AND length(c.doc) > 0`
  filter so the count matches what embed will actually process. ([`1fa9c72`](https://github.com/tylern91/rqmd/commit/1fa9c72)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#5](https://github.com/tylern91/rqmd/pull/5)

## [0.1.3] - 2026-06-29

### Fixed

- `update`: show real file total in progress (`Indexing: N/total`) by pre-collecting
  matching paths before the index loop; previously showed a literal `?`. ([`d10eab2`](https://github.com/tylern91/rqmd/commit/d10eab2)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#4](https://github.com/tylern91/rqmd/pull/4)
- `update`, `embed`, `collection add`: fix `term_width()` on Apple Silicon — `ioctl`
  must be declared variadic (`...`) to match the arm64 AAPCS64 calling convention;
  the non-variadic declaration put the `Winsize*` argument in the wrong register,
  causing `term_width()` to always return `None` and the width-clamp to never engage.
  Progress lines now overwrite in place instead of spawning a new line per update. ([`d10eab2`](https://github.com/tylern91/rqmd/commit/d10eab2)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#4](https://github.com/tylern91/rqmd/pull/4)
- `update`, `embed`, `collection add`: harden progress rendering by emitting
  `\r\x1b[2K` (erase-line) before each update and using `unwrap_or(80)` as fallback
  width so a width-detection miss can no longer cause line wrap. ([`d10eab2`](https://github.com/tylern91/rqmd/commit/d10eab2)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#4](https://github.com/tylern91/rqmd/pull/4)
- `cli`: bump `rqmd-cli` crate version so `cargo install --path` without `--force`
  correctly detects and installs new builds. ([`d10eab2`](https://github.com/tylern91/rqmd/commit/d10eab2)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#4](https://github.com/tylern91/rqmd/pull/4)

---

## [0.1.2] - 2026-06-29
### Added

- `embed`: display bytes/s throughput in progress bar (matches qmd's `formatBytes/s` metric).
  Progress line now shows: `bar% input · N chunks · D/T docs · X.X MB/s · ETA T` ([`45c6b0f`](https://github.com/tylern91/rqmd/commit/45c6b0f)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#3](https://github.com/tylern91/rqmd/pull/3)

### Fixed

- `embed`, `update`, `collection add`: clamp progress lines to terminal width via
  `term_width()` / `fit_to_width()` helpers in `format.rs`; prevents multiline smear
  when paths or stats exceed the terminal width. Progress is suppressed when not a TTY. ([`45c6b0f`](https://github.com/tylern91/rqmd/commit/45c6b0f)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#3](https://github.com/tylern91/rqmd/pull/3)
- `update`: fix advisory message branding — was `'qmd embed'`, now `'rqmd embed'`. ([`45c6b0f`](https://github.com/tylern91/rqmd/commit/45c6b0f)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#3](https://github.com/tylern91/rqmd/pull/3)
- `embed`: fix `UNIQUE constraint failed: content_vectors.vid` crash — reconcile
  HNSW `next_vid` with `MAX(content_vectors.vid)` in SQLite on startup; add in-run
  hash dedup to stop duplicate-hash drift; add `--rebuild` flag and divergence advisory. ([`45c6b0f`](https://github.com/tylern91/rqmd/commit/45c6b0f)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#3](https://github.com/tylern91/rqmd/pull/3)
- `embed`: guard embed/rerank token overflow with truncation to context window
  (`EMBED_CONTEXT_SIZE - 4` tokens); prevents `GGML_ASSERT n_ubatch >= n_tokens` abort. ([`45c6b0f`](https://github.com/tylern91/rqmd/commit/45c6b0f)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#3](https://github.com/tylern91/rqmd/pull/3)
- `fts`: normalize Tantivy BM25 score to `[0,1)` using `s/(1+s)` squash (mirrors
  qmd) so `rqmd search` never displays scores above 100%. ([`45c6b0f`](https://github.com/tylern91/rqmd/commit/45c6b0f)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#3](https://github.com/tylern91/rqmd/pull/3)
- `llm`: suppress llama.cpp INFO/WARN noise; send logs to tracing subscriber instead
  of stderr; add `add_sequence(false)` for Mean-pooling encoders. ([`45c6b0f`](https://github.com/tylern91/rqmd/commit/45c6b0f)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#3](https://github.com/tylern91/rqmd/pull/3)
- `embed`: make embed resumable across interrupts; fix `update` UNIQUE constraint;
  fix char-boundary panic on multi-byte UTF-8 (em dash, CJK) in chunker. ([`45c6b0f`](https://github.com/tylern91/rqmd/commit/45c6b0f)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#3](https://github.com/tylern91/rqmd/pull/3)
- `status`: rewrite `rqmd status` to match qmd's layout — single `Size:` line,
  per-collection multi-line blocks, `Updated`/`AST Chunking`/`Examples`/`Models`/`Tips`
  sections; correct `rqmd` branding throughout. ([`45c6b0f`](https://github.com/tylern91/rqmd/commit/45c6b0f)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#3](https://github.com/tylern91/rqmd/pull/3)

---

## [0.1.1] - 2026-06-29

### Fixed

- `collection add`: stop loading the inference backend (embed + rerank GGUF
  models) during BM25 indexing. Switched to `open_store_no_backend` +
  `index_document_fts_only` so model loading is deferred to `rqmd embed`. ([`c9e43d8`](https://github.com/tylern91/rqmd/commit/c9e43d8)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#2](https://github.com/tylern91/rqmd/pull/2)
- `rqmd embed`: clear stale `content_vectors` rows before re-embedding a
  collection. Prevents UNIQUE constraint violation on `vid` when a prior
  interrupted embed left the DB ahead of the HNSW index. ([`c9e43d8`](https://github.com/tylern91/rqmd/commit/c9e43d8)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#2](https://github.com/tylern91/rqmd/pull/2)
- CLI result display: fix hardcoded `rrrqmd://` URI scheme typo in
  `print_cli`; path labels now use the canonical `rqmd://` URI from
  `SearchResult.file`. ([`c9e43d8`](https://github.com/tylern91/rqmd/commit/c9e43d8)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#2](https://github.com/tylern91/rqmd/pull/2)

## [0.1.0] - Initial release

rqmd is a Rust port of [tobi/qmd](https://github.com/tobi/qmd), the original
TypeScript hybrid-search CLI. This is the first public release of the Rust
implementation.

### Added

- **rqmd-core** — core library crate: SQLite schema (rusqlite), Tantivy BM25
  full-text index, usearch HNSW vector index, Reciprocal Rank Fusion (RRF),
  sliding-window chunker, and the hybrid BM25+vector+RRF+cross-encoder pipeline. ([`c48550a`](https://github.com/tylern91/rqmd/commit/c48550a)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#1](https://github.com/tylern91/rqmd/pull/1)
- **rqmd-cli** — binary crate producing the `rqmd` command with subcommands:
  `query`, `search`, `vsearch`, `get`, `multi-get`, `ls`, `collection`, `context`,
  `init`, `status`, `embed`, `update`, `doctor`, `bench`, `eval`, `mcp`. ([`c48550a`](https://github.com/tylern91/rqmd/commit/c48550a)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#1](https://github.com/tylern91/rqmd/pull/1)
- **rqmd-llm** — inference backend abstraction. Default: `LlamaCppBackend` via
  `llama-cpp-2` (GGUF, Metal on macOS / CPU on Linux). Optional `ort-backend`
  feature: OrtBackend via ONNX Runtime (CoreML/CUDA/DirectML). ([`c48550a`](https://github.com/tylern91/rqmd/commit/c48550a)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#1](https://github.com/tylern91/rqmd/pull/1)
- **rqmd-mcp** — MCP server exposing `query`, `search`, `get`, `multi_get`, and
  `status` tools. Stdio and Streamable HTTP transports. ([`c48550a`](https://github.com/tylern91/rqmd/commit/c48550a)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#1](https://github.com/tylern91/rqmd/pull/1)
- **Workspace profiles**: `dev` (fast incremental), `release` (LTO thin), `dist`
  (LTO fat, symbols stripped, panic=abort) for release binaries. ([`c48550a`](https://github.com/tylern91/rqmd/commit/c48550a)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#1](https://github.com/tylern91/rqmd/pull/1)
- **CI**: `rust.yml` — macOS arm64 (default + ort-backend) + Linux x64; clippy
  `-D warnings`, fmt check, unit tests, BM25 quality eval. Dist binary artifact
  on push to `main`. ([`c48550a`](https://github.com/tylern91/rqmd/commit/c48550a)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#1](https://github.com/tylern91/rqmd/pull/1)
- **Nix flake**: reproducible dev shell with Rust stable + cmake/C++ for
  `llama-cpp-2` build dependencies. ([`c48550a`](https://github.com/tylern91/rqmd/commit/c48550a)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#1](https://github.com/tylern91/rqmd/pull/1)

### Notes

- Query expansion / HyDE (`generate_constrained`) is wired in the API but the
  generate model is not yet loaded — a deferred future phase. `query` uses
  BM25 + vector + RRF + rerank only. ([`c48550a`](https://github.com/tylern91/rqmd/commit/c48550a)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#1](https://github.com/tylern91/rqmd/pull/1)
- HF models are pinned by repository name (not digest). Model pinning by digest
  will be added in a future release. ([`c48550a`](https://github.com/tylern91/rqmd/commit/c48550a)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#1](https://github.com/tylern91/rqmd/pull/1)
- The SQLite schema is intentionally compatible with the original TypeScript `qmd`
  index format. Indexes created by `rqmd` use RFC-3339 UTC timestamps in
  `created_at`/`modified_at`/`embedded_at`. ([`c48550a`](https://github.com/tylern91/rqmd/commit/c48550a)) by [@tylern91-kat](https://github.com/tylern91-kat) in [#1](https://github.com/tylern91/rqmd/pull/1)

# Upstream `tobi/qmd` adoption assessment (2026-09-27)

rqmd is a Rust port of [tobi/qmd](https://github.com/tobi/qmd). Upstream's open issues and PRs
(roughly 60 issues / 30 fix PRs as of 2026-09-26) were compared against rqmd's implementation to
find (a) bugs qmd already found that rqmd inherited or independently has, and (b) rqmd-specific
security issues neither project has filed. This pass covered 54 upstream items plus a read-only
security audit of the four rqmd crates.

Everything in the **Adopted now** and **Security fixes shipped** tables below landed in PR #87,
#88, and #89. Everything in **Tracked as issues** is filed but not yet fixed — each links its
GitHub issue and its upstream counterpart where one exists. **Handled already** and **N/A** are
included for completeness so this table doesn't get re-litigated on the next comparison pass.

## Security fixes shipped (this session's audit, not from upstream)

These were found by reading rqmd's own code, not from the upstream comparison — qmd is a
different language/runtime (Node.js/TypeScript) and doesn't share rqmd's CLI hook-execution or
MCP-limit code paths.

| # | Severity | Evidence | Fix |
|---|----------|----------|-----|
| S1 | High | `rqmd-cli/src/store.rs:19-21` + `commands/index.rs:693-697` (pre-fix) — a project-local `.rqmd/index.sqlite` picked up implicitly runs its collection's `update_command` through `sh -c` with no user opt-in. Cloning a repo that ships a crafted `.rqmd/` and running `rqmd update` executes arbitrary code. | PR #87: `resolve_index_dir` now returns an `IndexSource` (`Explicit`/`ProjectLocal`/`Global`); hooks only run automatically for `Explicit`/`Global` sources. A project-local index skips hooks with a `WARN` unless `rqmd update --run-hooks` is passed. Hook subprocess now uses absolute `/bin/sh`. |
| S2 | Med-High | `rqmd-core/src/fts.rs:327` (pre-fix) clamps `limit` via `.clamp(limit, 5000)`, which **panics** when `limit > 5000` (clamp panics when min > max) while the store mutex is held — poisoning it until restart. The MCP `query`/`search` tools passed client-supplied `limit` through unclamped. | PR #87: `fts.rs`'s overscan clamp fixed to `saturating_mul(...).min(cap.max(limit))` (never panics); `rqmd-mcp/src/server.rs` adds `MAX_SEARCH_LIMIT = 1000` and `clamp_limit()`, applied to both `search` and `query`. |
| S3 | Med | `rqmd-cli/src/document.rs:104-113` (pre-fix) — the collection walker follows symlinks (`follow_links(true)`) with no check that the resolved path stays under the collection root. A symlink can index `~/.ssh/...`, which MCP `get` then serves. | PR #87: canonicalize the collection root once; filter out any walked entry whose canonicalized path doesn't start with it. Symlinks that stay inside the root still work. |
| S4 | Low | `rqmd-core/src/db.rs:306` — `get "#"` (empty docid after the `#`) becomes `LIKE '%'` and returns an arbitrary document. `get_document_by_filepath` (`db.rs:265`, used by MCP `get`/`similar`) didn't filter on `active`, so a deactivated (deleted-from-disk) document's stale content could still be served by path lookup. | PR #87: empty-docid guard added; new `get_active_document_by_filepath` wraps the filepath lookup with `.filter(|d| d.active)`, used at the `get`/`similar` call sites. |

Five more findings from the same audit pass were verified (with fresh `file:line` reads) but not
fixed in this pass — they're lower severity, need a design decision, or are pure hardening. Filed
as issues:

| Finding | Evidence | Issue |
|---|---|---|
| Lock steal race: `kill -0` EPERM (process alive, owned by another user) is read as "dead"; the liveness-check → reclaim sequence has a TOCTOU window letting two concurrent `acquire` calls both reclaim the same stale lock. | `rqmd-core/src/lock.rs:39-66`, `:91-100` | [#90](https://github.com/tylern91/rqmd/issues/90) |
| MCP `search`/`query`/`get`/`status` fully serialize per store — a single `std::sync::Mutex<Store>` for each of `fts_store`/`ml_store` means concurrent *read-only* calls queue instead of overlapping. | `rqmd-mcp/src/server.rs:28-34, 61-88, 215-243` | [#91](https://github.com/tylern91/rqmd/issues/91) |
| MCP `get` has no default cap on returned document size (`multi_get` caps document *count* at 200 but not per-document size). | `rqmd-mcp/src/server.rs:362` (`.take(max_lines.unwrap_or(usize::MAX))`) | [#92](https://github.com/tylern91/rqmd/issues/92) |
| MCP `status` includes the server's absolute filesystem path (embeds OS username on typical home-directory installs) in its response text. | `rqmd-mcp/src/server.rs:441-444` | [#93](https://github.com/tylern91/rqmd/issues/93) |
| `--host ::1` (bracketless IPv6 literal) fails to bind — already flagged as a known limitation in the code's own doc comment, but undocumented in user-facing docs. | `rqmd-mcp/src/lib.rs:91-95, 217` | [#94](https://github.com/tylern91/rqmd/issues/94) |

## Adopted now (upstream correctness bugs, verified and fixed)

| Upstream | Description | Evidence | Fix |
|---|---|---|---|
| qmd #987 | vsearch/similar-to-hash loaded the full document row (`SELECT ... raw ...`) for every raw HNSW hit inside a widening-`k` loop, before the final `limit` truncation — memory use scaled with the widening scan, not the requested result count. | `rqmd-core/src/store.rs` (`search_vec_multi`, `similar_to_hash`, pre-fix) | PR #88: rank by `doc_for_vid_meta`/`doc_for_vid_meta_in` (no body fetch), load the body only for the final truncated set. `doc_for_vid` (the old body-fetching helper) is now dead code and was deleted. |
| qmd #933 (analog) | Vid→document resolution used `LIMIT 1` with no collection filter. If identical content exists in collections A and B (same content hash), a vsearch scoped to B could resolve to A's row and get filtered out, silently dropping a true positive. | `rqmd-core/src/db.rs` (vid resolution, pre-fix) | PR #88: new `doc_for_vid_meta_in(conn, vid, collections)` adds `AND d.collection IN (...)` when scoped; used by `search_vec_multi`, `similar_to_hash`'s scoped path, and `vec_hits_to_ranked`. Test: same hash in two collections, scoped vsearch returns the correct collection's copy. |
| qmd #952 / #1000 | Query expansion (`lex:`/`vec:`/`hyde:` sub-queries from the local LLM) wasn't deduplicated — repeated lines, or a line identical to the original query, each ran their own widening retrieval pass. | `rqmd-core/src/store.rs` (`parse_and_run_expansion`, pre-fix) | PR #88: dedup by `(kind, trimmed text)` and drop lines equal to the original query, before running any retrieval. Test with a canned expansion string and a fixed-output backend. |
| qmd #966 | No NFC/NFD Unicode normalization at index or query time — an NFD-encoded query (e.g. combining-character form) could miss an NFC-indexed document, because Tantivy's default tokenizer treats combining marks as separators. **Verified with a failing probe test first** (per the plan's "drop this PR if the probe passes on main" instruction) — the probe failed, confirming the bug. | `rqmd-core/src/fts.rs` (no normalization, pre-fix) | PR #89: new `normalize_nfc()` (via `unicode-normalization`), applied to `title`/`body` at index time and `query_text` at query time. **Limitation** (documented in the fix's doc comment and CHANGELOG): already-indexed unchanged content is not retroactively re-normalized — `index_document_fts_only_with_raw` returns early on `IndexOutcome::Unchanged`, skipping the Tantivy `add_document` call, so an NFD-form document indexed before this fix stays NFD until its content actually changes. There's no dedicated "reindex FTS only" command to force it. |

## Tracked as issues (adoption candidates, not yet fixed)

| Upstream | Description | Size | Issue |
|---|---|---|---|
| qmd #976 | CJK text may tokenize poorly: default tokenizer + `RemoveLongFilter(40)` isn't CJK-aware. **Inference only** — not verified with a failing test in this pass. | M | [#95](https://github.com/tylern91/rqmd/issues/95) |
| qmd #955 | Table-aware chunking — long markdown tables have no dedicated break-point boundary type, so the chunker's windowed search can cut mid-row. | M | [#96](https://github.com/tylern91/rqmd/issues/96) |
| qmd #936 / #983 | Filtered HNSW search — scoped vsearch resolves vids per-hit post-scan (correctness already fixed, PR #88) but still scans the full unfiltered HNSW graph before scoping; a filtered-search API (if usearch supports it) would skip that. | L | [#97](https://github.com/tylern91/rqmd/issues/97) |
| qmd #809 / #913 | No `--timeout`/cancellation for long-running search, query, or embed operations, CLI or MCP. | M | [#98](https://github.com/tylern91/rqmd/issues/98) |
| qmd #957 | No automatic GPU→CPU fallback when Metal/CUDA backend init fails — only a manual `RQMD_FORCE_CPU` override exists. | M | [#99](https://github.com/tylern91/rqmd/issues/99) |
| qmd #937 | usearch HNSW index has no incremental compaction; the only way to reclaim space from deleted/updated vectors is `--rebuild`, which re-embeds the entire corpus. | L | [#100](https://github.com/tylern91/rqmd/issues/100) |
| qmd #944 | No lock eviction path for a live-but-stuck holder — only automatic dead-PID reclamation (see S-list above for its own correctness gaps) or a manual `rm -rf` of the lock directory. | M | [#101](https://github.com/tylern91/rqmd/issues/101) |
| qmd #979 | Update hooks have no subprocess timeout; multi-collection keep-going behavior on a hook failure needs verification/hardening. (Absolute `/bin/sh` path already shipped in PR #87.) | M | [#102](https://github.com/tylern91/rqmd/issues/102) |
| qmd #996 (gap) | Per-document embed error handling during `rqmd embed` — whether one document's failure aborts the whole run or is skipped-and-logged needs verification; the main thrust of #996 is already handled (see below). | S | [#103](https://github.com/tylern91/rqmd/issues/103) |
| qmd #920 / #774 / #909 | Query expansion prompt has no explicit language-match instruction — a non-English query's LLM-generated sub-queries could drift to another language, hurting BM25 recall in particular. | M | [#104](https://github.com/tylern91/rqmd/issues/104) |
| qmd #954 | No OpenAI-compatible remote inference backend — local-only GGUF models today. Deliberately in tension with rqmd's local-first design principle; needs an explicit adopt/reject decision, not just an implementation. | L | [#105](https://github.com/tylern91/rqmd/issues/105) |
| qmd #942 | No cross-process model sharing — a CLI invocation loads its own model copy even when a live `rqmd mcp` daemon already has one loaded for the same index. | L | [#106](https://github.com/tylern91/rqmd/issues/106) |

## Handled already

These upstream items map to behavior rqmd already has, verified by reading the current code
(not just recalled from the original port):

- **Stale content served after file deletion (qmd #961 candidate).** Verified this pass:
  `rqmd update`'s per-collection walk already deactivates documents whose file is gone
  (`crates/rqmd-cli/src/commands/index.rs:801`, `db::deactivate_missing_documents`), and docid-path
  lookup already filters `active=1` (`crates/rqmd-core/src/db.rs:311`). The one real gap —
  filepath-based lookup (`get_document_by_filepath`, `db.rs:265`) not filtering `active` — was
  exactly S4 above and is now fixed in PR #87. **This item was in the original "track as issues"
  list as an inference; verification this pass showed it's already handled, so no issue was
  filed for it.**
- #996, #991, #989 (mostly), #975, #941/#988, #915, #897, #971, #959, #947, #931, #914, #922 (one
  edge case remains unaddressed — not independently re-verified this pass), #946.

## N/A (qmd-specific, no rqmd equivalent)

qmd's Node.js/TypeScript runtime, SQLite FTS5 usage, and REST-first design mean some upstream
issues have no rqmd analog: #985, #926, #994, #997, #998, #970, #965.

## Dependency audit

`cargo audit` (re-run 2026-09-27 on `main`, same result before and after this session's PRs):
**0 vulnerabilities**, 2 pre-existing warnings, both blocked on upstream releases rqmd doesn't
control:

- `lru@0.16.4` — [RUSTSEC-2026-0253](https://rustsec.org/advisories/RUSTSEC-2026-0253) (unsound
  panic-safety issue in `LruCache::pop()`), pulled in transitively by `tantivy 0.26.1`. rqmd never
  calls `LruCache::pop` directly — blocked on a tantivy release that updates its `lru` dependency.
- `paste@1.0.15` — [RUSTSEC-2024-0436](https://rustsec.org/advisories/RUSTSEC-2024-0436)
  (unmaintained, no known vulnerability, informational only).

**Decision: Dependabot stays disabled** on this repo (the GitHub API returns 403 — Dependabot
alerts are disabled at the repo/org level). `cargo audit` in `.github/workflows/security.yml`
remains the dependency-vulnerability gate; no `.github/dependabot.yml` was added. Revisit if
Dependabot becomes available and `cargo audit`'s CI-only cadence turns out to be insufficient.

## Verification

Every fix above shipped with its own regression test, and every correctness fix (PR #88's two
bugs, PR #89's NFC fix) was verified by mutation testing — temporarily reverting the fix, confirming
the associated test fails with the predicted symptom, then restoring the fix and confirming the
test passes again. All three PRs passed the full local gate before merge: `cargo fmt --all
--check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`,
`cargo run --bin rqmd -- eval --mode bm25` (100% on all tiers, no regression), `cargo check
--no-default-features --features metal`, and `cargo audit`.

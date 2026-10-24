# Issue triage and dependency review (2026-10-24)

[← Research notes](README.md)

A survey of open rqmd issues, externally reported search and indexing problems, and the dependency and CI state of the repository. Every externally reported problem was checked against rqmd's own code before it was scored; the table below lists the idea in rqmd's terms. Labels: **Fact** is a line read or a command run in the session, **Inference** is reasoning from facts, **Open question** needs a decision.

## Outcome

| Outcome | Items |
|---|---|
| Implement (small, bounded) | 32 |
| No action | 29 |
| Design issue first | 13 |
| Reject | 10 |
| Close an existing issue | 1 |
| **Total** | **85** |

- Fact: no change forces a full index rebuild or a full re-embed. Index work is limited to additive columns, one partial full-text re-index (a renamed collection) and one partial re-embed (files of newly supported languages).
- Fact: the chunk-vector reuse added in 0.28.0 is defeated by the normal flow: `rqmd update` sweeps orphaned vectors (`crates/rqmd-cli/src/commands/index.rs`, `reclaim_orphaned_vectors`) before `rqmd embed` can reuse them. A CLI-level test proves it before the fix.
- Inference: the macOS CI leg spent most of its time on one model-download test; see "CI" below.

## Triaged items

Status is the item's state in rqmd today. Impact is H, M or L; effort is S, M or L.

### Implement (small, bounded) (32)

| Idea | In rqmd | Impact | Effort | Related rqmd issue |
|---|---|---|---|---|
| Chunk-vector reuse for edited documents is defeated by the orphan sweep that `rqmd update` runs before `rqmd embed` | partly in rqmd | H | M | rqmd#131 (closed by PR #142) |
| Graceful Ctrl-C/SIGTERM for `rqmd embed`: stop admitting documents, checkpoint completed work, exit with a clear message | partly in rqmd | M | S | rqmd#98 (closed: --timeout for query and embed) |
| Make `rqmd embed`/`update` status and progress output honor NO_COLOR and non-TTY stdout | new | L | S |  |
| Report a failed rerank instead of silently returning fusion order as if it had been reranked | partly in rqmd | M | S | rqmd#125 |
| Let the query deadline interrupt reranking between candidates and return fusion order instead of overrunning | partly in rqmd | M | M | rqmd#125 |
| Return fused (unreranked) results instead of an error when the query deadline expires before or during rerank | new | H | M | rqmd#125 |
| Persist query-expansion output and rerank scores across runs in a bounded, content-keyed cache | new | H | L | rqmd#121 |
| Declare readOnlyHint (plus destructive/idempotent/open-world hints) on all five MCP tools | new | H | S | #146 |
| Opt-in idle shutdown for the HTTP MCP daemon | new | M | S | #122 |
| Close open SSE streams on daemon shutdown so `rqmd mcp stop` finishes within its 5 s wait | new | M | S |  |
| Report the count of documents awaiting embeddings in the MCP status tool | partly in rqmd | L | S |  |
| Let users keep the index and the model cache in one chosen directory, and document how | partly in rqmd | L | S |  |
| Make the optional ONNX backend honor HF_HOME, HF_ENDPOINT, HF_TOKEN and HF_HUB_OFFLINE, and pin and verify its model | new | M | M |  |
| Upgrade the bundled llama.cpp binding to the latest patch release | new | L | S |  |
| Upgrade the MCP server SDK to patch a remote memory-exhaustion DoS in the HTTP transport | new | H | S | rqmd#81 (closed: rmcp 3.x migration), open PR #146 |
| Make the built-in directory exclusions (build, dist, vendor, node_modules, target, .cache) visible in add/update summaries | partly in rqmd | M | S | rqmd#132 (closed; set the 'never silently lose documents' principle) |
| Skip descending into excluded directories while walking a collection instead of filtering every file underneath | new | M | S | rqmd#118/#119 (stat skip) |
| Show a collection's ignore patterns, and reject malformed ignore globs instead of silently dropping them | partly in rqmd | L | S |  |
| `collection rename` must move the full-text entries and path contexts along with the SQLite rows | partly in rqmd | M | M |  |
| Forward SIGTERM/SIGINT/SIGHUP from `rqmd update` to a running update-hook process group | new | M | S |  |
| Per-stage progress on stderr for `rqmd query`/`vsearch` (model load, embed, expansion, sub-queries, rerank i/N) | partly in rqmd | M | M | rqmd#128, rqmd#125 |
| Honour NO_COLOR and non-TTY stdout in `rqmd update`/`embed` status lines | new | L | S |  |
| `rqmd watch`: keep the full-text index fresh as collection files change | new | H | M | rqmd#120, rqmd#122, rqmd#86 |
| Keep `rqmd doctor` / `rqmd status` cost from scaling with total body bytes: octet_length() for the empty-body test and COUNT(*) for per-collection counts | partly in rqmd | M | S |  |
| Bound result-body memory in hybrid query: move bodies instead of cloning, and drop non-survivor bodies before building results (no output truncation) | partly in rqmd | L | S |  |
| Exact collection scoping for keyword search: filter on the doc_id fast field during collection, replacing the tokenized pushdown and 20x/5000 overscan | partly in rqmd | M | M |  |
| Deterministic tie-breaking by filepath in RRF fusion and per-document vector ranking | new | M | S |  |
| Opt-in multiple results (distinct sections) per file for query/search/vsearch | new | M | M | rqmd#123 |
| Opt-in recency weighting by document age with a floor | new | M | M | rqmd#124 |
| Reuse stored vectors across chunking-fingerprint changes when the chunk text is identical | new | H | M | rqmd#126, rqmd#127 (derived prerequisite, no issue yet) |
| Declaration-boundary chunking for Go, Rust, C#, Kotlin and Swift source files | partly in rqmd | L | M | rqmd#127 |
| Stale-embedding warning advises a full rebuild when an incremental embed suffices; raw ANSI ignores NO_COLOR | new | M | S | none (derived finding) |

### Design issue first (13)

| Idea | In rqmd | Impact | Effort | Related rqmd issue |
|---|---|---|---|---|
| Per-collection embed-exclusion globs: keep matching documents in keyword search but out of the vector index | new | M | M | rqmd#132 (closed: .gitignore and max_file_bytes, which exclude from BM25 too) |
| Per-collection size threshold above which a document is kept in keyword search but not embedded | new | M | S | rqmd#132 |
| Let an MCP client ask for an incremental index refresh before searching | new | M | L | #122 |
| Do not walk collection roots outside the project when the index was picked up implicitly from a project-local .rqmd/ | partly in rqmd | M | M | rqmd#86 (closed, hook gate precedent) |
| Allow a collection to opt out of the built-in directory exclusions | new | M | M | rqmd#132 |
| Re-running `collection add` for an existing name silently clears its update hook and include setting | new | M | S |  |
| Retrieval prefers an exact `collection/path` match over a suffix match from another collection | partly in rqmd | L | S |  |
| Multi-document retrieval must not exceed SQLite's bound-parameter limit | partly in rqmd | L | S |  |
| `rqmd mcp stop` / SIGTERM must reliably terminate the HTTP daemon even with an open session or an in-flight query | partly in rqmd | M | M | rqmd#122 |
| Cut per-hit SQLite lookups in scoped vector search: memoize vid-to-document resolution across k-doubling passes and cache the prepared statement | partly in rqmd | M | S |  |
| Graded-relevance evaluation: `rqmd eval` reads user-supplied judged queries (qrels) and reports nDCG@10, MRR and Recall@10 against an existing index, read-only | partly in rqmd | M | M |  |
| User-tunable chunk size and overlap | new | L | L | rqmd#126 |
| Merge a near-duplicate tail chunk into its predecessor | partly in rqmd | L | M | rqmd#126 |

### No action (29)

| Idea | In rqmd | Impact | Effort | Related rqmd issue |
|---|---|---|---|---|
| Guarantee embed inputs are valid text after length truncation (no split multi-unit characters reaching a strict JSON embedding endpoint) | not applicable | L | S | rqmd#105 (closed not planned: remote backend) |
| Ensure each embedding input is processed in a single physical batch so long chunks never get corrupted vectors | already in rqmd | L | S | rqmd#130 (closed: reuse one embedding context) |
| Resume a partially embedded large document from per-chunk checkpoints instead of restarting it | not applicable | L | L | rqmd#98, rqmd#103 (closed: skip-and-continue on a document's embed error) |
| A rerank result produced by a fallback path must never be persisted as if it were real model output | not applicable | L | S | rqmd#121 |
| Make sure the rerank/fusion score blend is monotonic in fusion rank | not applicable | L | S | rqmd#129 |
| Bound the size of each rerank input | already in rqmd | L | S |  |
| Index refresh must not wipe persisted query-expansion and rerank results | not applicable | L | S | rqmd#121 |
| Research: optional token-level late-interaction reranking stage | new | L | L | rqmd#129 |
| MCP HTTP transport must stream SSE responses (listen/subscription streams) rather than buffer them | already in rqmd | L | S |  |
| Return document retrieval results as plain text content rather than embedded resources | already in rqmd | L | S |  |
| Validate malformed search payloads on an HTTP REST query endpoint (400 instead of 500) | not applicable | L | S |  |
| Server and benchmark commands must not load models named by an untrusted project-local config | not applicable | L | S | rqmd#86 (hook trust, closed), rqmd#105 (remote backend) |
| Code-sign release binaries so OS application-control policies do not block them | not applicable | L | L |  |
| Collection ignore patterns must survive reopening the index | already in rqmd | L | S |  |
| Read-only commands must never drop collection rows because an external config source is absent | not applicable | L | S |  |
| Removing a collection also removes its indexed documents, vectors and full-text entries | already in rqmd | L | S |  |
| Indexing two files with identical content must not hit a content primary-key conflict | already in rqmd | L | S |  |
| A timed-out or killed search process must not linger as a CPU-burning orphan | not applicable | L | S | rqmd#98 (closed, added --timeout) |
| Vector/rerank stage shows no progress, so a slow query looks hung | duplicate of an rqmd issue | M | M | rqmd#128 |
| Vector search cost grows linearly with chunks times expansion sub-queries | already in rqmd | L | S |  |
| Wrapper launcher must forward termination signals to the real CLI process | not applicable | L | S |  |
| Bound a hung native rerank and fall back to fused results (stacked inside the signal-forwarding PR) | duplicate of an rqmd issue | L | M | rqmd#125 |
| Cache MCP server instructions instead of rebuilding them (with an index-status aggregation) on every HTTP request | not applicable | L | S |  |
| Run the CLI in the launcher's own process instead of spawning a second runtime | not applicable | L | S |  |
| Skip unchanged files on re-index by stat, with racily-clean, size-limit and rename safety | already in rqmd | L | S |  |
| Drop duplicate query expansions before running searches | already in rqmd | L | S |  |
| One ranked list per search across collections (no per-collection fusion) | already in rqmd | L | S |  |
| Vector-search document lookup joins the full body once per matching chunk | already in rqmd | L | S |  |
| Request to publish a new package release | not applicable | L | S |  |

### Reject (10)

| Idea | In rqmd | Impact | Effort | Related rqmd issue |
|---|---|---|---|---|
| Content-defined chunk boundaries so an insertion near the top of a document does not shift every later chunk | partly in rqmd | L | L | rqmd#126 (open: configurable chunk size) |
| Recovering from a native inference thread that never returns | not applicable | L | L |  |
| Operator-enforced rerank policy for the MCP query tool (separate bounded reranking tool) | partly in rqmd | L | M |  |
| Opt-in source-root-relative file paths in MCP search results | new | L | M | #93 |
| Add document and pending-embedding counts to the unauthenticated HTTP /health endpoint | partly in rqmd | L | S | #93 |
| Expose the MCP tool router so another rmcp server can embed rqmd's tools | partly in rqmd | L | S |  |
| Add `venv` to the default excluded directories | new | L | S | rqmd#132 |
| Opt-in in-process serialized maintenance and bounded embedding sweeps for library (SDK) hosts | not applicable | L | L | rqmd#122, rqmd#120 |
| Partition the vector index by collection so scoped vector search scans only its collections | not applicable | L | L |  |
| Repack the vector table to reclaim dead rows | not applicable | L | M |  |

### Close an existing issue (1)

| Idea | In rqmd | Impact | Effort | Related rqmd issue |
|---|---|---|---|---|
| A failing per-collection update hook does not stop the other collections from being indexed | already in rqmd | L | S | rqmd#102 (closed) |

## Delivered in this round

| Change | Pull request |
|---|---|
| Rust CI speed-up: model test once, cache saved only on `main`, parallel smoke build | #150 |
| One required status, `ci-complete`, with a changes job instead of a path filter | #164 |
| Security workflow: issue creation fix, timeouts, pinned Trivy | #158 |
| Nix workflow runs on `main` and on flake changes | #159 |
| Dependabot updates and a 30-day lock soak gate | #160 |
| rmcp 3.6.0, patched webpki crates, trimmed model-download TLS stack | #161 |
| `readOnlyHint` on the read-only MCP tools (#146) | #162 |
| Forward SIGINT, SIGTERM and SIGHUP to the update hook | #163 |
| Minimum supported Rust version 1.99 | #166 |
| HTTP/2 requests accepted by the MCP host check | #167 |
| Model cache location documented | #169 |

Design issues filed for larger items include the multi-get limit of about 1,000 entries (#154), read commands creating an empty index (#156) and a project-local index following external collection paths (#148).

## Dependency review

`cargo audit` on 2026-10-24. Fact unless marked.

| Item | Severity | Reachable in rqmd | Decision |
|---|---|---|---|
| rustls-webpki, webpki-roots, webpki-root-certs (RUSTSEC-2026-0381, -0382, -0383) | vulnerability, no CVSS | No: model downloads use `hf_hub::api::tokio` with the platform TLS stack; `ureq` was compiled only through hf-hub's default features | Trim hf-hub features; pin the two patched crates under an explicit soak waiver |
| rmcp 3.4.1 (GHSA-4432-vg92-fqh6) | high, denial of service on `mcp --http`, invisible to `cargo audit` | Yes | Update to 3.6.0 under an explicit soak waiver |
| tokio 1.53.1 (RUSTSEC-2026-0386) | unsound warning | No (`AsyncFd` unused) | Wait for the soak (1.53.3 on 2026-11-12); 1.54.0 is yanked |
| lru 0.16.4 (RUSTSEC-2026-0253) | unsound warning | No (`usize` keys) | Moves with tantivy 0.27 after 2026-11-16 |
| paste 1.0.15 (RUSTSEC-2024-0436) | unmaintained, build time only | No | None |
| Trivy v0.70.0 in `security.yml` | high (poisoned vulnerability database) | CI only | Pin v0.74.0 |
| SQLite 3.53.2 (rusqlite 0.40.2) | FTS5 fixes included | n/a | None |

Deferred without a security driver: llama-cpp-2, rusqlite 0.41, usearch 2.26.5, tree-sitter 0.27, dirs 7 (could change the data directory and hide existing indexes), reqwest 0.13, tower-http 0.7, hf-hub 1.0, and ort (stays pinned to `=2.0.0-rc.12`).

### llama-cpp-2: why it stays at 0.1.154

- Fact: 0.1.154 already bundles llama.cpp b10200. Every llama.cpp advisory has a vulnerable range at or below b8145, and the RPC remote-code-execution advisory is at or below b7991.
- Fact: rqmd enables only the `sampler` feature (`crates/rqmd-llm/Cargo.toml`), so the RPC backend is not compiled in.
- Inference: the fixes in 0.1.155 to 0.1.160 near rqmd's code (split UTF-8 token decoding, `embeddings_out_len`, a multimodal use-after-free) do not affect it, because rqmd decodes with a stateful decoder, reads `embeddings_seq_ith` and does not use `mtmd`.
- Fact: the embed fingerprint does not include the engine version, so a newer snapshot could shift vectors with no stale warning.
- Decision: skip. Any future bump needs a gate of cosine similarity at or above 0.999 on a fixed embedding sample; otherwise only `rqmd embed --rebuild` is correct.

## CI

- Fact: one acceptance test downloads and runs the real models. It took 168 s on Linux and 2,568 s on macOS and set the wall clock of every pull request (median 2,592 s over 35 runs).
- Fact: the Linux legs do not compile the Metal and CoreML code or the non-unix branches, so macOS compilation, linting and the non-model tests stay on every pull request; only the model test moved to the weekly macOS run.
- Open question: whether the macOS model test is fast enough with `RQMD_FORCE_CPU=1` to return to every pull request; a measurement is pending.
- Open question: Dependabot's `ignore` list for crates that can change stored data may also suppress security-update pull requests for them; `cargo audit` still scans the whole lockfile and those advisories are handled by hand.

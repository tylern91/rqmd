# Decisions on four adoption candidates (2026-10-04)

[← Research notes](README.md)

Four of the issues filed by the [2026-09-27 upstream adoption pass](2026-09-27-upstream-qmd-adoption.md)
were investigated and **not implemented**. Each was closed as `not_planned` with the evidence below,
so the next comparison against upstream `tobi/qmd` doesn't re-open it without new facts.

| Issue | Proposal | Decision |
|-------|----------|----------|
| #97 | Filter the HNSW traversal to the scoped collection | Not adopted — no gain where it matters, recall loss where it helps |
| #100 | Compact the HNSW graph in place | Not worth building — tombstones are recycled |
| #105 | OpenAI-compatible remote inference backend | Declined — conflicts with the local-first principle |
| #106 | Route CLI inference through a running MCP daemon | Declined — needs a new daemon protocol for a small gain |

## #97 — filtered HNSW search for scoped `vsearch`

**Question.** `Store::search_vec_scoped` over-fetches from the whole graph (`k`, doubling until
`fetch_size` in-scope results exist) and drops out-of-scope hits. usearch exposes
`Index::filtered_search`, which admits only chosen keys during traversal. Is it worth using?

**Method.** A throwaway `#[ignore]` test in `hnsw.rs` (release build, not committed) built 50,000
768-dimensional cosine vectors across 10 synthetic clusters (the "collections": one at 1%, one at
10%, eight sharing the rest), then ran 40 scoped queries per case for the top 20, comparing the
doubling loop against `filtered_search` with a `HashSet` predicate.

| Scope | Query is | Doubling loop p50 / p95 | Filtered p50 / p95 | Vectors the doubling loop fetched | Filtered result overlap with the doubling loop's |
|-------|----------|-------------------------|--------------------|-----------------------------------|------------------------------------------------|
| 1% (500) | inside the scope | 0.41 / 0.44 ms | 0.40 / 0.42 ms | 20 | 800/800 |
| 1% (500) | far from the scope | 341 / 457 ms | 63 / 79 ms | 48,395 | 699/800 |
| 10% (5,000) | inside the scope | 0.76 / 1.43 ms | 0.65 / 0.81 ms | 20 | 800/800 |
| 10% (5,000) | far from the scope | 59 / 171 ms | 18 / 34 ms | 24,576 | 347/800 |

**Reading.**
- When the query is relevant to the scope — the normal reason to scope a search — the first
  `k = fetch_size` probe already finds enough in-scope hits. The two paths are the same speed and
  return the same results.
- Filtering only wins when the scope holds nothing near the query, where results are weak anyway.
  There the doubling loop degenerates to a near-exhaustive scan and so returns the exact in-scope
  top-20, while filtered traversal is approximate and agreed on only 347–699 of 800 results.
  Trading recall for speed in that case is not a clear improvement, and the issue's acceptance
  criterion is that scoped correctness is unaffected.
- The "vectors fetched" column also bounds the baseline's per-hit `doc_for_vid_meta_in` SQLite
  lookups, which the HNSW-only timing above does not include. The far-from-scope baseline is
  therefore slower than shown. This was not measured.

**Limits of the evidence.** Synthetic clustered vectors, not real embeddings; HNSW layer only;
one machine. A real corpus where scoped queries are routinely far from their scope could change the
picture, and would justify re-running the measurement. The adoption sketch if so:
`VectorIndex::search_filtered(embedding, k, keep)` wrapping `Index::filtered_search`, an in-scope
vid set precomputed with one `documents`/`content_vectors` join, the doubling loop kept as the
fallback when filtered results under-fill, and `doc_for_vid_meta_in` kept.

## #100 — compacting the HNSW graph

**Question.** Can usearch rebuild the graph from existing vectors without re-embedding?

- **`Index::compact` is not that.** It is bound at `usearch-2.26.1/rust/lib.rs:1742`. In
  `index_dense.hpp:1907-1926` it allocates a replacement vector table of the *same* size
  (`new_vectors_lookup(vectors_lookup_.size())`), copies vectors to their new slots, and does not
  touch `slot_lookup_` (declared at `:519`). So it does not shrink the structure and does not
  repair the key lookup.
- **Tombstones are recycled, so bloat is bounded.** `add` pops a removed slot from `free_keys_` and
  reuses its node before allocating a new one (`index_dense.hpp:2188-2196`). Deleting and re-adding
  documents does not grow the graph without limit.
- **The measured cost is small.** The live index held 27 orphaned vectors on 2026-09-16.
  `rqmd embed --cleanup` already reclaims the SQLite side (orphaned `content_vectors` rows and
  unreferenced `content` rows); see [TROUBLESHOOTING.md](../TROUBLESHOOTING.md#rqmd-doctor-reports-orphaned-vectors).
- **A rebuild without re-embedding is possible.** Vectors are stored as `F32`
  (`hnsw.rs` `make_opts`), `VectorIndex::get_vector` reads them back, and `add_with_vid` re-inserts
  them under their original vids into a fresh index. It is not built, because nothing measured
  needs it. If a real index ever shows meaningful dead space, that is the path.

## #105 — remote OpenAI-compatible backend

`CONTRIBUTING.md:32` lists **Local-first** among the principles that a change must not cut against:
no telemetry, and the only network access is the first-run model download. A backend that sends
document text and queries to a remote endpoint is exactly that cut. The issue itself names the
tension. If demand appears, the shape that respects the principle is a separate opt-in crate,
off by default, not a `BackendKind` in `rqmd-llm`.

## #106 — sharing the daemon's models with CLI invocations

The daemon already amortizes model loads within its own process: the model store initializes
lazily and `release_idle_models` frees models idle past a TTL (`rqmd-mcp/src/server.rs`).
`daemon::fetch_health` (`rqmd-cli/src/daemon.rs:83`) only reads `/health/daemon`; it is not a
channel for running work. Proxying `query`/`embed` through the daemon needs a new "run this
operation in the daemon" protocol, plus a policy for the daemon's single serialized model store
(`query` is documented as serialized in [MCP.md](../MCP.md)) being shared with CLI callers. The
benefit is limited to running the CLI and the daemon against the same index at the same time,
which saves one redundant model load, not correctness. Revisit if concurrent use turns out to be
common.

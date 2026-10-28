# Where a scoped vector search spends its time (2026-10-28)

[← Research notes](README.md)

Answers [#152](https://github.com/tylern91/rqmd/issues/152): on a real index, does per-hit
resolution or ANN widening dominate a scoped `rqmd query`? The [2026-10-04 note](2026-10-04-issue-decisions.md#97--filtered-hnsw-search-for-scoped-vsearch)
timed only the HNSW call on synthetic vectors and said the per-hit lookups "were not measured".
This note measures them, with the hidden `rqmd query --timings` diagnostic added by the same change.

## Decision rule (written before the numbers)

Open an implementation issue only if both hold:

1. The per-hit lookups or the ANN widening passes are at least 20% of the end-to-end latency of a
   scoped query (the median of the runs below), in at least one of the scoped cases; and
2. the change keeps result equivalence: the same in-scope results, in the same order, for the same
   query and index.

Otherwise the note says no change is needed. Reranker and expansion latency are out of scope
(#121, #125, #128); they are reported only so the shares add up.

## What was measured

- **Machine.** Apple M4 (10 cores), 32 GiB, macOS (Darwin 27.0.0). Release build (`cargo build
  --release`, thin LTO) of origin/main 095b53c (v0.33.0) plus the `--timings` change. The change
  adds clock reads only when the flag is given; the 5-query comparison against the unmodified
  v0.33.0 binary was byte-identical in stdout, stderr and exit code without the flag.
- **Index.** An APFS clone (`cp -c`) of the author's live index, taken while the MCP daemon was
  running; every command below used `--index-dir` on the clone, which was deleted afterwards.
  `update` was never run. 33 collections, 57,295 active documents, 124,651 chunk vectors in
  `content_vectors` (54,021 distinct content hashes), `index.sqlite` 1.9 GB, `hnsw.usearch` 1.0 GB
  (768-dimension f32), the cached models were embeddinggemma-300M, Qwen3-Reranker-0.6B and Qwen3-1.7B (GGUF).
- **Query.** `terraform state locking`, `-n` default (10), so the internal candidate pool
  (`fetch_size`) is 20.
- **Three cases.**

| Case | Scope | Documents | Vectors |
|------|-------|-----------|---------|
| unscoped | no `-c`: the five collections that are included by default (including `vault`) | 14,580 | about 42,800 |
| large collection | `-c vault` | 9,726 | 29,260 |
| small collection, query far from it | `-c docker-images` (the query is about Terraform) | 15 | 21 |

"Unscoped" is not the whole index: `effective_collections` resolves it to the default-included
collections, so it is itself a scope of about a third of the vectors.

- **Commands.** Per case, one warm-up run (discarded) and then 7 runs (A) or 5 runs (B). The
  table reports medians. `--timings` writes to stderr, so stdout was discarded.

```
# A: retrieval only (no model generation, no rerank)
rqmd --index-dir CLONE query "terraform state locking" [-c vault | -c docker-images] --no-expand --no-rerank --timings
# B: the default pipeline (expansion and rerank on)
rqmd --index-dir CLONE query "terraform state locking" [-c vault | -c docker-images] --timings
```

The stage figures are the command's own `--timings` output. "Wall" is the process wall time
including opening the store and loading models; "total" is the time inside the query call.

## Results

Medians in ms. "Lookup" is the sum of the per-hit `doc_for_vid_meta_in` calls; "ANN" is the sum of
the `hnsw.search` calls.

### A: retrieval only (7 runs)

| Case | Wall | Total | Embed | ANN | Lookup | Passes (largest k) | Vectors fetched = lookups | ANN + lookup share of total |
|------|-----:|------:|------:|----:|-------:|-------------------:|--------------------------:|----------------------------:|
| unscoped | 694 | 319 | 188 | 45 | 81 | 9 (5,120) | 10,220 | 39% |
| `-c vault` | 40,906 | 40,584 | 175 | 199 | 40,204 | 11 (20,480) | 40,940 | 99.5% |
| `-c docker-images` | 9,607 | 9,249 | 177 | 4,358 | 4,668 | 15 (327,680) | 655,340 | 97.6% |

Run-to-run range (min to max): unscoped total 299 to 381; vault total 38,832 to 42,511;
docker-images total 9,151 to 9,456. Fusion, candidate resolution, section choice and result
assembly were each under 2 ms; the BM25 probe was 0.5 to 1.6 ms.

### B: default pipeline (5 runs)

| Case | Total | Embed | ANN | Lookup | Expansion generate | Rerank | Passes | Lookups | ANN + lookup share |
|------|------:|------:|----:|-------:|-------------------:|-------:|-------:|--------:|-------------------:|
| unscoped | 17,509 | 223 | 113 | 239 | 2,524 | 14,396 | 27 | 30,660 | 2% |
| `-c vault` | 76,596 | 220 | 320 | 55,034 | 2,541 | 18,481 | 31 | 81,860 | 72% |
| `-c docker-images` | 34,329 | 223 | 11,640 | 13,877 | 2,531 | 5,970 | 45 | 1,966,020 | 74% |

In B the 27 to 45 passes come from the query's own vector search plus the vector searches that the
model's `vec:` and `hyde:` expansion lines run, each widening on its own. The reranker scored 20,
20 and 15 sections (the `docker-images` scope has only 15 documents).

### Cost per lookup

| Case (A) | Lookup time / lookups |
|----------|----------------------:|
| unscoped (five collections) | 7.9 µs |
| `-c vault` | 982 µs |
| `-c docker-images` | 7.1 µs |

## Why the vault lookups are slow

A fact read off the query plan on the clone (`EXPLAIN QUERY PLAN` of the SQL in
`db::doc_chunk_for_vid_in`, which `doc_for_vid_meta_in` calls; sqlite3 CLI 3.54.1, so the plan in
rusqlite's bundled SQLite is inferred, but the timings above match it):

| Scope in the `IN (...)` list | Plan for the `documents` side of the join |
|------------------------------|-------------------------------------------|
| one collection (`'vault'`, `'docker-images'`) | `SEARCH d USING INDEX sqlite_autoindex_documents_1 (collection=?)`: finds the collection's documents, then compares `hash` |
| the five default collections | `SEARCH d USING INDEX idx_documents_hash (hash=?)` and a temp B-tree for `ORDER BY` |

With a one-collection scope each lookup therefore walks the collection's documents (9,726 for
`vault`), which is the 982 µs. For `docker-images` (15 documents) the same plan is cheap, and for
the default scope the planner uses the hash index.

A check of the cause, outside rqmd: the same statement run from Python's `sqlite3` module (SQLite
3.54.1) on the clone, read-only, 200 vids that belong to the scope, warm cache:

| Scope | Planner's choice | With `INDEXED BY idx_documents_hash` |
|-------|-----------------:|-------------------------------------:|
| `vault` | 516 µs per lookup | 5.4 µs |
| `docker-images` | 4.1 µs | 5.6 µs |

So the slow plan reproduces the per-lookup cost, at the same order of magnitude as the 982 µs
measured inside rqmd (the SQLite build and the mix of vids differ). Hinting the index was not tried in rqmd,
and this change does not alter the SQL. Whether `INDEXED BY` or a rewritten join is the right fix,
and that it returns the same row for a hash shared across collections, is for the follow-up.

## ANN widening and the size of the HNSW index

Facts:

- A scoped query doubles `k` from `fetch_size` (20) until 20 in-scope hits exist (or `k` reaches
  `hnsw.size()`). Far from the scope that takes many passes: 15 passes and `k = 327,680` for
  `docker-images`, where the 21 vectors of the scope were only reached in the last pass.
- In that run the last pass asked for 327,680 vectors and the index returned 327,680, so
  `hnsw.size()` is at least 327,680. `content_vectors` has 124,651 rows (all for active
  documents, vids 2,308,165 to 2,735,660). At least 203,029 index entries (62%) therefore resolve
  to no document. Each is still fetched and looked up on every pass that reaches it. (An earlier
  working figure of 163,840 for `hnsw.size()` came from a run that stopped for another reason and
  was wrong.)

Open:

- What those entries are. Tombstoned or orphaned vectors counted by usearch's `size()` is the
  likely cause (the index file is 1.0 GB, at most about 338,000 f32 vectors of 3 KB, fewer once the graph is counted), but I did not
  verify how `size()` counts removed entries, and `rqmd doctor` prints no orphan count on this
  index.
- Whether the clone, copied while the daemon was running, differs from the live pair of files.
  The numbers are for this copy.

## Conclusion, from the data

- **Per-hit resolution dominates** retrieval for a scope that is one large collection: 99% of the
  run for `-c vault` (40.2 s of 40.6 s), and 72% of the default pipeline (55.0 s of 76.6 s). The
  ANN traversal there is under 0.5%. This is a cost per lookup, 982 µs against 7 to 8 µs elsewhere,
  not only a count of lookups, and the plan in the table above explains it.
- **Widening dominates** for a scope far from the query: for `-c docker-images` the passes number 15
  (45 in the default pipeline) and fetch 655,340 vectors (1.97 million), so ANN plus lookups is
  97.6% of retrieval, split about half and half (4.4 s ANN, 4.7 s lookups), and 74% of the default
  pipeline. Cheaper lookups would not change the ANN half.
- **Neither dominates** the default scope: ANN plus lookups are 39% of retrieval and 2% of the
  default pipeline, where reranking (82%) and expansion (14%) do. Embedding the query is 59% of the
  unscoped retrieval.
- The decision rule's first condition holds (97.6% and 99.5% in A, 72% and 74% in B, against a 20%
  threshold), so an implementation issue is warranted. The second condition is a requirement on the
  change, and is in the draft below.

Limits: one query, one machine, one index, a clone taken from a live index. The unscoped and
`docker-images` lookup costs rest on warm page cache after the warm-up run. The query was chosen to
be far from `docker-images`; a scope close to the query stops after the first pass (the in-scope
count reaches 20 at once) and costs milliseconds, as the 2026-10-04 note found.

## What this change does not cover

- `rqmd vsearch` and `Store::search_vec_multi_with` are not instrumented: `--timings` is a
  `query` flag. That function has its own widening loop and uses `doc_chunk_for_vid_in`, not
  `doc_for_vid_meta_in`.
- The issue's line references (`store.rs:1390-1425`, `store.rs:780-800`) have moved since it was
  written. The query path today is `Store::search_vec_scoped` and `Store::vec_hits_to_ranked`
  in `crates/rqmd-core/src/store.rs`, and the lookup is `db::doc_chunk_for_vid_in` in
  `crates/rqmd-core/src/db.rs`.
- Measuring reranker or expansion latency (they are reported here only as shares).

## Draft follow-up issue (not filed)

**Title:** perf(query): one-collection scoped vector search walks the collection per hit

**Body:**

```
## Problem
`rqmd query -c <one collection>` resolves each ANN hit with `db::doc_chunk_for_vid_in`. For a
one-collection scope SQLite picks `sqlite_autoindex_documents_1 (collection=?)` for the join with
`documents`, so every lookup walks that collection's documents. Measured with `rqmd query --timings`
(docs/research/2026-10-28-scoped-vector-search-timings.md, #152), on a 57k-document index:

- `-c vault` (9,726 documents): 40,940 lookups took 40.2 s, 982 us each, 99.5% of a
  `--no-expand --no-rerank` query (72% of the default pipeline). With the default five-collection
  scope the same lookup is 7.9 us.
- A scope far from the query widens to k = 327,680 (15 passes, 655,340 lookups). Lookups and ANN
  traversal are each about half of that time.

## Proposed direction
1. Make the lookup use `idx_documents_hash` whatever the scope (for example `INDEXED BY`, or join
   on `cv.hash` first and filter `collection` afterwards). Measure on a clone of a real index.
2. Separately, consider batching the lookups of one pass into one query, and stopping a pass early
   once `fetch_size` in-scope hits exist. Only if (1) is not enough.
3. `hnsw.size()` was at least 327,680 against 124,651 `content_vectors` rows on that index. Find out
   what the extra entries are (tombstones?), and whether `rqmd embed --cleanup` or a rebuild removes
   them, since each is fetched on every widening pass.

## Acceptance criteria
- [ ] An equivalence test: for a fixture index with several collections that share content hashes,
      `search_vec_scoped` returns the same in-scope results, in the same order, before and after
      the change, for a one-collection scope, a multi-collection scope and no scope. The
      `ORDER BY d.collection, d.path LIMIT 1` tie-break for a hash shared across collections is
      preserved.
- [ ] `rqmd query --timings` on the same index shows the per-lookup time for `-c <large collection>`
      within 2x of the five-collection scope.
- [ ] No change to results, scores, the embed fingerprint, chunking or the index format.

## Out of scope
`filtered_search` (see 2026-10-04-issue-decisions.md), reranker and expansion latency.
```

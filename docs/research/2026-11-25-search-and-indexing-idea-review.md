# Search and indexing idea review: what was adopted, measured and dropped (2026-11-25)

[← Research notes](README.md)

A review of externally reported search and indexing ideas against the current code. Each idea was checked against the checkout first (does the gap still exist), then adopted, turned into a design issue, measured, or dropped. rqmd issue and PR numbers below come from the tracker, not from memory.

## Method

1. List the ideas, drop any whose premise no longer holds in the code, and mark each as already present, new, or not applicable.
2. For each new idea record impact (H/M/L), effort (S/M/L) and the rqmd evidence (`file:line`).
3. Small, bounded changes became issues with acceptance criteria and one PR each. Design-heavy ones became issues with a design and no code. Ideas that need numbers became one measurement issue (#296).
4. Every acceptance box was checked against a test that was run, and the PR merged only after the merge gate passed on the final head.

## Adopted (merged)

| Idea | Impact | Effort | rqmd evidence before | Issue | PR |
|---|---|---|---|---|---|
| Reject an unknown collection name before any refresh or model load, in the CLI and in the MCP `query`, `search` and `multi_get` tools | M | S | `Store::effective_collections` passed the requested list through unchecked | #285 | #297 |
| Look up code fences and break points by binary search while chunking | L | S | linear scans per match and per window in `chunking.rs` | #286 | #298 |
| Download missing models in the background when the MCP server starts | M | M | first `query` downloaded inside its deadline | #287 | #301 |
| Return the raw reranker score as `rerank_score` | L | S | blended and discarded in `assemble_results` | #288 | #299 |
| Warn when a new collection root nests with another, and show one file once when roots nest | M | M | no overlap check; fusion deduped by path only | #289 | #300 |
| `rqmd update --exclude <name>` | L | S | `update` took a single `-c` | #290 | #302 |
| Match digit-grouped numbers (`1,031,369`) in keyword queries | L | S | no thousands-separator handling in the query builder | #291 | #303 |
| Pin keyword behaviour for hyphenated terms with tests | L | S | no tests for hyphenated terms | #292 | #304 |

Follow-up found while merging: a `query` that arrives while the startup download is still running can fail on the hub cache lock or download the same file twice. The hub client takes a per-file lock with a 10 second wait and checks for the blob before taking it. This is a reading of the code, not a run. Tracked in #305.

## Design issues (no code yet)

| Idea | Issue |
|---|---|
| Export and import embedding vectors keyed by content hash, with a fingerprint check | #293 |
| A per-collection rule to embed only chosen sections | #294 |
| An official container image for the MCP HTTP server | #295 |

## Measured

Four ideas needed numbers before anyone builds them (#296). The bar, set in the issue: build a variant only if a retrieval metric improves beyond run-to-run noise, no metric regresses, and latency grows by less than 10%.

### Fixture and its limit

The numbers for M1 to M3 come from a public set of scientific abstracts, not from a corpus of notes. The datasets cached on the machine were empty reference stubs, so the public files were fetched into a scratch directory: 50 test queries chosen with a fixed seed (42), the documents judged relevant for them, and random distractors up to 2,000 documents. The index was built from scratch in a throwaway directory (2,017 chunks) and the live index was never written to. Abstracts differ from Markdown notes and repository docs, so a do-not-build below means **not proven on abstracts**, not disproved on notes. The way to reopen any of them is a small set of judged queries over a real notes corpus (see "What would change this").

Baseline (hybrid, rerank and expansion on, 50 of 50 queries scored, k=10): nDCG@10 0.7589, MRR@10 0.7084, Recall@10 0.9, Hit@10 0.9. BM25 alone: nDCG@10 0.5870, MRR@10 0.5732, Recall@10 0.664, Hit@10 0.7. Two identical baseline runs on separate index copies gave identical metrics to the last digit, so the fixture shows no metric noise.

Command, for baseline and each variant, with its own copy of the index:

```
RQMD_INDEX_DIR=<scratch index copy> <binary> eval --judgements <judgements.jsonl> --mode hybrid --json
```

### Results

| # | Idea | Baseline | Variant | Verdict |
|---|---|---|---|---|
| M1 | Drop `lex:` expansion lines that are only filler or share no content term with the query | hybrid nDCG@10 0.7589; bm25 0.5870 | identical on all four metrics in both modes | do-not-build |
| M2 | Make the 256-token expansion budget configurable | nDCG@10 0.7589, MRR@10 0.7084 | N=64: +0.0003 / +0.0003; N=128: 0 / 0; N=512: +0.0026 / +0.0033; Recall@10 and Hit@10 stay 0.9 | do-not-build |
| M3 | Show the passage from the chunk the vector search matched | ranking and metrics identical | the shown passage changed in 0 of 500 hits (447 same, 53 without a vector hit) | do-not-build |
| M4 | Keep sections of short instruction files as separate chunks | 224 short instruction documents, 224 chunks | 591 chunks, 105 documents would split, total chunks 160,793 to 161,160 (+0.23%) | do-not-build for now |

- **M1.** Identical metrics cannot tell "the filter dropped nothing" from "it dropped lines that did not change the top 10". Whether any line was dropped was not counted.
- **M2.** The largest gain, N=512, is about one query moving up one rank on 50 queries, which is inside what 50 queries can show, and a larger budget costs generation time. Nothing here supports a configurable budget.
- **M3.** Document-level metrics cannot see a change that returns the same documents in the same order. The count of changed passages is the only evidence, and it is zero on this fixture.
- **M4.** Counted read-only on a live index of mostly notes and repository docs, through an ignored test that opens the index read-only. The cost is small, but whether results improve cannot be measured without judgements on those instruction files.

Latency was not measured. The machine was busy (load average above 13), and every variant already failed the first condition of the bar, so the latency condition was never reached.

Reproducing: the variant code was never committed and has been discarded, so the measurements cannot be re-run as they stand and a re-test starts by rewriting the variants. Each was a small change gated by an environment variable that was read where the behaviour is decided:

- M1: a filter on `lex:` expansion lines in the expansion parsing (`fts.rs`, `store.rs`), no variable.
- M2: `RQMD_EXPAND_MAX_TOKENS` replacing the 256-token expansion budget in `rqmd-llm`.
- M3: `RQMD_PASSAGE_FROM_VECTOR=1` choosing the shown passage from the chunk the vector search matched, with `RQMD_PASSAGE_FROM_VECTOR_STATS=1` printing per hit whether it differed.
- M4: `RQMD_CHUNK_SHORT_SECTIONS` keeping sections of short instruction files as separate chunks, counted by a read-only ignored test run as `RQMD_MEASURE_INDEX=<live index.sqlite> cargo test -p rqmd-core --test short_section_chunk_measurement -- --ignored --nocapture`, prefixed with `RTK_DISABLED=1` so the output is not condensed.

The eval command above is unchanged and works on any build, so the baseline row can be re-run today.

### What would change this

- A set of judged queries over a real notes corpus (a few dozen queries with the relevant files named) would settle M1 and M2 for that corpus.
- Judgements or snippet labels on instruction files would be needed for M3 and M4.
- A larger positive M2 result at N=512 with a stable per-query gain, and a latency cost under 10%, would reopen M2.

## Dropped after verification

| Idea | Why it was dropped |
|---|---|
| Allowlists for hidden directories | covered by `--hidden` and `--ignore` |
| Override of the query instruction text | the prefix is fixed and part of the embed fingerprint |
| In-memory ingest API | no library users who need it |
| Lock hardening with process start time | the existing lock handling covers the failure |
| Typed vector-search result lines | no consumer |
| `venv` as a default exclude | would hide documents users did index on purpose |
| Compact MCP response mode | no measured size problem |
| A setup-collection command | `collection add` already does it |
| An MCP status snapshot | `status` already answers |
| Frontmatter keys added to the embedded text | would change every vector for an unmeasured gain |
| A newer embedding model | measured and rejected on 2026-10-13, see [performance-adoption.md](2026-10-13-performance-adoption.md) |
| Collapsing hits by content hash across distinct files | would hide separate files with identical text; #289 collapses only hits that resolve to the same absolute path |

## Confidence

High for the adopted items: each has tests that were run and a merged PR. Medium for M1 to M3: measured on one 50-query fixture of abstracts with no run-to-run noise, but not on a corpus like the real index. A later independent re-run of the baseline and of M2 at N=512, each on a fresh copy of the index, reproduced the numbers exactly (nDCG@10 0.7589 and 0.7615, MRR@10 0.7084 and 0.7117). High for the M4 count, which is read-only on a real index.

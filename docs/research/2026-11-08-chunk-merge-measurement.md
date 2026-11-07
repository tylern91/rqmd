# Minimum-chunk merge pass: measurement (#126)

Measurement only. Nothing in `src/` changed: chunk boundaries, the embed fingerprint and the index format are as before. The numbers come from `crates/rqmd-core/tests/chunk_merge_measurement.rs`, run read-only against a live index on 2026-11-08 (55,913 distinct documents, 128,519 chunks, chunker size 3600, overlap 540, window 800, AST chunking not compiled in):

```
RQMD_MEASURE_INDEX=<index.sqlite> cargo test --release -p rqmd-core \
  --test chunk_merge_measurement -- --ignored --nocapture
```

The simulation merges each chunk shorter than the minimum into its previous neighbour (into the next one when it is the first). It counts distinct documents by content hash.

## Result (all collections)

| Minimum | Chunks below | Of those, in single-chunk docs (not mergeable) | Docs that would change | Chunks removed | Largest merged chunk | Merged over 3600 chars |
|---|---|---|---|---|---|---|
| 200 chars | 2,137 (1.7%) | 2,134 | 3 (0.0%) | 3 | 3,909 chars | 2 |
| 400 chars | 5,135 (4.0%) | 5,121 | 14 (0.0%) | 14 | 4,270 chars | 12 |
| 800 chars | 14,507 (11.3%) | 11,046 | 3,460 (6.2%) | 3,461 (2.7%) | 4,617 chars | 3,348 |

The token column is an estimate (chars / 3.5, limit 2,044 tokens, about 7,154 chars), not a tokenizer. No merged chunk comes near that limit: the largest, 4,617 chars, is about 1,320 estimated tokens. CJK text tokenizes denser than the estimate assumes, so the figure is a lower bound for it. The splitter limits are in bytes, while the report counts characters.

## What it says

- Almost every chunk below 200 or 400 chars is a whole document that fits in one chunk. There is nothing to merge it into, so a merge rule cannot remove it.
- At 200 and 400 chars the rule would change 3 and 14 documents out of 55,913. That is no benefit worth a fingerprint change.
- At 800 chars it changes 3,460 documents (6.2%) for 3,461 fewer chunks (2.7%). Those documents get new boundaries, so their chunk text changes and chunk-vector reuse (#131) does not help: they are re-embedded, and `CHUNK_STRATEGY_VERSION` must be bumped, which marks every Markdown vector stale.
- Merged chunks above the 3,600-character target are common at 800 chars (3,348), because a merge ignores the break-point window.

## Recommendation

Do not implement the merge pass. The measured benefit is under 3% fewer chunks even at the most aggressive threshold, against a global stale-vector rebuild. Configurable chunk size and overlap stays deferred as the design note on #126 says; if it is built later, store the setting with the index and ship an explicit re-embed notice in `rqmd status`.

Confidence: high for this index, which is one machine's corpus (mostly Markdown notes and repository docs). A corpus with many short sections could differ; rerunning the test on it is one command. What would change the conclusion: a corpus where sub-minimum chunks mostly sit at the end of multi-chunk documents.

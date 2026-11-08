# Late-interaction reranking: decision (#129)

Scoping decision only, not a measured result. Nothing was measured.

## Question

Issue #129 asked whether a token-level late-interaction scorer (ColBERT-style MaxSim) as an optional stage would improve final ranking over the current single cross-encoder rerank stage at an acceptable latency and index-size cost on a local machine.

## Decision

Not planned for now. #129 is closed `not_planned`. No code, fingerprint or index format changes.

## Why

- The pipeline already reranks: the cross-encoder scores the top `RERANK_CANDIDATE_LIMIT = 20` fusion candidates and its score is blended with the fusion score through `BLEND_HI`/`BLEND_LO` (both in crates/rqmd-core/src/store.rs). A second reranking stage would add to that, not replace a missing stage.
- Storage scales with tokens. A multi-vector index stores one vector per token. rqmd chunks run up to about 3600 characters (roughly 900 or more tokens). Late-interaction models such as small ColBERT variants cap input at 512 positions and are usually configured for a document length of about 180 to 300 tokens. Scoring would need windowed chunks, and the per-document storage would depend on the windowing choice.
- Any stored vector layout has to be part of the embed fingerprint (`marked_fingerprint` in crates/rqmd-core/src/store.rs), so adopting it later means a re-embed for existing indexes and the steps in the repo skill rqmd-embed-fingerprint-changes.
- It is a large change for an optional stage with no observed ranking gap that the current cross-encoder fails to cover. The decision rule in the issue needs a multi-hour out-of-tree benchmark across four corpora, which is not justified without such a gap.
- Whether a suitable small multi-vector model exports to ONNX with its projection layer and runs on the pinned `ort =2.0.0-rc.12`, or has a usable GGUF build for `llama-cpp-2`, is unverified.

## What is already in place

`rqmd eval` can score a judgement file against an existing index (#236, building on #153), so a later benchmark can be run with the repo's own tooling.

## When to reopen

- A user asks for it, or
- `rqmd eval` shows a ranking regression that the cross-encoder blend cannot fix, or
- A small multi-vector model is confirmed to run on the existing `ort` or `llama-cpp-2` backends without a new native dependency.

## Method if reopened

Keep the issue's decision rule: open an implementation issue only if the average gain over the current pipeline is at least 1.0 nDCG@10, no corpus regresses by more than 1.0, query latency stays within 2x of today's reranked query, and the extra storage is bounded and optional. Refinements to apply:

- Window each chunk to the model's document length (or truncate and report the share of chunks truncated) and compute storage after windowing.
- Measure three variants: MaxSim replacing the cross-encoder, MaxSim blended by reciprocal rank fusion, and MaxSim added alongside the cross-encoder (for example as a pre-filter over a wider pool).
- Measure end-to-end latency for both pipelines at the same query-expansion setting, and state which ratio the 2x gate uses.
- Corpora: SciFact, NFCorpus, a code set, a CJK set, and the bundled `rqmd eval` fixtures; run a reduced first pass (SciFact, NFCorpus, fixtures) and extend only if it comes within 0.5 nDCG@10 of the gate.

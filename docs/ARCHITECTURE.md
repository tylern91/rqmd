# Architecture

[← README](../README.md)

## How it works

**Indexing** (`rqmd collection add` / `rqmd update`): each matched file is
read, hashed (SHA-256 of its content — the resulting hash's first 6 hex
characters become the document's `#docid`), and inserted into the BM25 index
immediately. Vector indexing is a separate, deferred step — `rqmd update`
only touches BM25 and bookkeeping, so it stays usable without a model loaded.

**Embedding** (`rqmd embed`): documents that don't have vectors yet (or, with
`--rebuild`, every document) are split into chunks, each chunk is embedded,
and the resulting vectors are added to the HNSW index. `rqmd doctor` can
detect when the chunking parameters or embed model have changed since the
last embed run by comparing a stored fingerprint (below) against the current
configuration.

**Query**: `rqmd search` and `rqmd vsearch` each run a single retrieval mode
directly. `rqmd query` (and the MCP `query` tool) additionally runs LLM-based
query expansion by default — see [Query syntax and expansion](CLI.md#query-syntax-and-expansion)
— fuses every resulting list with RRF, and reranks the fused candidates with
a cross-encoder. As a latency optimization, if the initial BM25 probe on the
raw query already returns a dominant top result (score above a fixed
threshold with a clear gap to the runner-up), expansion is skipped entirely
and the pipeline proceeds directly to reranking that candidate set.

### Smart chunking

Documents longer than the chunk size are split at content-aware break
points rather than at a fixed character offset. The chunker looks for the
highest-scoring break point inside a window centered on the ideal cut
position, scoring candidates by what kind of boundary they are: an H1
heading scores highest, descending through H2–H6, with code-fence
boundaries and horizontal rules scored similarly high, blank lines (paragraph
breaks) lower, list items lower still, and a bare newline as the last
resort. Breaks are never placed inside a fenced code block — if the ideal
cut would land inside one, the search continues past it.

The chunk target is 3,600 characters (roughly 900 tokens at the ~4
characters/token EmbeddingGemma tends toward) with a 540-character (15%)
overlap between consecutive chunks, and the break-point search window
extends 400 characters either side of the ideal cut. Because these figures
are byte offsets into UTF-8 text, and multi-byte characters (accented
letters, em dashes, CJK text) can span two or three bytes each, every
computed cut point is snapped forward or backward to the nearest valid
character boundary before the text is sliced — cutting mid-character would
otherwise corrupt the chunk and panic on decode.

These three constants (embed model name, chunk size, chunk overlap) feed a
short fingerprint hash that gets stored alongside the index. If any of them
changes — a different embed model, or a future tuning pass on the chunking
parameters — the stored fingerprint no longer matches, and `rqmd doctor`
surfaces that the existing vectors were built under a different
configuration and may need re-embedding.

---

## Design decisions

| Aspect | Choice |
|--------|--------|
| Search backend | Tantivy (BM25) + usearch (HNSW) |
| Index location | `~/.cache/rqmd/` (Linux) / `~/Library/Caches/rqmd/` (macOS) |
| Embed model | embeddinggemma-300M (GGUF, Metal/CPU) |
| Rerank model | Qwen3-Reranker-0.6B (GGUF) |
| ORT backend | ✓ CoreML / CUDA / DirectML (feature-gated) |
| Query expansion | ✓ LLM-generated lex/vec/hyde (stock Qwen3-1.7B), fused via RRF |
| MlxBackend | Deferred — `mlx-rs` `Array: !Send` conflicts with parallel embed pool |
| Startup time | ~5ms (no JIT) |

The RRF fusion formula, BM25 field weights, chunking parameters (900 tokens /
15% overlap), and docid scheme (`first 6 hex chars of SHA-256(content)`) match
the original qmd design so search quality is preserved.

See [BENCHMARK.md](../BENCHMARK.md) for the de-risking spike results (inference backend
+ DB bake-off) that drove the Tantivy+usearch and llama-cpp-2 decisions.

---

## Score interpretation

Every result carries a single `score` field, but what that number *means*
depends on which stage produced it — and the three stages are not on a
common scale, so comparing a 0.62 from one query to a 0.71 from another
tells you very little on its own.

- **BM25 / FTS score** (`rqmd search`): Tantivy's raw BM25 score is a
  positive, unbounded value where higher is better, but its magnitude isn't
  meaningful in isolation — it depends on term rarity and document length.
  rqmd squashes it into a fixed `[0, 1)` range with the monotonic
  transform `score / (1 + score)`. This preserves ordering exactly (it's
  monotonic) and needs no per-query normalization, but it means a 0.9 from
  one query and a 0.9 from another aren't "equally good" in any absolute
  sense — the transform only makes the *number* boundable, not
  cross-query-comparable.
- **Vector / cosine score** (`rqmd vsearch`): embeddings are unit-normalized,
  so nearest-neighbor search reduces to cosine similarity. The underlying
  index returns a distance where 0 means identical; rqmd reports
  `1 - distance` as the similarity, so 1.0 is a perfect match and scores
  trend toward 0 (or negative, in principle) as vectors diverge.
- **Hybrid / fused score** (`rqmd query`): the default pipeline runs BM25
  and vector search (plus LLM-expanded lex/vec/hyde variants when expansion
  is active) as separate ranked lists, then fuses them with Reciprocal Rank
  Fusion. Each list contributes `weight / (60 + rank + 1)` per document,
  where `rank` is that document's zero-based position within its list
  after collapsing duplicate chunks down to one entry per document. The
  original query's own lists get weight 2.0; expansion-derived lists get
  weight 1.0. Whichever list ranks a document at position 0 anywhere adds a
  flat +0.05 bonus; ranking at position 1 or 2 (without hitting position 0)
  adds +0.02. These bonuses apply unconditionally — they aren't gated on
  how far ahead of the next result a document is — so the fused score is a
  relative ranking signal assembled from several inputs, not a probability
  or confidence value. When reranking runs, the cross-encoder's score for a
  candidate is blended back in as `0.75 * rerank_score + 0.25 * rrf_score`;
  rerank scores themselves are not normalized across pairs, so this blend
  is only meaningful as a way of biasing the RRF ordering, not as an
  absolute quality metric either.

The practical takeaway: use scores to compare results *within* one query's
result set, not across different queries or across `search`/`vsearch`/`query`
modes. Because RRF's OR-semantics mean a document can rank purely from
matching one of several expansion variants, plus the unconditional rank
bonuses above, there's no fixed score threshold below which a result is
"irrelevant" — a low fused score can still be the best available match for
a query with generally weak recall.

---

## Workspace layout

```
rqmd/                    # repo root = Cargo workspace
├── Cargo.toml           # workspace definition + release/dist profiles
├── .cargo/config.toml   # MUSL static build target config
├── crates/
│   ├── rqmd-core/       # engine: search, chunking, store, collections
│   ├── rqmd-llm/        # inference backends (LlamaCpp + ORT)
│   ├── rqmd-cli/        # CLI entry point (clap)
│   └── rqmd-mcp/        # MCP server (rmcp, stdio + HTTP)
├── docs/                # SYNTAX.md, CLI.md, MODELS.md, MCP.md,
│                        # CONFIGURATION.md, ARCHITECTURE.md, CRATE-API.md,
│                        # MIGRATING.md, TROUBLESHOOTING.md, INSTALL.md
└── assets/              # rqmd_architecture.svg
```

### Build profiles

| Profile | Command | LTO | Strip | Use |
|---------|---------|-----|-------|-----|
| `dev` | `cargo build` | off | none | development |
| `release` | `cargo build --release` | thin | debuginfo | testing |
| `dist` | `cargo build --profile dist` | fat | symbols | release binary |

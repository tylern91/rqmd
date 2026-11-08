# Models & Inference Backends

[← README](../README.md)

## Inference backends

Two backends are available, selected at runtime via env var or `--backend` flag.

### LlamaCppBackend (default)

Uses [llama-cpp-2](https://github.com/utilityai/llama-cpp-rs) to run GGUF models locally.

| Role | Model | Size |
|------|-------|------|
| Embeddings | `ggml-org/embeddinggemma-300M-GGUF` | ~300MB |
| Reranking | `ggml-org/Qwen3-Reranker-0.6B-Q8_0-GGUF` | ~600MB |

Models are downloaded automatically from HuggingFace on first use and cached
at `~/.cache/huggingface/hub/`.

On macOS, llama.cpp uses Metal (Apple GPU) automatically. On Linux, CPU-only
unless CUDA is available.

```sh
rqmd embed                          # uses LlamaCppBackend by default
rqmd --backend llama embed          # explicit
RQMD_INFERENCE_BACKEND=llama rqmd embed
```

### OrtBackend (`ort-backend` feature)

Uses [ONNX Runtime](https://ort.pyke.io/) with pluggable execution providers.
Build with `--features ort-backend`.

| Role | Model | Size |
|------|-------|------|
| Embeddings | `BAAI/bge-base-en-v1.5` (ONNX) | ~440MB |
| Reranking | *(not supported — falls back to LlamaCppBackend)* | — |

Execution providers selected by `--ort-ep` or `RQMD_ORT_EP`:

| EP | Flag | Platform | Hardware |
|----|------|----------|----------|
| CoreML | `coreml` | macOS | Apple Neural Engine + GPU |
| CUDA | `cuda` | Linux / Windows | NVIDIA GPU |
| DirectML | `directml` | Windows | Any GPU via DirectML |
| CPU | `cpu` | All | CPU fallback |
| Auto | `auto` (default) | All | CoreML on macOS, CPU elsewhere |

```sh
# CoreML (Apple Neural Engine — fastest for embed-sized models on M-series)
RQMD_INFERENCE_BACKEND=ort RQMD_ORT_EP=coreml rqmd embed
rqmd --backend ort --ort-ep coreml embed
```

**Pinned revision and checksums.** The ORT backend downloads `BAAI/bge-base-en-v1.5` at a fixed commit and verifies both files against SHA-256 hashes, the same way the GGUF models are:

| File | SHA-256 |
|------|---------|
| `onnx/model.onnx` | `9bc579acdba21c253c62a9bf866891355a63ffa3442b52c8a37d75b2ccb91848` |
| `tokenizer.json` | `d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66` |

The revision is `a5beb1e3e68b9ab74eb54cfd186867f64f240e1a`, the `main` branch head as fetched on 2026-11-07 (last modified 2024-02-21), so the pinned bytes are the ones `main` served before the pin. The model hash is HuggingFace's published LFS SHA-256. `tokenizer.json` is not an LFS file and HuggingFace publishes no SHA-256 for it (only the git blob id `688882a79f44442ddc1f60d70334a7ff5df0fb47`), so its hash was computed locally from the bytes served at that commit. A file that does not match fails with an error naming the file, the expected hash and the hash found; the unverified bytes are never loaded. `HF_HUB_OFFLINE=1` is honoured, and the cache lives where the GGUF models do (`$HF_HOME/hub/`, otherwise `~/.cache/huggingface/hub/`). A model snapshot cached before the pin is hashed once and then reused; the small tokenizer is hashed on every start, and a download that fails its check is deleted from the cache so the next run fetches it again.

The pin does not change the embed fingerprint. The fingerprint is `BAAI/bge-base-en-v1.5/onnx/model.onnx` and carries no revision, so an existing ORT index stays valid and nothing is re-embedded.

To bump the model deliberately: read the new commit from `https://huggingface.co/api/models/BAAI/bge-base-en-v1.5` (the `sha` field) and the model's `lfs.sha256` from the same URL with `?blobs=true`, download `tokenizer.json` at that commit (`https://huggingface.co/BAAI/bge-base-en-v1.5/resolve/<commit>/tokenizer.json`) and hash it with `shasum -a 256`, then update `DEFAULT_ORT_EMBED_REVISION`, `DEFAULT_ORT_EMBED_SHA256` and `DEFAULT_ORT_TOKENIZER_SHA256` in `crates/rqmd-llm/src/lib.rs` together. If the new weights embed differently, the fingerprint must change too, so that indexes built with the old weights are flagged for re-embedding.

---

## Models

| Role | Backend | Model | Size |
|------|---------|-------|------|
| Embeddings | LlamaCpp (default) | `ggml-org/embeddinggemma-300M-GGUF` | ~300 MB |
| Reranking | LlamaCpp | `ggml-org/Qwen3-Reranker-0.6B-Q8_0-GGUF` | ~600 MB |
| Embeddings | ORT (`ort-backend` feature) | `BAAI/bge-base-en-v1.5` (ONNX) | ~440 MB |
| Query expansion | LlamaCpp | `ggml-org/Qwen3-1.7B-GGUF` (`Qwen3-1.7B-Q8_0.gguf`) | ~1.7 GB |

Models download automatically from HuggingFace on first use (~900 MB for embed + rerank; ~2.6 GB with query expansion) and are cached at `~/.cache/huggingface/hub/`. All three model repos are public — no token is required for a normal download.

**Cache location.** Models live under `$HF_HOME/hub/` when `HF_HOME` is set, otherwise `~/.cache/huggingface/hub/` (hf-hub's `Cache::from_env`; `HF_HUB_CACHE` is not read). `rqmd doctor` prints the resolved cache root and whether each model is cached. To move the cache, set `HF_HOME` for every rqmd process, including the MCP daemon and any launchd or systemd unit. To pre-seed a machine or share one cache between users, copy or mount the `models--<org>--<repo>` directories into that `hub/` directory; models already present are used without a download. To free the space, delete those directories; the next run downloads them again.

Set `HF_ENDPOINT` to use a mirror, or `HF_HUB_OFFLINE=1` to disable downloads entirely and require every model to already be cached — rqmd fails with an actionable error naming the missing file instead of attempting a request. If `~/.cache/huggingface/token` holds an expired or invalid token, rqmd retries anonymously rather than failing outright; set `HF_TOKEN` or `HUGGING_FACE_HUB_TOKEN` to use a specific token instead (checked in that order, ahead of the cached token file).

---

## Model configuration

rqmd's default backend uses three local GGUF models: an embedding model, a
reranker, and a small generation model for query expansion.

**Asymmetric embedding prompts**: the embedding model documents different
recommended prompt templates for queries versus the passages being searched,
and rqmd honors that distinction rather than embedding both sides
identically. Query-side text (and HyDE's hypothetical-document text, which
plays a query's role in the pipeline) is prefixed with
`"task: search result | query: "`; passage-side text (indexed document
chunks) is prefixed with `"title: none | text: "` — the "no title" form of
the documented template, since threading real per-chunk titles through
wasn't judged worth the added plumbing. Using the wrong prefix on either
side doesn't break the pipeline, but it does mean queries and documents
embed into slightly different regions of the vector space than the model
intends, degrading retrieval quality.

**Reranking**: the cross-encoder scores a query/document pair using the
template `"Query: {query}\nDocument: {doc}"`. As noted in
[Score interpretation](ARCHITECTURE.md#score-interpretation), these scores are not
normalized across pairs — they're only meaningful as a relative ordering
within one rerank call, which is why they're blended with, rather than
replacing, the RRF score. When the rerank model fails or returns an unusable
score list, the blend is skipped and results keep their fusion order; see
[Rerank fallback](CLI.md#rerank-fallback).

**Query expansion**: the generation model is prompted with a ChatML-formatted
instruction asking it to produce short `lex:`/`vec:`/`hyde:` lines — a
literal-keyword variant, a semantic-phrasing variant, and a hypothetical
answer passage, respectively — which are parsed back out of its free-form
output and fed into the fusion pipeline as separate ranked lists.

**Backend parity note**: the local llama.cpp-based backend explicitly
L2-normalizes every embedding vector it returns, even though the embedding
model's own output is expected to already be close to unit length. This
exists so that the local and ONNX Runtime backends produce vectors with
identical normalization guarantees — cosine similarity is scale-invariant,
so a vector that's merely *close to* unit length would still rank
identically within a single backend's own index. The normalization matters
for consistency of the stored contract across backends, not for
within-backend ranking correctness.

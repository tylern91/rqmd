# Troubleshooting

[← README](../README.md)

## cmake version requirements

cmake ≥3.14 is required. cmake 4.x is supported — the `llama-cpp-sys-2` crate
(which builds llama.cpp from source) builds correctly with cmake 4.x on macOS
and Linux. You do not need to pin or downgrade cmake.

**Do not** add `target-cpu` flags to `.cargo/config.toml` — they change the
llama-cpp-sys fingerprint and force a cmake rebuild. Pass them at build time:

```sh
RUSTFLAGS="-C target-cpu=native" cargo build --profile dist -p rqmd-cli
```

## Model downloads are slow / fail

Models are fetched from HuggingFace on first `rqmd embed` and cached at
`~/.cache/huggingface/hub/`. Set `HF_ENDPOINT` for a mirror, or
`HF_HUB_OFFLINE=1` to require every model to already be cached (fails fast
with the expected file path instead of trying the network).

`rqmd doctor` reports which models are cached without downloading anything —
run it first if `rqmd embed` reports a model as missing unexpectedly.

**401 Unauthorized**: these model repos are public, so a 401 almost always
means a stale token, not a permissions problem. rqmd retries anonymously if
the token in `~/.cache/huggingface/token` is rejected; if the retry also
fails, run `huggingface-cli login` to refresh it, or delete that file to
download anonymously. Set `HF_TOKEN` or `HUGGING_FACE_HUB_TOKEN` to use a
specific token instead of the cached one.

## "OrtBackend: reranking not supported"

`OrtBackend` handles embeddings only. Reranking uses `LlamaCppBackend`
automatically as a fallback.

## `rqmd doctor` reports orphaned vectors

An orphaned vector is a `content_vectors` row whose hash has no active
document referencing it — left behind when a file is removed or renamed.
It's unreachable in search, but still occupies disk until reclaimed.

Run `rqmd embed --cleanup` to reclaim it: it sweeps orphaned vectors, deletes
`content` rows referenced by no document, and `VACUUM`s the database — no
model load, no re-embed, done in seconds regardless of corpus size.

`hnsw.usearch` will **not** shrink afterward — usearch has no compaction
API, so freed vector slots are reused rather than returned to the
filesystem. Only `index.sqlite` visibly drops in size; that's expected.

**`--rebuild` is for a stale fingerprint, not for orphans.** It re-embeds
everything under the current model/chunker and is the right tool when
`doctor` reports a stale `embed_fingerprint`, not when it reports orphaned
vectors.

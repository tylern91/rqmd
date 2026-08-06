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

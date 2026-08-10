# Migrating from qmd

[← README](../README.md)

## Differences from qmd

| Feature | qmd (TypeScript) | rqmd (Rust) |
|---------|-----------------|-------------|
| Runtime | Node.js required | Self-contained static binary |
| Startup | ~300 ms (JIT) | ~5 ms |
| Search pipeline | BM25 + vector + RRF + rerank | Same pipeline, same parameters |
| MCP server identity | `qmd` | `rqmd` |
| Chunking | tree-sitter AST-aware | Regex heuristic (headings, code fences, lists) |
| Index location | `~/.cache/qmd/` | `~/.cache/rqmd/` (Linux) / `~/Library/Caches/rqmd/` (macOS) |
| File exclusion | `.gitignore` aware | Built-in exclusions + per-collection `ignore` list |

Search quality is equivalent — the RRF formula, BM25 field weights, chunk size (900 tokens / 15% overlap), and docid scheme are all ported verbatim from qmd.

---

## Migrating from qmd

rqmd uses its own index at `~/.cache/rqmd/` (Linux) / `~/Library/Caches/rqmd/` (macOS) — existing qmd collections need to be re-added:

```sh
rqmd collection add ~/path/to/your/docs --name your-collection
rqmd embed
```

All environment variables are prefixed `RQMD_` instead of `QMD_`:

| Old (qmd) | New (rqmd) |
|-----------|----------|
| `QMD_INDEX_DIR` | `RQMD_INDEX_DIR` |
| `QMD_INFERENCE_BACKEND` | `RQMD_INFERENCE_BACKEND` |
| `QMD_ORT_EP` | `RQMD_ORT_EP` |
| `QMD_FORCE_CPU` | `RQMD_FORCE_CPU` |

The MCP server now identifies as `rqmd` — update any `claude_desktop_config.json` entries accordingly.

---

## Migrating from rqmd ≤ 0.10.x

v0.11.0 drops the doubled-R `RRQMD_` environment variable prefix in favor of
`RQMD_`, matching the binary, crate, and URI scheme naming everywhere else in
the product. There is no compatibility shim — set the new names before
upgrading:

| Old (≤ 0.10.x) | New (≥ 0.11.0) |
|-----------------|----------------|
| `RRQMD_INDEX_DIR` | `RQMD_INDEX_DIR` |
| `RRQMD_INFERENCE_BACKEND` | `RQMD_INFERENCE_BACKEND` |
| `RRQMD_ORT_EP` | `RQMD_ORT_EP` |
| `RRQMD_FORCE_CPU` | `RQMD_FORCE_CPU` |
| `RRQMD_MCP_HOST` | `RQMD_MCP_HOST` |
| `RRQMD_MCP_PORT` | `RQMD_MCP_PORT` |
| `RRQMD_MCP_ALLOW_NON_LOOPBACK` | `RQMD_MCP_ALLOW_NON_LOOPBACK` |
| `RRQMD_MODEL_IDLE_TTL` | `RQMD_MODEL_IDLE_TTL` |
| `RRQMD_NO_EXPAND` | `RQMD_NO_EXPAND` |
| `RRQMD_VERBOSE` | `RQMD_VERBOSE` |

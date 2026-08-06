# Configuration

[← README](../README.md)

## Excluding files

By default rqmd indexes every file matching a collection's mask pattern
(`**/*.md`). It reads **no** ignore files — `.gitignore` and `.ignore` are
never consulted.

**Built-in exclusions** (skipped unless overridden):

- Hidden files and directories — any path component (relative to the
  collection root) starting with `.`. Pass `--hidden` when adding a
  collection to index these. This only affects components *inside* the
  tree: the collection root itself is never subject to this rule, so adding
  a dot-prefixed directory (e.g. `~/.dotfiles`) as a collection always works
  regardless of `--hidden`.
- `node_modules`, `vendor`, `dist`, `build`, `target`, `.cache` — these are
  always skipped and are not affected by `--hidden`.

```sh
# Index hidden files/dirs under the collection root too
rqmd collection add ~/notes --hidden
```

**`--mask` — which files are candidates for indexing:**

The mask is a glob (default `**/*.md`) evaluated relative to the collection
root. Two ways to match more than one pattern:

```sh
# Brace alternation — a single glob, one pattern
rqmd collection add ~/notes --mask '**/*.{md,mdx,txt}'

# Comma-separated globs — split on top-level commas only (commas inside
# {...} are left alone, so the two forms can be combined)
rqmd collection add ~/notes --mask '**/*.md,**/*.txt'

# Directory-scoped glob — restrict to a subtree
rqmd collection add ~/docs --mask 'docs/**/*.md'
```

An invalid `--mask` glob is a hard error at `collection add` time (it decides
what gets indexed at all, so a silently-dropped alternative would mean
documents vanish from the index with no diagnostic). An invalid `--ignore`
glob, by contrast, is silently skipped — an unmatchable extra exclusion is
harmless.

**Per-collection ignore patterns** (gitignore-style globs):

```sh
# Exclude patterns when adding a collection
rqmd collection add ~/notes --ignore '*.log' --ignore 'tmp/'

# Multiple patterns are combined with OR — any match excludes the file
rqmd collection add ~/docs --ignore 'drafts/**' --ignore '**/node_modules'
```

Mask and ignore patterns are stored with the collection and apply on every
subsequent `rqmd update` run — you only need to specify them once.

---

## Environment variables

| Variable | Values | Default | Description |
|----------|--------|---------|-------------|
| `RRQMD_INDEX_DIR` | path | `~/.cache/rqmd/` (Linux) / `~/Library/Caches/rqmd/` (macOS) | Index storage directory |
| `RRQMD_INFERENCE_BACKEND` | `llama`, `ort` | `llama` | Inference backend |
| `RRQMD_ORT_EP` | `auto`, `coreml`, `cuda`, `directml`, `cpu` | `auto` | ONNX Runtime EP |
| `RRQMD_FORCE_CPU` | `1` | *(unset)* | Disable GPU layers in LlamaCppBackend |
| `RRQMD_MCP_HOST` | host/IP | `127.0.0.1` | Bind address for `rqmd mcp --http`/`--daemon` |
| `RRQMD_MCP_PORT` | port number | `8181` | Bind port for `rqmd mcp --http`/`--daemon` |
| `RRQMD_MODEL_IDLE_TTL` | seconds | `300` | MCP daemon: unload an idle model after this many seconds of no use; `0` disables eviction |
| `RRQMD_NO_EXPAND` | `1` | *(unset)* | Equivalent to always passing `--no-expand` to `rqmd query` |
| `RRQMD_VERBOSE` | `1` | *(unset)* | Verbose ORT backend logging |

---

## Where data lives

Paths below use the Linux default; on macOS the base is `~/Library/Caches/` instead of `~/.cache/` (from `dirs::cache_dir()`).

| What | Path |
|------|------|
| Index + collections (SQLite) | `~/.cache/rqmd/index.sqlite` |
| BM25 index (Tantivy) | `~/.cache/rqmd/tantivy/` |
| Vector index (usearch) | `~/.cache/rqmd/hnsw.usearch` |
| Model cache (HuggingFace) | `~/.cache/huggingface/hub/` |
| Project-local index | `.rqmd/` (created by `rqmd init`) |

Override the root index directory with `--index-dir <path>` or `$RRQMD_INDEX_DIR`.

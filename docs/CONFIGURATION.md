# Configuration

[← README](../README.md)

## Excluding files

By default rqmd indexes every file matching a collection's mask pattern
(`**/*.md`). It reads **no** ignore files unless the collection opts in with
`--respect-gitignore` (see below); `.ignore` is never consulted.

**Built-in exclusions** (skipped unless overridden):

- Hidden files and directories — any path component (relative to the
  collection root) starting with `.`. Pass `--hidden` when adding a
  collection to index these. This only affects components *inside* the
  tree: the collection root itself is never subject to this rule, so adding
  a dot-prefixed directory (e.g. `~/.dotfiles`) as a collection always works
  regardless of `--hidden`.
- `node_modules`, `vendor`, `dist`, `build`, `target`, `.cache` — these are
  always skipped and are not affected by `--hidden`. The walk does not
  descend into them, so a large `node_modules` or `target` tree costs nothing
  on `rqmd update`. `collection add` and `update` name the folders they left
  out on the summary line, relative to the collection root and capped at five
  names:
  `left out 2 folder(s) by the built-in rule (build/, node_modules/)`. The
  clause is absent when nothing was left out. A folder inside a left-out
  folder is not listed separately, and hidden folders are not listed.

```sh
# Index hidden files/dirs under the collection root too
rqmd collection add ~/notes --hidden
```

**`--respect-gitignore` — skip what the tree's `.gitignore` files match:**

Off by default, so a collection never silently loses documents. When on, the
`.gitignore` files inside the collection tree are applied while walking it.
The setting is stored with the collection and used by every later `update`.
Only `.gitignore` files in the tree are read: no global ignore file, and the
tree does not have to be a git repository. Symlinks that resolve outside the
collection root stay excluded either way.

```sh
rqmd collection add ~/code/project --mask '**/*.md' --respect-gitignore
```

**`--max-file-bytes` — skip very large files:**

Files larger than the limit are not read, chunked or embedded. The default is
10485760 bytes (10 MiB). Files left out for size are counted in the
`collection add` and `update` summaries (`skipped N (M over the size limit)`),
and a file that grows past the limit is removed from the index on the next
`update` like any file that is no longer present.

```sh
rqmd collection add ~/notes --max-file-bytes 1048576
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
glob is also a hard error at `collection add` time: the message names the
pattern and nothing is stored, because a mistyped pattern would otherwise
exclude nothing without saying so. A pattern that is already stored and
invalid (from an earlier release) does not stop `rqmd update`; it prints
`WARN: collection '<name>': ignoring invalid ignore pattern '<pattern>'` and
the remaining patterns still apply.

**Per-collection ignore patterns** (gitignore-style globs):

```sh
# Exclude patterns when adding a collection
rqmd collection add ~/notes --ignore '*.log' --ignore 'tmp/'

# Multiple patterns are combined with OR — any match excludes the file
rqmd collection add ~/docs --ignore 'drafts/**' --ignore '**/node_modules'
```

Mask and ignore patterns are stored with the collection and apply on every
subsequent `rqmd update` run — you only need to specify them once.
`rqmd collection show <name>` lists the stored ignore patterns (or `(none)`).

**Collection names**: a name must not be empty or contain `/` (a `/` would
make `collection/path` ambiguous in `rqmd get` and `rqmd://` URIs). Both
`collection add --name` and `collection rename` reject such names. Running
`collection add` again for a name that already exists updates its path, mask,
ignore, hidden, gitignore and size settings from the new flags, and keeps the
update hook (`collection update-cmd`), the include/exclude setting and the
embed exclusions below.

## Excluding documents from embedding

A document can stay searchable by keyword (`rqmd search`, and the full-text
side of `rqmd query`) while `rqmd embed` never encodes it. This suits large or
low-value files such as generated sources, logs and exports. Both settings are
per collection, off by default, and changed with `rqmd collection set` at any
time after `collection add`:

```sh
# Leave generated files and logs out of the vector index
rqmd collection set notes --embed-ignore 'generated/**' --embed-ignore '*.log'

# Do not embed documents whose indexed text is over 64 KiB
rqmd collection set notes --embed-max-bytes 65536

# Remove the rules again
rqmd collection set notes --clear-embed-ignore --clear-embed-max-bytes
```

- **`--embed-ignore PATTERN`** (repeatable) uses the same glob syntax as
  `--ignore`, matched against the path relative to the collection root. The
  patterns given replace the stored list. A malformed glob fails the command
  and nothing is stored.
- **`--embed-max-bytes BYTES`** is measured on the indexed text, which is what
  the embedder reads: the file with its frontmatter block removed (plus any
  `tags:`/`aliases:` terms). A file whose frontmatter makes it larger than the
  cap, but whose indexed text is under it, is still embedded. This cap is
  separate from `--max-file-bytes`, which leaves a file out of the index
  entirely.
- The settings are stored with the collection, so `collection show` lists them
  (`Embed ignore:` and `Embed max:` lines, shown only when set), re-adding the
  collection keeps them, and `collection rename` carries them over.
- They are not part of the embed fingerprint. Adding, changing or clearing a
  rule re-embeds nothing; it changes only which documents the next
  `rqmd embed` considers. Clearing a rule makes its documents pending again.
- Excluded documents do not count as pending. `rqmd status` and `rqmd doctor`
  stop listing them, and `rqmd embed` says `All content hashes already have
  embeddings` when only excluded documents lack vectors.
- **Vectors that already exist** for a document that becomes excluded are kept
  by a plain `rqmd embed` and dropped by `rqmd embed --cleanup`. Until then
  vector search never returns the excluded document. `rqmd embed --rebuild`
  clears vectors and does not re-create them for excluded documents.
- **Shared content.** Vectors are stored once per distinct text and shared
  across collections. When an excluded document has the same text as a
  document that is embedded elsewhere, `rqmd vsearch` and the vector side of
  `rqmd query` skip the excluded copy, scoped or unscoped, and return only the
  document that may be embedded. `rqmd embed --cleanup` keeps such shared
  vectors.

When at least one collection has a rule or a cap, `rqmd status` adds a line
under `Documents`, and `rqmd doctor` adds an `Embed exclusions:` line and, when
there are any, the number of vectors kept for excluded documents:

```text
  Excluded: 12 from embedding (9 by rule, 3 over the size cap)
```

With no rule and no cap on any collection, the output of `status`, `doctor`,
`embed` and `collection show` is the same as before these settings existed.

---

## Environment variables

| Variable | Values | Default | Description |
|----------|--------|---------|-------------|
| `RQMD_INDEX_DIR` | path | `~/.cache/rqmd/` (Linux) / `~/Library/Caches/rqmd/` (macOS) | Index storage directory |
| `RQMD_INFERENCE_BACKEND` | `llama`, `ort` | `llama` | Inference backend |
| `RQMD_ORT_EP` | `auto`, `coreml`, `cuda`, `directml`, `cpu` | `auto` | ONNX Runtime EP |
| `RQMD_FORCE_CPU` | `1` | *(unset)* | Disable GPU layers in LlamaCppBackend. Without it, a model that fails to load on the GPU (or whose context cannot be allocated there) is retried on the CPU with a warning, and stays on the CPU for the rest of the process |
| `RQMD_MCP_HOST` | host/IP | `127.0.0.1` | Bind address for `rqmd mcp --http`/`--daemon` |
| `RQMD_MCP_PORT` | port number | `8181` | Bind port for `rqmd mcp --http`/`--daemon` |
| `RQMD_MCP_ALLOW_NON_LOOPBACK` | `1` | *(unset)* | Required alongside a non-loopback `RQMD_MCP_HOST`/`--host` — confirms you understand this exposes the index's full-text and semantic search (and `get`, which returns file content) with no authentication; see [SECURITY.md](../SECURITY.md) |
| `RQMD_MCP_FTS_READERS` | count | `min(4, cores)` | MCP server: number of read-only store handles serving `search`/`get`/`multi_get`/`status` concurrently (`1`–`16`); each holds its own SQLite connection, Tantivy reader and mmap'd vector index |
| `RQMD_HOOK_TIMEOUT_SECS` | seconds | `300` | `rqmd update`: kill a collection's `update_command` hook after this long (positive whole number; anything else uses the default) |
| `RQMD_MCP_TOOL_TIMEOUT_SECS` | seconds | `120` | MCP server: give up on a `query` call after this long (positive whole number; anything else uses the default). Starts once the call holds the model store, so waiting for another `query` or for model load is not charged; checked between stages, so one running model call is not interrupted. A deadline that passes before or during reranking returns the unreranked results with a note, unless the call sets `strict_timeout: true`. It also bounds a `refresh: true` pass on `search` or `query`, counted from the start of the pass and checked between files; a pass that stops at it keeps what it indexed and adds a note (see [Refreshing before a search](MCP.md#refreshing-before-a-search)) |
| `RQMD_MODEL_IDLE_TTL` | seconds | `300` | MCP daemon: unload an idle model after this many seconds of no use; `0` disables eviction |
| `RQMD_MCP_IDLE_EXIT_SECS` | seconds | *(unset)* | MCP HTTP server and daemon: exit after this many seconds with no request. Unset means the server runs until it is stopped. The value must be a positive whole number; `0` or anything else is an error and the server does not start. Only requests to `/mcp` count; open event streams and health checks (`/health`, `rqmd mcp status`) do not. A request that is still being answered keeps the server up. See [Idle exit](MCP.md#idle-exit) |
| `RQMD_NO_EXPAND` | `1` | *(unset)* | Equivalent to always passing `--no-expand` to `rqmd query` |
| `RQMD_QUERY_CACHE` | `1`, `true`, `yes`, `on` (or `0`, `false`, `no`, `off`) | *(unset)* | Equivalent to always passing `--cache` to `rqmd query`: reuse expansion output and rerank scores from `model_cache.sqlite` in the index directory. `--no-cache` overrides it for one run. The file keeps query-derived text and scores on disk; see [Query cache](CLI.md#query-cache) |
| `RQMD_VERBOSE` | `1` | *(unset)* | Verbose ORT backend logging |
| `NO_COLOR` | any value | *(unset)* | Turn off ANSI colour on a terminal. See [Colour and progress output](CLI.md#colour-and-progress-output) |
| `TERM` | `dumb` | *(from the terminal)* | `dumb` turns off colour and prints progress as plain newline-terminated lines instead of redrawing one |

---

## Where data lives

Paths below use the Linux default; on macOS the base is `~/Library/Caches/` instead of `~/.cache/` (from `dirs::cache_dir()`).

| What | Path |
|------|------|
| Index + collections (SQLite) | `~/.cache/rqmd/index.sqlite` |
| BM25 index (Tantivy) | `~/.cache/rqmd/tantivy/` |
| Vector index (usearch) | `~/.cache/rqmd/hnsw.usearch` |
| Query cache (opt-in, `rqmd query --cache`) | `~/.cache/rqmd/model_cache.sqlite` |
| Model cache (HuggingFace) | `~/.cache/huggingface/hub/` |
| Project-local index | `.rqmd/` (created by `rqmd init`) |

Override the root index directory with `--index-dir <path>` or `$RQMD_INDEX_DIR`.

# CLI Reference

[← README](../README.md)

## Commands

| Command | Description |
|---------|-------------|
| `rqmd query <text> [--no-expand]` | Hybrid search: BM25 + vector + rerank + LLM query expansion |
| `rqmd search <text>` | BM25 keyword search only |
| `rqmd vsearch <text>` | Vector similarity only |
| `rqmd similar <path\|#docid>` | Find documents most similar to an already-indexed one |
| `rqmd get <path\|#docid>` | Retrieve document by path or content hash |
| `rqmd multi-get <glob>` | Retrieve multiple documents |
| `rqmd ls [collection[/path]]` | List collections or files |
| `rqmd embed [-c collection] [--rebuild]` | Generate embeddings (`--rebuild`: clear vectors and re-embed from scratch) |
| `rqmd update [-c collection]` | Re-index: reports new, updated, unchanged, and removed (soft-deleted) document counts |
| `rqmd status` | Index health and collection summary |
| `rqmd doctor` | Diagnose config, index, model, and device issues |
| `rqmd bench [-n N]` | Embed throughput benchmark (default: 5 rounds) |
| `rqmd eval [--mode bm25\|vec\|hybrid] [--verbose]` | Search quality eval against synthetic fixtures |
| `rqmd mcp [--http] [--port N] [--host HOST] [--daemon]` | Start MCP server |
| `rqmd mcp status` | Show MCP daemon status (pid, health, uptime) |
| `rqmd mcp stop` | Stop the running MCP daemon |
| `rqmd collection add <path> [--mask PATTERN] [--ignore PATTERN] [--hidden]` | Add a directory as a collection |
| `rqmd collection list` | List all collections |
| `rqmd collection remove <name>` | Remove a collection |
| `rqmd collection rename <old> <new>` | Rename a collection |
| `rqmd collection show <name>` | Show collection details |
| `rqmd collection update-cmd <name> [cmd]` | Set/clear pre-update hook |
| `rqmd collection include/exclude <name>` | Toggle from default queries |
| `rqmd context add [path] <text>` | Add context for a path |
| `rqmd context list` | List all contexts |
| `rqmd context rm <path>` | Remove context |
| `rqmd context check` | Find paths missing context |
| `rqmd init` | Create a project-local `.rqmd` index |

Global flags (before the subcommand):

```
--index-dir <path>       Override index directory ($RRQMD_INDEX_DIR)
--backend llama|ort      Inference backend ($RRQMD_INFERENCE_BACKEND)
--ort-ep auto|coreml|cuda|directml|cpu   ORT execution provider ($RRQMD_ORT_EP)
```

---

## Query syntax and expansion

`rqmd query` (and the MCP `query` tool) auto-expand plain-text queries using a local
Qwen3-1.7B model, producing `lex`, `vec`, and `hyde` variants that are fused with the
original query via RRF. `rqmd search` and `rqmd vsearch` do **not** expand — they run
their respective single-mode search only.

**`rqmd query` flags:**

| Flag | Default | Description |
|------|---------|-------------|
| `--intent <text>` | *(none)* | Background context to steer expansion and reranking |
| `-n <num>` | `10` | Number of results to return |
| `-c/--collection <name>` | *(all)* | Scope to a collection (repeatable, OR-matched) |
| `--no-expand` | off | Skip LLM query expansion; search the query text as-is (env: `RRQMD_NO_EXPAND`) |
| `--no-rerank` | off | Skip cross-encoder reranking (expansion still runs) |
| `--full` | off | Return full document bodies instead of snippets |
| `--format <fmt>` | `cli` | Output format — see below |

**`--format` values:**

| Value | Description |
|-------|-------------|
| `cli` | Human-readable, colorized terminal output |
| `json` | Pretty-printed JSON array of results |
| `csv` | `docid,score,file,title,context,line,snippet` — every field is CSV-escaped (quoted, with embedded quotes doubled, whenever it contains a comma, quote, or newline) |
| `md` (alias `markdown`) | Markdown-formatted result list |
| `xml` | XML-wrapped result list |
| `files` | Just the absolute filesystem path of each matching file, one per line — pipeable straight into `xargs` |

An unrecognized `--format` value is a hard argument-parsing error (clap rejects
it before the command runs) — it no longer silently falls back to `cli`.
`rqmd get` and `rqmd multi-get` only support `cli`, `json`, and `files`; passing
`csv`, `md`, or `xml` to those two commands fails with an explicit error rather
than producing malformed output.

**Candidate pool sizing (`-n`):** reranking is the expensive step — each
candidate costs one `LlamaContext` evaluation — so the number of chunks
fetched for reranking is not simply the requested `-n`. rqmd asks for
`limit * 2` candidates, then clamps that to a minimum of 20 and a maximum of
100. Requesting more than 50 results (which would ask for over 100
candidates) triggers a logged warning that the candidate pool has been capped
at 100, rather than silently truncating — since rerank cost scales linearly
with pool depth, this cap keeps `-n` from causing runaway latency.

**Examples:**

```sh
# Plain text — auto-expanded into lex/vec/hyde by the LLM
rqmd query "how does authentication work"

# Explicit expand (equivalent to above)
rqmd query "expand: how does authentication work"

# Typed multi-line query (bypasses LLM; each line is a direct sub-query)
rqmd query $'lex: auth token -oauth\nvec: how does authentication work\nhyde: The auth system uses JWT tokens with a 15-minute TTL...'

# Intent flag — steers expansion and reranking toward web performance
rqmd query --intent "web page load times" "performance"

# Intent inline (query document)
rqmd query $'intent: web page load times\nlex: performance\nvec: how to improve page speed'

# Scope to a specific collection
rqmd query -c docs "deployment pipeline"
```

Full grammar (typed lines, lex phrase/negation operators, MCP `searches` array):
[SYNTAX.md](SYNTAX.md).
